//! `server.json` in the runtime dir (§10.4): written by `cinelog serve`, read by `link`,
//! `search`, the MOTD and `doctor`. Also the `auto` port rule of §7.1.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use cinelog_proto::Scope;
use serde::{Deserialize, Serialize};

pub const SERVER_JSON: &str = "server.json";

/// First port of the `auto` rule; TEAM always uses it.
pub const BASE_PORT: u16 = 10000;
/// Range for personal ports whose `10000 + uid` would overflow.
const HASHED_PORTS: std::ops::Range<u32> = 40000..60000;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerInfo {
    pub pid: u32,
    /// Base URL without a trailing slash, e.g. `https://box.lan:11000`.
    pub url: String,
    pub port: u16,
    /// SHA-256 fingerprint of the TLS certificate.
    #[serde(default)]
    pub fingerprint: String,
    #[serde(default)]
    pub version: String,
}

impl ServerInfo {
    pub fn path(runtime_dir: &Path) -> PathBuf {
        runtime_dir.join(SERVER_JSON)
    }

    /// Reads `server.json`; `None` if it is missing, malformed or its pid is gone.
    pub fn read_live(runtime_dir: &Path) -> Option<Self> {
        let info: Self = serde_json::from_slice(&fs::read(Self::path(runtime_dir)).ok()?).ok()?;
        Path::new(&format!("/proc/{}", info.pid)).exists().then_some(info)
    }

    /// Atomic 0600 write.
    pub fn write(&self, runtime_dir: &Path) -> std::io::Result<()> {
        let path = Self::path(runtime_dir);
        let tmp = runtime_dir.join(format!(".{SERVER_JSON}.{}.tmp", std::process::id()));
        let mut f = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)?;
        f.write_all(&serde_json::to_vec(self).expect("server info serializes"))?;
        drop(f);
        fs::rename(tmp, path)
    }

    /// Web URL of one session.
    pub fn session_url(&self, uuid: &str) -> String {
        format!("{}/s/{uuid}", self.url.trim_end_matches('/'))
    }
}

/// The `auto` port (§7.1): TEAM 10000; PERSONAL `10000 + uid` while that fits, otherwise a
/// deterministic hash of the uid into 40000–59999.
pub fn auto_port(scope: Scope, uid: u32) -> u16 {
    match scope {
        Scope::Team => BASE_PORT,
        Scope::Personal => u32::from(BASE_PORT)
            .checked_add(uid)
            .and_then(|p| u16::try_from(p).ok())
            .unwrap_or_else(|| {
                // Fibonacci hashing spreads neighbouring uids across the range.
                let h = uid.wrapping_mul(0x9e37_79b9) % (HASHED_PORTS.end - HASHED_PORTS.start);
                u16::try_from(HASHED_PORTS.start + h).expect("range fits in u16")
            }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ports() {
        assert_eq!(auto_port(Scope::Team, 1000), 10000);
        assert_eq!(auto_port(Scope::Personal, 1000), 11000);
        assert_eq!(auto_port(Scope::Personal, 55535), 65535);
        let big = auto_port(Scope::Personal, 55536);
        assert!((40000..60000).contains(&big));
        assert_eq!(big, auto_port(Scope::Personal, 55536));
        assert_ne!(auto_port(Scope::Personal, 100_000), auto_port(Scope::Personal, 100_001));
        assert!((40000..60000).contains(&auto_port(Scope::Personal, u32::MAX)));
    }

    #[test]
    fn server_json_roundtrip_and_liveness() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(ServerInfo::read_live(dir.path()), None);
        let info = ServerInfo {
            pid: std::process::id(),
            url: "https://box:11000/".into(),
            port: 11000,
            fingerprint: "ab".into(),
            version: crate::VERSION.into(),
        };
        info.write(dir.path()).unwrap();
        assert_eq!(ServerInfo::read_live(dir.path()), Some(info.clone()));
        assert_eq!(info.session_url("u-1"), "https://box:11000/s/u-1");
        let dead = ServerInfo {
            pid: u32::MAX - 1,
            ..info
        };
        dead.write(dir.path()).unwrap();
        assert_eq!(ServerInfo::read_live(dir.path()), None);
    }
}
