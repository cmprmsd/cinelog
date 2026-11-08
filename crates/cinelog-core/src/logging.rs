//! cinelog's own logs: `rec.log` is append-only and capped (§6.1).

use std::fs::OpenOptions;
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

/// Size above which `rec.log` is moved to `rec.log.1` before the next append.
pub const REC_LOG_CAP: u64 = 1024 * 1024;

#[derive(Debug, Clone)]
pub struct RecLog {
    path: PathBuf,
    cap: u64,
}

impl RecLog {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            cap: REC_LOG_CAP,
        }
    }

    pub fn with_cap(path: impl Into<PathBuf>, cap: u64) -> Self {
        Self { path: path.into(), cap }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Appends one line `<rfc3339> pid=<pid> <msg>`. Never panics; errors are returned so
    /// the caller can ignore them (logging must never break a shell). The parent directory
    /// is created (0700) if needed.
    pub fn append(&self, msg: &str) -> std::io::Result<()> {
        if let Some(parent) = self.path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::DirBuilder::new().recursive(true).mode(0o700).create(parent)?;
        }
        if std::fs::metadata(&self.path).is_ok_and(|m| m.len() >= self.cap) {
            let mut old = self.path.clone().into_os_string();
            old.push(".1");
            std::fs::rename(&self.path, old)?;
        }
        let mut f = OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(&self.path)?;
        let line = format!(
            "{} pid={} {}\n",
            jiff::Timestamp::now(),
            std::process::id(),
            msg.replace('\n', " ")
        );
        f.write_all(line.as_bytes())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn appends_and_rotates() {
        let dir = tempfile::tempdir().unwrap();
        let log = RecLog::with_cap(dir.path().join("state/rec.log"), 64);
        log.append("first\nline").unwrap();
        let s = std::fs::read_to_string(log.path()).unwrap();
        assert!(s.ends_with("first line\n"), "{s}");
        log.append(&"x".repeat(80)).unwrap();
        log.append("after").unwrap();
        assert!(dir.path().join("state/rec.log.1").exists());
        assert!(std::fs::read_to_string(log.path()).unwrap().contains("after"));
    }
}
