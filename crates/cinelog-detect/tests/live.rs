//! Detection against the live `/proc`: a PTY held by the test itself, child process groups,
//! an isolated tmux server (`tmux -L cinelog-detect-<pid>`) and `script`. The tmux and
//! script tests are skipped when the tool is not installed.

use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use cinelog_core::config::HidepidFallback;
use cinelog_detect::{
    CheckRequest, Detector, Foreground, FoundVia, NestedClaim, NoRegistry, ProcDetector, ProcFs, RealProcFs, Reason,
    StartDecision, TtyIdentity, tty_of_fd,
};
use cinelog_proto::Scope;
use nix::fcntl::OFlag;
use nix::pty::{grantpt, posix_openpt, ptsname_r, unlockpt};

fn detector() -> ProcDetector<RealProcFs, NoRegistry> {
    ProcDetector::new(RealProcFs::new(), NoRegistry)
}

fn euid() -> u32 {
    nix::unistd::geteuid().as_raw()
}

fn have(tool: &str) -> bool {
    Command::new(tool)
        .arg("-V")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok()
}

fn wait_for<T>(what: &str, mut f: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(v) = f() {
            return v;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

struct KillOnDrop(Child);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn own_pty_is_found_on_ourselves() {
    // O_CLOEXEC on both ends: children spawned by parallel tests must not inherit them.
    let master = posix_openpt(OFlag::O_RDWR | OFlag::O_NOCTTY | OFlag::O_CLOEXEC).unwrap();
    grantpt(&master).unwrap();
    unlockpt(&master).unwrap();
    let slave_path = ptsname_r(&master).unwrap();
    let slave = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NOCTTY | libc::O_CLOEXEC)
        .open(&slave_path)
        .unwrap();
    let TtyIdentity::Pts(n) = tty_of_fd(slave.as_raw_fd()) else {
        panic!("slave is not a pts")
    };
    let det = detector();
    let me = std::process::id();
    let h = det.holder(n, me).expect("holder");
    assert_eq!((h.pid, h.via, h.uid), (me, FoundVia::Ancestor, euid()));
    assert!(det.holds_master(me, n).unwrap());

    // The master was opened through /dev/ptmx (devtmpfs) but belongs to the devpts at
    // /dev/pts, like the slave.
    use std::os::unix::fs::MetadataExt;
    let pts_dev = std::fs::metadata("/dev/pts").unwrap().dev();
    let proc = RealProcFs::new();
    let fd = |f: i32| u32::try_from(f).unwrap();
    assert_eq!(proc.devpts_dev(me, fd(master.as_raw_fd())).unwrap(), Some(pts_dev));
    assert_eq!(proc.devpts_dev(me, fd(slave.as_raw_fd())).unwrap(), Some(pts_dev));
    let devnull = std::fs::File::open("/dev/null").unwrap();
    assert_eq!(proc.devpts_dev(me, fd(devnull.as_raw_fd())).unwrap(), None);

    // From a child that holds nothing, the walk reaches us as its parent.
    let child = KillOnDrop(Command::new("sleep").arg("30").spawn().unwrap());
    let h = det.holder(n, child.0.id()).expect("holder via parent");
    assert_eq!((h.pid, h.via), (me, FoundVia::Ancestor));

    let req = CheckRequest {
        tty: TtyIdentity::Pts(n),
        euid: euid(),
        start_pid: child.0.id(),
        hidepid_fallback: HidepidFallback::Start,
    };
    assert!(matches!(det.check_start(&req), StartDecision::Start { .. }));
    drop((master, slave));
    assert!(det.holder(n, me).is_none_or(|h| h.pid != me));
}

#[test]
fn foreground_job_and_relay() {
    let det = detector();
    let shell_pgrp = nix::unistd::getpgrp().as_raw() as u32;
    let child = KillOnDrop(Command::new("sleep").arg("30").process_group(0).spawn().unwrap());
    let pid = child.0.id();
    let mux = vec!["sleep".to_owned()];
    assert_eq!(
        det.foreground(pid, shell_pgrp, &mux),
        Foreground::Multiplexer {
            pid,
            comm: "sleep".into(),
            pgrp: pid
        }
    );
    assert_eq!(
        det.foreground(pid, shell_pgrp, &["tmux: client".to_owned()]),
        Foreground::Job {
            pgrp: pid,
            leader_comm: "sleep".into()
        }
    );
    assert!(det.relay_in_foreground(pid, &mux));
    assert!(!det.relay_in_foreground(shell_pgrp, &mux));
    let st = det.starttime(pid).unwrap();
    assert!(!det.corroborate_nested(pid, shell_pgrp, NestedClaim { pid, starttime: st }));
    assert_eq!(det.boot_id(), RealProcFs::new().boot_id().ok());
}

/// An isolated tmux server; its socket lives in a private `TMUX_TMPDIR`.
struct Tmux {
    name: String,
    dir: tempfile::TempDir,
}

impl Tmux {
    fn new() -> Self {
        Self {
            name: format!("cinelog-detect-{}", std::process::id()),
            dir: tempfile::tempdir().unwrap(),
        }
    }

    fn cmd(&self) -> Command {
        let mut c = Command::new("tmux");
        c.args(["-L", &self.name, "-f", "/dev/null"])
            .env("TMUX_TMPDIR", self.dir.path())
            .env_remove("TMUX");
        c
    }
}

impl Drop for Tmux {
    fn drop(&mut self) {
        let _ = self.cmd().arg("kill-server").stderr(Stdio::null()).status();
    }
}

#[test]
fn tmux_pane_is_held_by_the_server() {
    if !have("tmux") {
        eprintln!("tmux not installed, skipped");
        return;
    }
    let tmux = Tmux::new();
    let out = tmux
        .cmd()
        .args([
            "new-session",
            "-d",
            "-x",
            "80",
            "-y",
            "24",
            "-P",
            "-F",
            "#{pid} #{pane_pid} #{pane_tty}",
            "sleep 60",
        ])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let line = String::from_utf8(out.stdout).unwrap();
    let f: Vec<&str> = line.split_whitespace().collect();
    let (server, pane): (u32, u32) = (f[0].parse().unwrap(), f[1].parse().unwrap());
    let n: u32 = f[2].strip_prefix("/dev/pts/").unwrap().parse().unwrap();

    let det = detector();
    let h = det.holder(n, pane).expect("tmux server holds the pane master");
    assert_eq!(
        (h.pid, h.comm.as_str(), h.via),
        (server, "tmux: server", FoundVia::Ancestor)
    );
    let req = CheckRequest {
        tty: TtyIdentity::Pts(n),
        euid: euid(),
        start_pid: pane,
        hidepid_fallback: HidepidFallback::Start,
    };
    assert_eq!(
        det.check_start(&req),
        StartDecision::Start {
            scope: Scope::for_euid(euid()),
            reason: Reason::TmuxServer
        }
    );
    // A process outside the server's tree finds it by the full scan.
    let h = det.holder(n, std::process::id()).expect("scan");
    assert_eq!((h.pid, h.via), (server, FoundVia::Scan));
}

#[test]
fn script_holder() {
    if !Command::new("script")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
    {
        eprintln!("script not installed, skipped");
        return;
    }
    let script = KillOnDrop(
        Command::new("script")
            .args(["-q", "-f", "-c", "exec sleep 60", "/dev/null"])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let proc = RealProcFs::new();
    let sid = script.0.id();
    let (child, n) = wait_for("script's child on its pts", || {
        proc.pids().ok()?.into_iter().find_map(|p| {
            let st = proc.stat(p).ok()?;
            (st.ppid == sid && st.comm == "sleep").then_some(())?;
            let fd0 = std::fs::read_link(format!("/proc/{p}/fd/0")).ok()?;
            let n = fd0.to_str()?.strip_prefix("/dev/pts/")?.parse::<u32>().ok()?;
            Some((p, n))
        })
    });
    let det = detector();
    let req = CheckRequest {
        tty: TtyIdentity::Pts(n),
        euid: euid(),
        start_pid: child,
        hidepid_fallback: HidepidFallback::Start,
    };
    assert_eq!(
        det.check_start(&req),
        StartDecision::Start {
            scope: Scope::for_euid(euid()),
            reason: Reason::Script
        }
    );
    assert_eq!(det.holder(n, child).map(|h| h.pid), Some(sid));
}
