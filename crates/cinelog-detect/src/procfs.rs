//! [`RealProcFs`]: the [`ProcFs`] implementation over `/proc` or a fixture tree, plus the
//! pure parsers for the `/proc` text formats it reads.

use std::fs;
use std::io;
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::path::{Path, PathBuf};

use cinelog_core::registry::ProcessProbe;

use crate::{FdEntry, Hidepid, Pid, ProcFs, ProcStat, Uids};

/// Name of the per-pid file listing entries that read as EACCES in fixture trees.
pub(crate) const DENY_FILE: &str = ".eacces";
/// Name of the per-pid file mapping fds to devpts instances (`<fd> <dev>` lines) in fixture trees.
pub(crate) const DEVPTS_FILE: &str = ".devpts";

/// The pty multiplexer device `/dev/ptmx` (and `<devpts>/ptmx`): char 5:2.
const PTMX_RDEV: u64 = (5 << 8) | 2;

fn is_devpts(path: &Path) -> io::Result<bool> {
    let fs = nix::sys::statfs::statfs(path).map_err(io::Error::from)?;
    Ok(fs.filesystem_type() == nix::sys::statfs::DEVPTS_SUPER_MAGIC)
}

/// [`ProcFs`] over a directory laid out like `/proc`.
///
/// [`RealProcFs::new`] reads the live `/proc`. [`RealProcFs::at`] reads a fixture tree, where
/// three conventions replace what files in a repository cannot express: the owner uid of
/// `<pid>/` is the effective uid from `<pid>/status`, `<pid>/.eacces` lists entry names
/// (`fd`, `exe`, `status`, …) that fail with `PermissionDenied`, and `<pid>/.devpts` holds
/// `<fd> <dev>` lines giving the devpts instance of pty fds.
#[derive(Debug, Clone)]
pub struct RealProcFs {
    root: PathBuf,
    live: bool,
}

impl Default for RealProcFs {
    fn default() -> Self {
        Self::new()
    }
}

impl RealProcFs {
    pub fn new() -> Self {
        Self {
            root: PathBuf::from("/proc"),
            live: true,
        }
    }

    pub fn at(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            live: false,
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn entry(&self, pid: Pid, name: &str) -> io::Result<PathBuf> {
        if !self.live {
            let deny = self.root.join(pid.to_string()).join(DENY_FILE);
            if let Ok(list) = fs::read_to_string(deny)
                && list.split_whitespace().any(|n| n == name)
            {
                return Err(io::Error::from(io::ErrorKind::PermissionDenied));
            }
        }
        Ok(self.root.join(pid.to_string()).join(name))
    }

    fn read(&self, pid: Pid, name: &str) -> io::Result<String> {
        fs::read_to_string(self.entry(pid, name)?)
    }
}

fn invalid(what: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, what.to_owned())
}

/// Parses one `/proc/<pid>/stat` line. The comm is everything between the first `(` and
/// the last `)`, so comms containing spaces or parentheses parse correctly.
pub fn parse_stat(line: &str) -> Option<ProcStat> {
    let open = line.find('(')?;
    let close = line.rfind(')')?;
    let pid = line[..open].trim().parse().ok()?;
    let comm = line.get(open + 1..close)?.to_owned();
    let rest: Vec<&str> = line.get(close + 1..)?.split_whitespace().collect();
    // rest[0] is field 3 (state); field N is rest[N - 3].
    let field = |n: usize| rest.get(n - 3).copied();
    Some(ProcStat {
        pid,
        comm,
        state: field(3)?.chars().next()?,
        ppid: field(4)?.parse().ok()?,
        pgrp: field(5)?.parse().ok()?,
        session: field(6)?.parse().ok()?,
        tty_nr: field(7)?.parse::<i64>().ok()? as u64,
        tpgid: field(8)?.parse().ok()?,
        starttime: field(22)?.parse().ok()?,
    })
}

/// The `Uid:` line of `/proc/<pid>/status`.
pub fn parse_status_uids(status: &str) -> Option<Uids> {
    let line = status.lines().find_map(|l| l.strip_prefix("Uid:"))?;
    let mut it = line.split_whitespace().map(|v| v.parse::<u32>());
    let mut next = || it.next()?.ok();
    Some(Uids {
        real: next()?,
        effective: next()?,
        saved: next()?,
        fs: next()?,
    })
}

/// The `tty-index:` value of an fdinfo file.
pub fn parse_fdinfo_tty_index(fdinfo: &str) -> Option<u32> {
    fdinfo
        .lines()
        .find_map(|l| l.strip_prefix("tty-index:"))
        .and_then(|v| v.trim().parse().ok())
}

/// The `hidepid=` option of the `/proc` mount in a `/proc/mounts` table (last mount wins).
pub fn parse_hidepid(mounts: &str) -> Hidepid {
    mounts
        .lines()
        .filter_map(|l| {
            let f: Vec<&str> = l.split_whitespace().collect();
            (f.len() >= 4 && f[1] == "/proc" && f[2] == "proc").then(|| f[3])
        })
        .next_back()
        .map_or(Hidepid::Off, |opts| {
            opts.split(',')
                .find_map(|o| o.strip_prefix("hidepid="))
                .map_or(Hidepid::Off, |v| match v {
                    "1" | "noaccess" => Hidepid::NoAccess,
                    "2" | "invisible" => Hidepid::Invisible,
                    "4" | "ptraceable" => Hidepid::Ptraceable,
                    _ => Hidepid::Off,
                })
        })
}

