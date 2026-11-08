//! `/proc` helpers: liveness, comm, ancestry, and finding a sandbox's processes by the
//! environment marker every sandboxed process inherits.

use std::fs;
use std::time::Duration;

use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;

/// Fields of `/proc/<pid>/stat` after the `(comm)` part.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stat {
    pub pid: u32,
    pub comm: String,
    pub state: char,
    pub ppid: u32,
    pub pgrp: u32,
    pub starttime: u64,
}

pub fn parse_stat(pid: u32, s: &str) -> Option<Stat> {
    let open = s.find('(')?;
    let close = s.rfind(')')?;
    let comm = s.get(open + 1..close)?.to_owned();
    let rest: Vec<&str> = s.get(close + 2..)?.split_whitespace().collect();
    Some(Stat {
        pid,
        comm,
        state: rest.first()?.chars().next()?,
        ppid: rest.get(1)?.parse().ok()?,
        pgrp: rest.get(2)?.parse().ok()?,
        starttime: rest.get(19)?.parse().ok()?,
    })
}

pub fn stat(pid: u32) -> Option<Stat> {
    parse_stat(pid, &fs::read_to_string(format!("/proc/{pid}/stat")).ok()?)
}

/// Running and not a zombie.
pub fn alive(pid: u32) -> bool {
    stat(pid).is_some_and(|s| s.state != 'Z' && s.state != 'X')
}

pub fn comm(pid: u32) -> Option<String> {
    stat(pid).map(|s| s.comm)
}

/// `/proc/<pid>/cmdline` split at NUL.
pub fn cmdline(pid: u32) -> Option<Vec<String>> {
    let raw = fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    Some(
        raw.split(|b| *b == 0)
            .filter(|a| !a.is_empty())
            .map(|a| String::from_utf8_lossy(a).into_owned())
            .collect(),
    )
}

pub fn all_pids() -> Vec<u32> {
    let Ok(dir) = fs::read_dir("/proc") else {
        return Vec::new();
    };
    dir.filter_map(|e| e.ok()?.file_name().to_str()?.parse().ok()).collect()
}

/// Every process (that we may read) whose environment contains `key=value`.
pub fn with_env(key: &str, value: &str) -> Vec<u32> {
    let needle = format!("{key}={value}").into_bytes();
    let me = std::process::id();
    all_pids()
        .into_iter()
        .filter(|&pid| pid != me)
        .filter(|pid| {
            fs::read(format!("/proc/{pid}/environ"))
                .map(|env| env.split(|b| *b == 0).any(|kv| kv == needle.as_slice()))
                .unwrap_or(false)
        })
        .filter(|&pid| alive(pid))
        .collect()
}

pub fn children(pid: u32) -> Vec<u32> {
    all_pids()
        .into_iter()
        .filter(|&p| stat(p).is_some_and(|s| s.ppid == pid))
        .collect()
}

/// All descendants (breadth first).
pub fn descendants(pid: u32) -> Vec<u32> {
    let stats: Vec<Stat> = all_pids().into_iter().filter_map(stat).collect();
    let mut out = Vec::new();
    let mut frontier = vec![pid];
    while let Some(p) = frontier.pop() {
        for s in stats.iter().filter(|s| s.ppid == p) {
            out.push(s.pid);
            frontier.push(s.pid);
        }
    }
    out
}

pub fn signal(pid: u32, sig: Signal) -> bool {
    i32::try_from(pid).is_ok_and(|p| kill(Pid::from_raw(p), sig).is_ok())
}

/// HUP + TERM, then KILL whatever is still alive after `grace`.
pub fn terminate(pids: &[u32], grace: Duration) {
    for &p in pids {
        signal(p, Signal::SIGHUP);
        signal(p, Signal::SIGTERM);
    }
    let deadline = std::time::Instant::now() + grace;
    while pids.iter().any(|&p| alive(p)) && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    for &p in pids.iter().filter(|&&p| alive(p)) {
        signal(p, Signal::SIGKILL);
    }
}

/// Waits until `pid` is gone (or a zombie nobody reaps).
pub fn wait_gone(pid: u32, timeout: Duration) -> bool {
    crate::util::wait_until("process exit", timeout, || (!alive(pid)).then_some(())).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stat_with_spaces_and_parens_in_comm() {
        let line = "4242 (tmux: server) S 1 4242 4242 0 -1 4194560 1 0 0 0 0 0 0 0 20 0 1 0 987654 0 0";
        let s = parse_stat(4242, line).unwrap();
        assert_eq!(s.comm, "tmux: server");
        assert_eq!((s.state, s.ppid, s.pgrp, s.starttime), ('S', 1, 4242, 987654));
        let s = parse_stat(7, "7 (a) b)) R 2 7 7 0 -1 0 0 0 0 0 0 0 0 0 20 0 1 0 55 0").unwrap();
        assert_eq!(s.comm, "a) b)");
        assert_eq!(s.starttime, 55);
    }

    #[test]
    fn own_process_is_visible() {
        let me = std::process::id();
        assert!(alive(me));
        assert!(stat(me).unwrap().starttime > 0);
        assert!(!cmdline(me).unwrap().is_empty());
    }

    #[test]
    fn env_marker_finds_children_and_terminate_kills_them() {
        let id = crate::util::unique_id();
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .env("CLIT_SANDBOX", &id)
            .spawn()
            .unwrap();
        let pid = child.id();
        let found = crate::util::wait_until("child", Duration::from_secs(5), || {
            let v = with_env("CLIT_SANDBOX", &id);
            (!v.is_empty()).then_some(v)
        })
        .unwrap();
        assert_eq!(found, vec![pid]);
        assert!(descendants(me()).contains(&pid));
        terminate(&found, Duration::from_millis(500));
        child.wait().unwrap();
        assert!(with_env("CLIT_SANDBOX", &id).is_empty());
    }

    fn me() -> u32 {
        std::process::id()
    }
}
