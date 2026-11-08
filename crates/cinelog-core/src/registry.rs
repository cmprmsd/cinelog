//! Recorder registry (§5.1): one 0600 JSON file per recorded tty in the runtime dir,
//! keyed by the *inner* slave the recorder created (`pts-<N>.json`, `tty-<name>.json`).
//!
//! Readers: subshells and `cinelog pause/resume/link` (same uid, to get nonce and uuid),
//! detection (own-recorder check) and the server (live sessions). An entry is valid only
//! if pid, starttime and boot_id match a running process; same-uid readers additionally
//! check that the pid holds the master (cinelog-detect).
//!
//! Writers replace the file atomically (temp file + rename) on every state change, so the
//! plugin can detect changes from the mtime alone (`zstat`, no fork).

use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::str::FromStr;

use cinelog_proto::{Nonce, Scope};
use serde::{Deserialize, Serialize};

/// Registry file format version.
pub const REGISTRY_FORMAT: u32 = 1;

/// Identity of a tty, used as registry key.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum TtyKey {
    /// `/dev/pts/<N>`.
    Pts(u32),
    /// Any other tty, by its device name without `/dev/` (`tty1`, `ttyS0`).
    Tty(String),
}

impl TtyKey {
    /// From a device path such as `/dev/pts/7` or `/dev/tty1`.
    pub fn from_path(path: &Path) -> Option<Self> {
        let s = path.to_str()?;
        if let Some(n) = s.strip_prefix("/dev/pts/") {
            return n.parse().ok().map(Self::Pts);
        }
        let name = s.strip_prefix("/dev/")?;
        (!name.is_empty() && !name.contains("..")).then(|| Self::Tty(name.replace('/', "-")))
    }

    pub fn file_name(&self) -> String {
        match self {
            Self::Pts(n) => format!("pts-{n}.json"),
            Self::Tty(name) => format!("tty-{name}.json"),
        }
    }
}

impl fmt::Display for TtyKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Pts(n) => write!(f, "/dev/pts/{n}"),
            Self::Tty(name) => write!(f, "/dev/{name}"),
        }
    }
}

/// Recorder state as seen by other processes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RecState {
    #[default]
    Live,
    Paused,
    /// Cast closed after an I/O error; passthrough only until `cinelog resume`.
    Degraded,
}

impl FromStr for RecState {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "live" => Ok(Self::Live),
            "paused" => Ok(Self::Paused),
            "degraded" => Ok(Self::Degraded),
            _ => Err(format!("unknown recorder state {s:?}")),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RegistryEntry {
    #[serde(default = "format_version")]
    pub format: u32,
    pub pid: u32,
    /// Recorder starttime (`/proc/<pid>/stat` field 22).
    pub starttime: u64,
    /// `/proc/sys/kernel/random/boot_id` at registration.
    pub boot_id: String,
    pub uuid: String,
    pub nonce: Nonce,
    pub scope: Scope,
    /// Current cast file (changes on rotation).
    pub cast: PathBuf,
    /// Inner slave path (the key of this file).
    pub pts: String,
    /// Host-side tty the recorder runs on.
    #[serde(default)]
    pub host_tty: String,
    /// Shell pid (child of the recorder).
    #[serde(default)]
    pub shell_pid: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_uuid: Option<String>,
    #[serde(default)]
    pub state: RecState,
    /// Reason of the current pause or degradation, for `link`/`doctor`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state_reason: Option<String>,
    #[serde(default)]
    pub part: u32,
    #[serde(default)]
    pub version: String,
}

fn format_version() -> u32 {
    REGISTRY_FORMAT
}

#[derive(Debug, thiserror::Error)]
pub enum RegistryError {
    #[error("registry I/O at {path}: {source}")]
    Io { path: PathBuf, source: std::io::Error },
    #[error("registry entry {path} is malformed: {source}")]
    Malformed { path: PathBuf, source: serde_json::Error },
    #[error("registry entry {0} is not owned by us or is accessible to others")]
    Insecure(PathBuf),
}

/// Liveness probe used to validate entries: returns the starttime of a running pid.
pub trait ProcessProbe {
    fn starttime(&self, pid: u32) -> Option<u64>;
    fn boot_id(&self) -> Option<String>;
}

impl RegistryEntry {
    /// pid + starttime + boot_id still describe a running process.
    pub fn is_alive(&self, probe: &dyn ProcessProbe) -> bool {
        probe.boot_id().as_deref() == Some(self.boot_id.as_str()) && probe.starttime(self.pid) == Some(self.starttime)
    }
}

/// The registry directory of one scope (`Paths::runtime_dir`).
#[derive(Debug, Clone)]
pub struct Registry {
    dir: PathBuf,
}

impl Registry {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn path_for(&self, key: &TtyKey) -> PathBuf {
        self.dir.join(key.file_name())
    }

