//! The old plugin's viewer: every recorded shell started `nohup python3
//! "$HOME/.zim/modules/cinelog/http_server.py"` when nothing listened on its port.
//! `cinelog install` stops it and checks that the persisted port is free (§10.6, M2 accept #5).

use std::ffi::OsString;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;

/// TCP state `LISTEN` in `/proc/net/tcp`.
const TCP_LISTEN: &str = "0A";

/// Whether `argv` runs the old viewer of the user whose home is `home`: a python
/// interpreter with a script `<home>/…/cinelog/http_server.py`.
pub fn is_old_server(argv: &[OsString], home: &Path) -> bool {
    let Some(exe) = argv.first() else { return false };
    let python = Path::new(exe)
        .file_name()
        .is_some_and(|n| n.as_bytes().starts_with(b"python"));
    python
        && argv[1..].iter().map(Path::new).any(|a| {
            a.is_absolute()
                && a.starts_with(home)
                && a.file_name().is_some_and(|n| n == "http_server.py")
                && a.parent().and_then(Path::file_name).is_some_and(|d| d == "cinelog")
        })
}

fn real_uid(status: &str) -> Option<u32> {
    status
        .lines()
        .find_map(|l| l.strip_prefix("Uid:"))
        .and_then(|v| v.split_whitespace().next())
        .and_then(|v| v.parse().ok())
}

/// Pids of the old viewers of user `uid` (home `home`) below `proc_root`.
pub fn find(proc_root: &Path, uid: u32, home: &Path) -> Vec<i32> {
    let Ok(dir) = std::fs::read_dir(proc_root) else {
        return Vec::new();
    };
    let mut pids: Vec<i32> = dir
        .filter_map(|e| e.ok()?.file_name().to_str()?.parse::<i32>().ok())
        .filter(|pid| {
            let base = proc_root.join(pid.to_string());
            let owned = std::fs::read_to_string(base.join("status"))
                .ok()
                .and_then(|s| real_uid(&s))
                == Some(uid);
            owned
                && std::fs::read(base.join("cmdline")).is_ok_and(|c| {
                    let argv: Vec<OsString> = c
                        .split(|b| *b == 0)
                        .filter(|a| !a.is_empty())
                        .map(|a| std::ffi::OsStr::from_bytes(a).to_owned())
                        .collect();
                    is_old_server(&argv, home)
                })
        })
        .collect();
    pids.sort_unstable();
    pids
}

/// Sends SIGTERM, waits up to `grace`, then SIGKILLs what is left. Returns the pids that
/// were still alive at the end.
pub fn stop(pids: &[i32], grace: Duration) -> Vec<i32> {
    let alive = |p: i32| kill(Pid::from_raw(p), None).is_ok() && !zombie(p);
    for &p in pids {
        let _ = kill(Pid::from_raw(p), Signal::SIGTERM);
    }
    let deadline = Instant::now() + grace;
    while pids.iter().any(|&p| alive(p)) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    for &p in pids.iter().filter(|&&p| alive(p)) {
        let _ = kill(Pid::from_raw(p), Signal::SIGKILL);
    }
    std::thread::sleep(Duration::from_millis(20));
    pids.iter().copied().filter(|&p| alive(p)).collect()
}

/// A zombie still answers signal 0 but holds no socket any more.
fn zombie(pid: i32) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/stat"))
        .ok()
        .and_then(|s| s.rsplit_once(')').map(|(_, rest)| rest.trim_start().starts_with('Z')))
        .unwrap_or(false)
}

/// Whether a `/proc/net/tcp`-format table lists a listening socket on `port`.
pub fn table_listens(table: &str, port: u16) -> bool {
    table.lines().skip(1).any(|l| {
        let mut f = l.split_whitespace();
        let local = f.nth(1);
        let state = f.nth(1);
        state == Some(TCP_LISTEN)
            && local
                .and_then(|a| a.rsplit_once(':'))
                .and_then(|(_, p)| u16::from_str_radix(p, 16).ok())
                == Some(port)
    })
}