/// Loginuid file content; `4294967295` (`(uid_t)-1`) means unset.
pub fn parse_loginuid(s: &str) -> Option<u32> {
    s.trim().parse::<u32>().ok().filter(|&u| u != u32::MAX)
}

impl ProcFs for RealProcFs {
    fn self_pid(&self) -> Pid {
        fs::read_link(self.root.join("self"))
            .ok()
            .and_then(|p| p.to_str()?.parse().ok())
            .unwrap_or_else(std::process::id)
    }

    fn pids(&self) -> io::Result<Vec<Pid>> {
        let mut pids: Vec<Pid> = fs::read_dir(&self.root)?
            .filter_map(|e| e.ok()?.file_name().to_str()?.parse().ok())
            .collect();
        pids.sort_unstable();
        Ok(pids)
    }

    fn stat(&self, pid: Pid) -> io::Result<ProcStat> {
        parse_stat(&self.read(pid, "stat")?).ok_or_else(|| invalid("malformed stat"))
    }

    fn comm(&self, pid: Pid) -> io::Result<String> {
        let mut s = self.read(pid, "comm")?;
        if s.ends_with('\n') {
            s.pop();
        }
        Ok(s)
    }

    fn cmdline(&self, pid: Pid) -> io::Result<Vec<String>> {
        let raw = fs::read(self.entry(pid, "cmdline")?)?;
        let raw = raw.strip_suffix(b"\0").unwrap_or(&raw);
        if raw.is_empty() {
            return Ok(Vec::new());
        }
        Ok(raw
            .split(|&b| b == 0)
            .map(|a| String::from_utf8_lossy(a).into_owned())
            .collect())
    }

    fn exe(&self, pid: Pid) -> io::Result<PathBuf> {
        fs::read_link(self.entry(pid, "exe")?)
    }

    fn uids(&self, pid: Pid) -> io::Result<Uids> {
        parse_status_uids(&self.read(pid, "status")?).ok_or_else(|| invalid("no Uid line in status"))
    }

    fn owner_uid(&self, pid: Pid) -> io::Result<u32> {
        if self.live {
            fs::metadata(self.root.join(pid.to_string())).map(|m| m.uid())
        } else {
            self.uids(pid).map(|u| u.effective)
        }
    }

    fn loginuid(&self, pid: Pid) -> io::Result<Option<u32>> {
        self.read(pid, "loginuid").map(|s| parse_loginuid(&s))
    }

    fn fds(&self, pid: Pid) -> io::Result<Vec<FdEntry>> {
        let dir = self.entry(pid, "fd")?;
        let mut out: Vec<FdEntry> = fs::read_dir(&dir)?
            .filter_map(|e| {
                let e = e.ok()?;
                let fd = e.file_name().to_str()?.parse().ok()?;
                // An fd closed between readdir and readlink is simply skipped.
                let target = fs::read_link(e.path()).ok()?;
                Some(FdEntry { fd, target })
            })
            .collect();
        out.sort_unstable_by_key(|e| e.fd);
        Ok(out)
    }

    fn fdinfo_tty_index(&self, pid: Pid, fd: u32) -> io::Result<Option<u32>> {
        let dir = self.entry(pid, "fdinfo")?;
        fs::read_to_string(dir.join(fd.to_string())).map(|s| parse_fdinfo_tty_index(&s))
    }

    fn devpts_dev(&self, pid: Pid, fd: u32) -> io::Result<Option<u64>> {
        let link = self.entry(pid, "fd")?.join(fd.to_string());
        if !self.live {
            let map = match fs::read_to_string(self.root.join(pid.to_string()).join(DEVPTS_FILE)) {
                Ok(m) => m,
                Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
                Err(e) => return Err(e),
            };
            let fd = fd.to_string();
            return Ok(map.lines().find_map(|l| {
                let (f, dev) = l.split_once(' ')?;
                (f == fd).then(|| dev.trim().parse().ok()).flatten()
            }));
        }
        let md = fs::metadata(&link)?;
        if is_devpts(&link)? {
            return Ok(Some(md.dev()));
        }
        if !md.file_type().is_char_device() || md.rdev() != PTMX_RDEV {
            return Ok(None);
        }
        // A master opened through a ptmx node outside devpts belongs to the devpts mounted at
        // `pts` beside that node, as the holder's root sees it (kernel `devpts_ptmx_path`).
        let target = fs::read_link(&link)?;
        let Some(dir) = target.parent() else { return Ok(None) };
        let pts = self
            .root
            .join(pid.to_string())
            .join("root")
            .join(dir.strip_prefix("/").unwrap_or(dir))
            .join("pts");
        match is_devpts(&pts) {
            Ok(true) => Ok(Some(fs::metadata(&pts)?.dev())),
            Ok(false) | Err(_) => Ok(None),
        }
    }