    /// Creates the directory (0700) if needed.
    pub fn ensure_dir(&self) -> Result<(), RegistryError> {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&self.dir)
            .map_err(|source| RegistryError::Io {
                path: self.dir.clone(),
                source,
            })
    }

    /// Atomically writes the entry for `key` with mode 0600.
    pub fn write(&self, key: &TtyKey, entry: &RegistryEntry) -> Result<PathBuf, RegistryError> {
        let path = self.path_for(key);
        let tmp = self
            .dir
            .join(format!(".{}.{}.tmp", key.file_name(), std::process::id()));
        let io = |source| RegistryError::Io {
            path: path.clone(),
            source,
        };
        let json = serde_json::to_vec(entry).expect("registry entries serialize");
        let mut f = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)
            .map_err(io)?;
        f.write_all(&json).map_err(io)?;
        drop(f);
        fs::rename(&tmp, &path).map_err(io)?;
        Ok(path)
    }

    /// Reads the entry for `key`. `Ok(None)` if there is none. The file must be owned by
    /// the euid and not accessible to group or others.
    pub fn read(&self, key: &TtyKey) -> Result<Option<RegistryEntry>, RegistryError> {
        let path = self.path_for(key);
        let meta = match fs::metadata(&path) {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(source) => return Err(RegistryError::Io { path, source }),
        };
        use std::os::unix::fs::MetadataExt;
        if meta.uid() != nix::unistd::geteuid().as_raw() || meta.permissions().mode() & 0o077 != 0 {
            return Err(RegistryError::Insecure(path));
        }
        let bytes = fs::read(&path).map_err(|source| RegistryError::Io {
            path: path.clone(),
            source,
        })?;
        serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|source| RegistryError::Malformed { path, source })
    }

    /// Removes the entry if it still belongs to `pid` (a newer recorder may own the key).
    pub fn remove_if_owner(&self, key: &TtyKey, pid: u32) -> Result<(), RegistryError> {
        match self.read(key) {
            Ok(Some(e)) if e.pid == pid => {
                let path = self.path_for(key);
                fs::remove_file(&path).map_err(|source| RegistryError::Io { path, source })
            }
            Ok(_) => Ok(()),
            Err(e) => Err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Probe(u64);
    impl ProcessProbe for Probe {
        fn starttime(&self, pid: u32) -> Option<u64> {
            (pid == 42).then_some(self.0)
        }
        fn boot_id(&self) -> Option<String> {
            Some("boot".into())
        }
    }

    fn entry() -> RegistryEntry {
        RegistryEntry {
            format: REGISTRY_FORMAT,
            pid: 42,
            starttime: 7,
            boot_id: "boot".into(),
            uuid: "u".into(),
            nonce: Nonce::from_bytes([1; 16]),
            scope: Scope::Personal,
            cast: "/l/a.cast".into(),
            pts: "/dev/pts/3".into(),
            host_tty: "/dev/pts/1".into(),
            shell_pid: 43,
            parent_uuid: None,
            state: RecState::Live,
            state_reason: None,
            part: 1,
            version: crate::VERSION.into(),
        }
    }

    #[test]
    fn keys() {
        assert_eq!(TtyKey::from_path(Path::new("/dev/pts/7")), Some(TtyKey::Pts(7)));
        assert_eq!(
            TtyKey::from_path(Path::new("/dev/tty1")).unwrap().file_name(),
            "tty-tty1.json"
        );
        assert_eq!(TtyKey::from_path(Path::new("/etc/passwd")), None);
        assert_eq!(TtyKey::from_path(Path::new("/dev/../x")), None);
    }

    #[test]
    fn write_read_remove() {
        let dir = tempfile::tempdir().unwrap();
        let reg = Registry::new(dir.path().join("cinelog"));
        reg.ensure_dir().unwrap();
        let key = TtyKey::Pts(3);
        let path = reg.write(&key, &entry()).unwrap();
        assert_eq!(fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        let raw = fs::read_to_string(&path).unwrap();
        assert!(raw.contains(&"01".repeat(16)), "nonce stored as hex");
        assert_eq!(reg.read(&key).unwrap(), Some(entry()));
        reg.remove_if_owner(&key, 99).unwrap();
        assert!(path.exists());
        reg.remove_if_owner(&key, 42).unwrap();
        assert_eq!(reg.read(&key).unwrap(), None);
    }

    #[test]
    fn insecure_mode_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let reg = Registry::new(dir.path());
        let path = reg.write(&TtyKey::Pts(1), &entry()).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(matches!(reg.read(&TtyKey::Pts(1)), Err(RegistryError::Insecure(_))));
    }

    #[test]
    fn liveness() {
        assert!(entry().is_alive(&Probe(7)));
        assert!(!entry().is_alive(&Probe(8)));
    }
}