/// Whether anything in this network namespace listens on TCP `port` (IPv4 or IPv6).
pub fn port_listened(proc_root: &Path, port: u16) -> bool {
    ["net/tcp", "net/tcp6"]
        .iter()
        .filter_map(|t| std::fs::read_to_string(proc_root.join(t)).ok())
        .any(|t| table_listens(&t, port))
}

/// The persisted viewer port: carried over by this install, or by an earlier one.
pub fn persisted_port(migrated: Option<u16>, migrated_port_file: &Path) -> Option<u16> {
    migrated.or_else(|| std::fs::read_to_string(migrated_port_file).ok()?.trim().parse().ok())
}

pub fn proc_root() -> PathBuf {
    PathBuf::from("/proc")
}

#[cfg(test)]
mod tests {
    use std::process::{Command, Stdio};

    use super::*;

    fn argv(a: &[&str]) -> Vec<OsString> {
        a.iter().map(OsString::from).collect()
    }

    #[test]
    fn recognises_the_old_viewer_of_this_home_only() {
        let home = Path::new("/home/u");
        let script = "/home/u/.zim/modules/cinelog/http_server.py";
        assert!(is_old_server(&argv(&["python3", script]), home));
        assert!(is_old_server(&argv(&["/usr/bin/python3.12", "-u", script]), home));
        assert!(!is_old_server(
            &argv(&["python3", "/home/other/.zim/modules/cinelog/http_server.py"]),
            home
        ));
        assert!(!is_old_server(&argv(&["python3", "/home/u/srv/http_server.py"]), home));
        assert!(!is_old_server(&argv(&["vim", script]), home));
        assert!(!is_old_server(&argv(&["python3", "cinelog/http_server.py"]), home));
        assert!(!is_old_server(&[], home));
    }

    #[test]
    fn listening_sockets_by_port() {
        let table = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode
   0: 0100007F:2710 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 1 1 0 100 0 0 10 0
   1: 0100007F:2AF8 0100007F:A000 01 00000000:00000000 00:00000000 00000000  1000        0 2 1 0 100 0 0 10 0
";
        assert!(table_listens(table, 10000));
        assert!(!table_listens(table, 11000), "established, not listening");
        assert!(!table_listens(table, 80));
        let v6 = "  sl  local_address                         remote_address                        st
   0: 00000000000000000000000000000000:2AF8 00000000000000000000000000000000:0000 0A";
        assert!(table_listens(v6, 11000));
    }

    #[test]
    fn port_from_this_install_or_the_file() {
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("migrated-port");
        assert_eq!(persisted_port(None, &f), None);
        std::fs::write(&f, "11000\n").unwrap();
        assert_eq!(persisted_port(None, &f), Some(11000));
        assert_eq!(persisted_port(Some(12000), &f), Some(12000));
    }

    /// A stand-in viewer (`sh` is not python, so a copy named python3 runs the script) is
    /// found under a fake home and stopped; a process ignoring SIGTERM is killed.
    #[test]
    fn finds_and_stops_a_running_viewer() {
        let d = tempfile::tempdir().unwrap();
        let home = d.path().join("home");
        let module = home.join(".zim/modules/cinelog");
        std::fs::create_dir_all(&module).unwrap();
        let script = module.join("http_server.py");
        std::fs::write(&script, "trap '' TERM; while :; do sleep 1; done\n").unwrap();
        let python = d.path().join("python3");
        std::os::unix::fs::symlink("/bin/sh", &python).unwrap();
        let mut child = Command::new(&python)
            .arg(&script)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .spawn()
            .unwrap();
        let pid = child.id() as i32;
        let uid = nix::unistd::getuid().as_raw();
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut found = Vec::new();
        while Instant::now() < deadline {
            found = find(&proc_root(), uid, &home);
            if !found.is_empty() {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(found, vec![pid]);
        assert!(find(&proc_root(), uid, &d.path().join("elsewhere")).is_empty());
        assert!(stop(&found, Duration::from_millis(200)).is_empty());
        child.wait().unwrap();
    }
}