    fn boot_id(&self) -> io::Result<String> {
        fs::read_to_string(self.root.join("sys/kernel/random/boot_id")).map(|s| s.trim().to_owned())
    }

    fn hidepid(&self) -> io::Result<Hidepid> {
        fs::read_to_string(self.root.join("mounts")).map(|m| parse_hidepid(&m))
    }
}

impl ProcessProbe for RealProcFs {
    fn starttime(&self, pid: u32) -> Option<u64> {
        self.stat(pid).ok().map(|s| s.starttime)
    }

    fn boot_id(&self) -> Option<String> {
        ProcFs::boot_id(self).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stat_with_hostile_comm() {
        let line = "4242 (a) b (c)) S 1 4242 4242 34816 4250 4194560 1 2 3 4 5 6 7 8 20 0 1 0 987654 1 2 3\n";
        let s = parse_stat(line).unwrap();
        assert_eq!(s.pid, 4242);
        assert_eq!(s.comm, "a) b (c)");
        assert_eq!(s.state, 'S');
        assert_eq!((s.ppid, s.pgrp, s.session), (1, 4242, 4242));
        assert_eq!(s.tty_nr, 34816);
        assert_eq!(s.tpgid, 4250);
        assert_eq!(s.starttime, 987654);
    }

    #[test]
    fn stat_without_tty() {
        let line = "7 (tmux: server) S 1 7 7 0 -1 4194624 0 0 0 0 0 0 0 0 20 0 1 0 55 1 2";
        let s = parse_stat(line).unwrap();
        assert_eq!(s.comm, "tmux: server");
        assert_eq!(s.tty_nr, 0);
        assert_eq!(s.tpgid, -1);
        assert_eq!(s.starttime, 55);
        assert!(parse_stat("7 (x) S 1 7").is_none());
        assert!(parse_stat("garbage").is_none());
    }

    #[test]
    fn status_uids() {
        let st = "Name:\tsudo\nUmask:\t0022\nUid:\t1000\t0\t0\t0\nGid:\t1000\t1000\t1000\t1000\n";
        assert_eq!(
            parse_status_uids(st),
            Some(Uids {
                real: 1000,
                effective: 0,
                saved: 0,
                fs: 0
            })
        );
        assert_eq!(parse_status_uids("Name:\tx\n"), None);
    }

    #[test]
    fn fdinfo_and_loginuid() {
        let info = "pos:\t0\nflags:\t0100002\nmnt_id:\t25\nino:\t2\ntty-index:\t17\n";
        assert_eq!(parse_fdinfo_tty_index(info), Some(17));
        assert_eq!(parse_fdinfo_tty_index("pos:\t0\n"), None);
        assert_eq!(parse_loginuid("4294967295"), None);
        assert_eq!(parse_loginuid("1000\n"), Some(1000));
        assert_eq!(parse_loginuid("0"), Some(0));
    }

    #[test]
    fn hidepid_options() {
        let m = |o: &str| format!("sysfs /sys sysfs rw 0 0\nproc /proc proc rw,nosuid,{o} 0 0\n");
        assert_eq!(parse_hidepid(&m("relatime")), Hidepid::Off);
        assert_eq!(parse_hidepid(&m("hidepid=2")), Hidepid::Invisible);
        assert_eq!(parse_hidepid(&m("hidepid=invisible,subset=pid")), Hidepid::Invisible);
        assert_eq!(parse_hidepid(&m("hidepid=noaccess")), Hidepid::NoAccess);
        assert_eq!(parse_hidepid(&m("hidepid=4")), Hidepid::Ptraceable);
        let two = "proc /proc proc rw,hidepid=2 0 0\nproc /proc proc rw 0 0\n";
        assert_eq!(parse_hidepid(two), Hidepid::Off);
        assert_eq!(parse_hidepid("proc /mnt/p proc rw,hidepid=2 0 0\n"), Hidepid::Off);
    }

    #[test]
    fn live_self() {
        let p = RealProcFs::new();
        let me = p.self_pid();
        assert_eq!(me, std::process::id());
        let st = p.stat(me).unwrap();
        assert_eq!(st.pid, me);
        assert_eq!(st.ppid, nix::unistd::getppid().as_raw() as u32);
        assert_eq!(st.pgrp, nix::unistd::getpgrp().as_raw() as u32);
        let uids = p.uids(me).unwrap();
        assert_eq!(uids.real, nix::unistd::getuid().as_raw());
        assert_eq!(uids.effective, nix::unistd::geteuid().as_raw());
        assert_eq!(p.owner_uid(me).unwrap(), uids.effective);
        assert!(!p.cmdline(me).unwrap().is_empty());
        assert!(!p.comm(me).unwrap().ends_with('\n'));
        assert!(p.pids().unwrap().contains(&me));
        assert!(!p.fds(me).unwrap().is_empty());
        assert!(!ProcFs::boot_id(&p).unwrap().is_empty());
        assert!(p.hidepid().is_ok());
        assert_eq!(ProcessProbe::starttime(&p, me), Some(st.starttime));
        assert!(p.stat(u32::MAX - 1).is_err());
    }
}
