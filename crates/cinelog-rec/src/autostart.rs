//! Server autostart (§5.1, §10.4): right after the fork a PERSONAL recorder runs
//! `<self_exe> serve --ensure` in a new session. That returns at once while `serve.lock` is
//! held; otherwise it starts the user unit or a detached server. TEAM recorders never start
//! a server (the system unit owns it).

use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// The binary to run: the recorder's own executable, or `/proc/self/exe` once it has been
/// replaced on disk.
pub(crate) fn self_exe() -> PathBuf {
    std::env::current_exe()
        .ok()
        .filter(|p| p.is_file())
        .unwrap_or_else(|| PathBuf::from("/proc/self/exe"))
}

/// Starts `exe serve --ensure` detached from the terminal and reaps it on a thread, so the
/// recorder (which waits only for its shell) never collects a zombie. Errors are returned
/// for the log; recording goes on regardless.
pub(crate) fn ensure_server(exe: &Path) -> io::Result<()> {
    let mut cmd = Command::new(exe);
    cmd.args(["serve", "--ensure"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .current_dir("/");
    // SAFETY: runs between fork and exec; only async-signal-safe syscalls, no allocation.
    unsafe {
        use std::os::unix::process::CommandExt;
        cmd.pre_exec(|| {
            if libc::setsid() < 0 {
                return Err(io::Error::last_os_error());
            }
            // The PTY master and the nonce pipe must not leak into the server:
            // close_range(3, ~0, CLOSE_RANGE_CLOEXEC), best effort.
            libc::syscall(libc::SYS_close_range, 3u32, u32::MAX, 1u32 << 2);
            Ok(())
        });
    }
    let mut child = cmd.spawn()?;
    std::thread::Builder::new()
        .name("cinelog-autostart".into())
        .spawn(move || {
            let _ = child.wait();
        })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;
    use std::time::{Duration, Instant};

    use super::*;

    #[test]
    fn runs_serve_ensure_in_its_own_session() {
        let d = tempfile::tempdir().unwrap();
        let out = d.path().join("out");
        let exe = d.path().join("fake-cinelog");
        std::fs::write(
            &exe,
            format!(
                "#!/bin/sh\necho \"$* $(ps -o sid= -p $$) $$\" > {}.tmp && mv {0}.tmp {0}\n",
                out.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        // A parallel test may still hold the script open for writing (ETXTBSY).
        loop {
            match ensure_server(&exe) {
                Err(e) if e.kind() == io::ErrorKind::ExecutableFileBusy && Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(5));
                }
                r => break r.unwrap(),
            }
        }
        while !out.exists() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        let line = std::fs::read_to_string(&out).unwrap();
        let f: Vec<&str> = line.split_whitespace().collect();
        assert_eq!(&f[..2], ["serve", "--ensure"], "{line}");
        assert_eq!(f[2], f[3], "session leader: {line}");
    }

    #[test]
    fn self_exe_is_a_file() {
        assert!(self_exe().exists());
    }
}
