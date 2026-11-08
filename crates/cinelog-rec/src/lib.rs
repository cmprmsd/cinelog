//! `cinelog rec`: the PTY recorder (plan §5.1, §5.3, §5.5, §5.7, §5.8).
//!
//! Frozen entry points used by the CLI: [`RecRequest`], [`run`], [`exec_unrecorded`].
//! [`run`] never returns: it either records until the shell exits and then exits the
//! process with the shell's status, or, on any failure before the fork, execs the shell
//! unrecorded through [`exec_unrecorded`].
//!
//! Layout: `setup` (preflight and start decision), `recorder` (the three threads),
//! `control` (recording decisions under one lock), `pause`, `input`, `screen`, `seq`,
//! `clockwatch`, `writer` (cast parts, rotation, degraded mode), `events` (bounded drop
//! policy), `reg` (registry entry), `mirror` (cmdline), `tmux` (pane context), `pty`.

use std::ffi::{CString, OsString};
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;

use cinelog_core::logging::RecLog;
use cinelog_proto::env;

mod clockwatch;
mod control;
mod events;
mod input;
mod mirror;
mod pause;
mod pty;
mod recorder;
mod reg;
mod screen;
mod seq;
mod setup;
mod tmux;
mod writer;

#[cfg(test)]
mod tests;

/// What `cinelog rec -- <shell> [args…]` was asked to run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecRequest {
    /// Absolute path of the shell to run inside the recording.
    pub shell: PathBuf,
    /// Arguments after the shell path (normally empty: the inner shell is non-login).
    pub args: Vec<OsString>,
}

/// Records `req` (or falls back to an unrecorded shell). Never returns.
pub fn run(req: RecRequest) -> ! {
    let (launch, cast) = match setup::prepare(&req) {
        Ok(prepared) => prepared,
        Err(f) => exec_unrecorded(&req, &f.reason, f.log.as_ref()),
    };
    // Detection identifies recorders by comm, whatever the binary is called.
    let _ = nix::sys::prctl::set_name(c"cinelog");
    let log = launch.log.clone();
    match recorder::record(launch) {
        Ok(status) => std::process::exit(status),
        Err(recorder::SpawnError(reason)) => {
            let _ = std::fs::remove_file(&cast);
            exec_unrecorded(&req, &reason, log.as_ref())
        }
    }
}

/// Execs the requested shell unrecorded (§5.1 "never fails the shell").
///
/// Sets `CINELOG_FAILED=<reason>`, `CINELOG_FAILED_TTY=<tty of fd 0>` and
/// `CINELOG_FAILED_PID=<own pid>` (which becomes the shell's `$$`), removes every other
/// `CINELOG_*` variable, appends the reason to `rec.log` if given, then `execv`s the shell.
/// If that fails it tries `/bin/zsh` and `/bin/sh`, and exits 127 as a last resort.
pub fn exec_unrecorded(req: &RecRequest, reason: &str, log: Option<&RecLog>) -> ! {
    if let Some(log) = log {
        let _ = log.append(&format!("not recording: {reason}"));
    }
    let tty = nix::unistd::ttyname(std::io::stdin())
        .map(|p| p.into_os_string())
        .unwrap_or_default();
    let stale: Vec<OsString> = std::env::vars_os()
        .map(|(k, _)| k)
        .filter(|k| k.as_bytes().starts_with(b"CINELOG_"))
        .collect();
    // SAFETY: single-threaded at this point (called before any recorder thread exists).
    unsafe {
        for k in stale {
            std::env::remove_var(k);
        }
        std::env::set_var(env::FAILED, reason.replace('\n', " "));
        std::env::set_var(env::FAILED_TTY, tty);
        std::env::set_var(env::FAILED_PID, std::process::id().to_string());
    }
    let args: Vec<CString> = req
        .args
        .iter()
        .filter_map(|a| CString::new(a.as_bytes()).ok())
        .collect();
    for shell in [req.shell.as_os_str(), "/bin/zsh".as_ref(), "/bin/sh".as_ref()] {
        let Ok(path) = CString::new(shell.as_bytes()) else {
            continue;
        };
        let argv0 = CString::new(
            std::path::Path::new(shell)
                .file_name()
                .map(|n| n.as_bytes())
                .unwrap_or(b"sh"),
        )
        .expect("file names contain no NUL");
        let argv: Vec<&CString> = std::iter::once(&argv0).chain(args.iter()).collect();
        let _ = nix::unistd::execv(&path, &argv);
    }
    std::process::exit(127)
}
