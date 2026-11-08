//! Where `cinelog serve` logs (§6.1): the terminal when started from one, journald under
//! systemd, else `<state>/serve.log` rotated at 10 MB with 3 files kept. `CINELOG_LOG`
//! takes an `EnvFilter` directive (default `info`).

use std::fs::{self, File, OpenOptions};
use std::io::{self, IsTerminal, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use tracing_subscriber::EnvFilter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

pub const SERVE_LOG: &str = "serve.log";
pub const ROTATE_BYTES: u64 = 10 * 1000 * 1000;
/// `serve.log` plus `.1` and `.2`.
pub const FILES_KEPT: u32 = 3;

/// An append-only log file that rotates itself (`serve.log` → `serve.log.1` → …).
#[derive(Debug)]
pub struct RotatingFile {
    path: PathBuf,
    file: File,
    size: u64,
    cap: u64,
    keep: u32,
}

impl RotatingFile {
    pub fn open(path: &Path, cap: u64, keep: u32) -> io::Result<Self> {
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
        }
        let file = Self::open_file(path)?;
        let size = file.metadata()?.len();
        Ok(Self {
            path: path.to_path_buf(),
            file,
            size,
            cap,
            keep: keep.max(1),
        })
    }

    fn open_file(path: &Path) -> io::Result<File> {
        OpenOptions::new().create(true).append(true).mode(0o600).open(path)
    }

    fn numbered(&self, n: u32) -> PathBuf {
        let mut s = self.path.clone().into_os_string();
        s.push(format!(".{n}"));
        PathBuf::from(s)
    }

    fn rotate(&mut self) -> io::Result<()> {
        let last = self.keep - 1;
        if last == 0 {
            self.file.set_len(0)?;
        } else {
            let _ = fs::remove_file(self.numbered(last));
            for n in (1..last).rev() {
                let _ = fs::rename(self.numbered(n), self.numbered(n + 1));
            }
            fs::rename(&self.path, self.numbered(1))?;
            self.file = Self::open_file(&self.path)?;
        }
        self.size = 0;
        Ok(())
    }
}

impl Write for RotatingFile {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.size > 0 && self.size + buf.len() as u64 > self.cap {
            self.rotate()?;
        }
        let n = self.file.write(buf)?;
        self.size += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

fn filter() -> EnvFilter {
    EnvFilter::try_from_env("CINELOG_LOG").unwrap_or_else(|_| EnvFilter::new("info"))
}

/// Installs the global subscriber. Returns where the log goes, for the first log line.
pub fn init(state_dir: &Path) -> String {
    let registry = tracing_subscriber::registry().with(filter());
    if io::stderr().is_terminal() {
        let layer = tracing_subscriber::fmt::layer()
            .with_writer(io::stderr)
            .with_target(false);
        let _ = registry.with(layer).try_init();
        return "stderr".into();
    }
    if std::env::var_os("JOURNAL_STREAM").is_some()
        && let Ok(layer) = tracing_journald::layer()
    {
        let _ = registry.with(layer.with_syslog_identifier("cinelog".into())).try_init();
        return "journald".into();
    }
    let path = state_dir.join(SERVE_LOG);
    match RotatingFile::open(&path, ROTATE_BYTES, FILES_KEPT) {
        Ok(f) => {
            let layer = tracing_subscriber::fmt::layer()
                .with_writer(Mutex::new(f))
                .with_ansi(false)
                .with_target(false);
            let _ = registry.with(layer).try_init();
            path.display().to_string()
        }
        Err(e) => {
            let layer = tracing_subscriber::fmt::layer()
                .with_writer(io::stderr)
                .with_ansi(false);
            let _ = registry.with(layer).try_init();
            format!("stderr ({}: {e})", path.display())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rotates_and_keeps_three_files() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("st/serve.log");
        let mut f = RotatingFile::open(&p, 10, 3).unwrap();
        for i in 0..5 {
            f.write_all(format!("line{i}...\n").as_bytes()).unwrap();
        }
        assert_eq!(fs::read_to_string(&p).unwrap(), "line4...\n");
        assert_eq!(
            fs::read_to_string(d.path().join("st/serve.log.1")).unwrap(),
            "line3...\n"
        );
        assert_eq!(
            fs::read_to_string(d.path().join("st/serve.log.2")).unwrap(),
            "line2...\n"
        );
        assert!(!d.path().join("st/serve.log.3").exists());
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(fs::metadata(&p).unwrap().permissions().mode() & 0o777, 0o600);
        // Reopening continues the current file.
        let mut f = RotatingFile::open(&p, 100, 3).unwrap();
        f.write_all(b"more\n").unwrap();
        assert_eq!(fs::read_to_string(&p).unwrap(), "line4...\nmore\n");
    }
}
