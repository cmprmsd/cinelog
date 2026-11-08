//! Test support: materializes a compact process-tree description (`tests/fixtures/proc/*.txt`)
//! into a directory laid out like `/proc`, read back with [`RealProcFs::at`].
//!
//! One directive per line, `#` starts a comment, values with spaces are double-quoted:
//!
//! ```text
//! boot <boot_id>                      default "fixture-boot"
//! self <pid>                          /proc/self
//! selfexe <path>                      exe an own recorder must run
//! hidepid <off|noaccess|invisible|ptraceable|unreadable>
//! user <uid> <name>                   passwd entry
//! proc pid=… comm=… [ppid=1] [pgrp=pid] [sid=pgrp] [uid=1000] [ruid=…] [euid=…]
//!      [exe=…] [args=a|b|c] [tty=pts/N|ttyN] [tpgid=-1] [start=pid*100] [loginuid=…]
//!      [ptmx=fd:index[@dev],…] [fds=fd:target,…] [devpts=1|-] [deny=fd,exe,status,…] [state=S]
//! registry pts=N pid=P uuid=U [start=…] [boot=…] [scope=personal|team]
//! expect …                            kept verbatim for the test driver
//! ```
//!
//! `tty=pts/N` also opens the slave on fds 0–2 unless `fds=`/`ptmx=` name them. `devpts=`
//! is the devpts instance (an arbitrary device number) of the proc's pty fds, default 1;
//! `@dev` overrides it for one master, and `-` leaves the instance unknown.

use std::collections::HashMap;
use std::fs;
use std::io;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};

use cinelog_core::registry::{REGISTRY_FORMAT, RecState, RegistryEntry};
use cinelog_proto::{Nonce, Scope};

use crate::procfs::{DENY_FILE, DEVPTS_FILE};
use crate::{ProcDetector, RealProcFs};

#[derive(Debug, thiserror::Error)]
pub enum FixtureError {
    #[error("fixture line {line}: {msg}")]
    Syntax { line: usize, msg: String },
    #[error("fixture I/O: {0}")]
    Io(#[from] io::Error),
}

/// A materialized fixture tree plus the parts of the description that are not files.
#[derive(Debug, Clone)]
pub struct Fixture {
    pub root: PathBuf,
    pub users: HashMap<u32, String>,
    pub registry: HashMap<u32, RegistryEntry>,
    pub self_exe: Option<PathBuf>,
    /// `expect` lines without the keyword, with their line numbers.
    pub expectations: Vec<(usize, String)>,
}

/// A `ptmx=` entry: tty-index and devpts instance (`None`: the proc's own `devpts=`).
#[derive(Debug, Clone, Copy)]
struct Master {
    index: u32,
    devpts: Option<Option<u64>>,
}

#[derive(Debug, Default)]
struct ProcSpec {
    pid: u32,
    ppid: u32,
    pgrp: u32,
    sid: u32,
    uids: [u32; 4],
    comm: String,
    exe: Option<String>,
    args: Vec<String>,
    tty_nr: u64,
    slave: Option<u32>,
    tpgid: i64,
    start: u64,
    loginuid: Option<u32>,
    ptmx: Vec<(u32, Master)>,
    fds: Vec<(u32, String)>,
    devpts: Option<u64>,
    deny: Vec<String>,
    state: char,
}

/// Splits on whitespace; `"…"` groups a value containing spaces (no escapes).
fn tokens(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quoted = false;
    let mut any = false;
    for c in line.chars() {
        match c {
            '"' => {
                quoted = !quoted;
                any = true;
            }
            c if c.is_whitespace() && !quoted => {
                if any {
                    out.push(std::mem::take(&mut cur));
                    any = false;
                }
            }
            c => {
                cur.push(c);
                any = true;
            }
        }
    }
    if any {
        out.push(cur);
    }
    out
}

fn encode_tty(s: &str) -> Option<u64> {
    let (major, minor): (u64, u64) = if let Some(n) = s.strip_prefix("pts/") {
        (136, n.parse().ok()?)
    } else {
        (4, s.strip_prefix("tty")?.parse().ok()?)
    };
    // new_encode_dev()
    Some((minor & 0xff) | (major << 8) | ((minor & !0xff) << 12))
}

struct Parser {
    line: usize,
}

impl Parser {
    fn err(&self, msg: impl Into<String>) -> FixtureError {
        FixtureError::Syntax {
            line: self.line,
            msg: msg.into(),
        }
    }

    fn num<T: std::str::FromStr>(&self, key: &str, v: &str) -> Result<T, FixtureError> {
        v.parse().map_err(|_| self.err(format!("{key}: not a number: {v:?}")))
    }

    fn pairs<'a>(&self, toks: &'a [String]) -> Result<Vec<(&'a str, &'a str)>, FixtureError> {
        toks.iter()
            .map(|t| {
                t.split_once('=')
                    .ok_or_else(|| self.err(format!("expected key=value, got {t:?}")))
            })
            .collect()
    }

    fn fd_list<T>(&self, v: &str, f: impl Fn(&str) -> Result<T, FixtureError>) -> Result<Vec<(u32, T)>, FixtureError> {
        v.split(',')
            .map(|item| {
                let (fd, rest) = item
                    .split_once(':')
                    .ok_or_else(|| self.err(format!("expected fd:value: {item:?}")))?;
                Ok((self.num("fd", fd)?, f(rest)?))
            })
            .collect()
    }

    fn devpts(&self, v: &str) -> Result<Option<u64>, FixtureError> {
        if v == "-" {
            Ok(None)
        } else {
            self.num("devpts", v).map(Some)
        }
    }

    /// `index[@dev|@-]`.
    fn ptmx_item(&self, v: &str) -> Result<Master, FixtureError> {
        let (index, devpts) = match v.split_once('@') {
            Some((i, dev)) => (i, Some(self.devpts(dev)?)),
            None => (v, None),
        };
        Ok(Master {
            index: self.num("tty-index", index)?,
            devpts,
        })
    }

    fn proc(&self, toks: &[String]) -> Result<ProcSpec, FixtureError> {
        let mut p = ProcSpec {
            ppid: 1,
            uids: [1000; 4],
            tpgid: -1,
            devpts: Some(1),
            state: 'S',
            ..ProcSpec::default()
        };
        let (mut pgrp, mut sid, mut start, mut args) = (None, None, None, None);
        let (mut ruid, mut euid) = (None, None);
        for (k, v) in self.pairs(toks)? {
            match k {
                "pid" => p.pid = self.num(k, v)?,
                "ppid" => p.ppid = self.num(k, v)?,
                "pgrp" => pgrp = Some(self.num(k, v)?),
                "sid" => sid = Some(self.num(k, v)?),
                "uid" => p.uids = [self.num(k, v)?; 4],
                "ruid" => ruid = Some(self.num(k, v)?),
                "euid" => euid = Some(self.num(k, v)?),
                "comm" => p.comm = v.to_owned(),
                "exe" => p.exe = Some(v.to_owned()),
                "args" => args = Some(v.split('|').map(str::to_owned).collect()),
                "tty" => {
                    p.tty_nr = encode_tty(v).ok_or_else(|| self.err(format!("bad tty {v:?}")))?;
                    p.slave = v.strip_prefix("pts/").and_then(|n| n.parse().ok());
                }
                "tpgid" => p.tpgid = self.num(k, v)?,
                "start" => start = Some(self.num(k, v)?),
                "loginuid" => p.loginuid = Some(self.num(k, v)?),
                "ptmx" => p.ptmx = self.fd_list(v, |i| self.ptmx_item(i))?,
                "fds" => p.fds = self.fd_list(v, |t| Ok(t.to_owned()))?,
                "devpts" => p.devpts = self.devpts(v)?,
                "deny" => p.deny = v.split(',').map(str::to_owned).collect(),
                "state" => p.state = v.chars().next().ok_or_else(|| self.err("empty state"))?,
                _ => return Err(self.err(format!("unknown proc key {k:?}"))),
            }
        }
        if p.pid == 0 || p.comm.is_empty() {
            return Err(self.err("proc needs pid and comm"));
        }
        if let Some(r) = ruid {
            p.uids[0] = r;
        }
        if let Some(e) = euid {
            p.uids[1] = e;
            p.uids[2] = e;
            p.uids[3] = e;
        }
        p.pgrp = pgrp.unwrap_or(p.pid);
        p.sid = sid.unwrap_or(p.pgrp);
        p.start = start.unwrap_or(u64::from(p.pid) * 100);
        p.args = args.unwrap_or_else(|| vec![p.comm.clone()]);
        Ok(p)
    }
}

fn write_proc(root: &Path, p: &ProcSpec) -> io::Result<()> {
    let dir = root.join(p.pid.to_string());
    fs::create_dir_all(dir.join("fd"))?;
    fs::create_dir_all(dir.join("fdinfo"))?;
    let stat = format!(
        "{} ({}) {} {} {} {} {} {} 4194560 0 0 0 0 0 0 0 0 20 0 1 0 {} 1000 100 18446744073709551615\n",
        p.pid, p.comm, p.state, p.ppid, p.pgrp, p.sid, p.tty_nr, p.tpgid, p.start
    );
    fs::write(dir.join("stat"), stat)?;
    fs::write(dir.join("comm"), format!("{}\n", p.comm))?;
    let mut cmdline = p.args.join("\0").into_bytes();
    cmdline.push(0);
    fs::write(dir.join("cmdline"), cmdline)?;
    let [r, e, s, f] = p.uids;
    fs::write(
        dir.join("status"),
        format!("Name:\t{}\nPid:\t{}\nUid:\t{r}\t{e}\t{s}\t{f}\n", p.comm, p.pid),
    )?;
    fs::write(dir.join("loginuid"), p.loginuid.unwrap_or(u32::MAX).to_string())?;
    if let Some(exe) = &p.exe {
        symlink(exe, dir.join("exe"))?;
    }
    let mut fds = p.fds.clone();
    if let Some(n) = p.slave {
        let named = |fd: u32| p.fds.iter().any(|f| f.0 == fd) || p.ptmx.iter().any(|f| f.0 == fd);
        fds.extend((0..3).filter(|&fd| !named(fd)).map(|fd| (fd, format!("/dev/pts/{n}"))));
    }
    let mut devpts = String::new();
    for (fd, target) in &fds {
        symlink(target, dir.join("fd").join(fd.to_string()))?;
        fs::write(
            dir.join("fdinfo").join(fd.to_string()),
            "pos:\t0\nflags:\t0100002\nmnt_id:\t25\n",
        )?;
        if let (true, Some(dev)) = (target.starts_with("/dev/pts/"), p.devpts) {
            devpts.push_str(&format!("{fd} {dev}\n"));
        }
    }
    for (fd, Master { index, devpts: inst }) in &p.ptmx {
        symlink("/dev/ptmx", dir.join("fd").join(fd.to_string()))?;
        fs::write(
            dir.join("fdinfo").join(fd.to_string()),
            format!("pos:\t0\nflags:\t0102002\nmnt_id:\t25\nino:\t2\ntty-index:\t{index}\n"),
        )?;
        if let Some(dev) = inst.unwrap_or(p.devpts) {
            devpts.push_str(&format!("{fd} {dev}\n"));
        }
    }
    if !devpts.is_empty() {
        fs::write(dir.join(DEVPTS_FILE), devpts)?;
    }
    if !p.deny.is_empty() {
        fs::write(dir.join(DENY_FILE), p.deny.join("\n"))?;
    }
    Ok(())
}

impl Fixture {
    /// Writes the tree described by `spec` into `root` (which should be empty).
    pub fn materialize(spec: &str, root: &Path) -> Result<Self, FixtureError> {
        let mut boot = "fixture-boot".to_owned();
        let mut self_pid = None;
        let mut mounts = Some("proc /proc proc rw,nosuid,nodev,noexec,relatime 0 0\n".to_owned());
        let mut fx = Self {
            root: root.to_path_buf(),
            users: HashMap::new(),
            registry: HashMap::new(),
            self_exe: None,
            expectations: Vec::new(),
        };
        let mut procs: Vec<ProcSpec> = Vec::new();
        let mut registry_lines = Vec::new();
        for (i, raw) in spec.lines().enumerate() {
            let parser = Parser { line: i + 1 };
            let line = raw.split_once('#').map_or(raw, |(l, _)| l).trim();
            let Some((kw, rest)) = line
                .split_once(char::is_whitespace)
                .or((!line.is_empty()).then_some((line, "")))
            else {
                continue;
            };
            let toks = tokens(rest);
            match kw {
                "boot" => boot = toks.first().cloned().ok_or_else(|| parser.err("boot needs an id"))?,
                "self" => self_pid = Some(parser.num::<u32>("self", toks.first().map_or("", String::as_str))?),
                "selfexe" => fx.self_exe = toks.first().map(PathBuf::from),
                "hidepid" => {
                    let v = toks.first().map_or("", String::as_str);
                    mounts = match v {
                        "unreadable" => None,
                        "off" => mounts,
                        _ => Some(format!("proc /proc proc rw,nosuid,hidepid={v} 0 0\n")),
                    };
                }
                "user" => {
                    let (uid, name) = match toks.as_slice() {
                        [u, n] => (parser.num::<u32>("uid", u)?, n.clone()),
                        _ => return Err(parser.err("user <uid> <name>")),
                    };
                    fx.users.insert(uid, name);
                }
                "proc" => procs.push(parser.proc(&toks)?),
                "registry" => registry_lines.push((i + 1, toks)),
                "expect" => fx.expectations.push((i + 1, rest.trim().to_owned())),
                _ => return Err(parser.err(format!("unknown directive {kw:?}"))),
            }
        }

        fs::create_dir_all(root.join("sys/kernel/random"))?;
        fs::write(root.join("sys/kernel/random/boot_id"), format!("{boot}\n"))?;
        if let Some(m) = mounts {
            fs::write(root.join("mounts"), m)?;
        }
        if let Some(pid) = self_pid {
            symlink(pid.to_string(), root.join("self"))?;
        }
        for p in &procs {
            write_proc(root, p)?;
        }

        for (line, toks) in registry_lines {
            let parser = Parser { line };
            let (mut pts, mut pid, mut uuid, mut start, mut b, mut scope) =
                (None, None, None, None, None, Scope::Personal);
            for (k, v) in parser.pairs(&toks)? {
                match k {
                    "pts" => pts = Some(parser.num::<u32>(k, v)?),
                    "pid" => pid = Some(parser.num::<u32>(k, v)?),
                    "uuid" => uuid = Some(v.to_owned()),
                    "start" => start = Some(parser.num::<u64>(k, v)?),
                    "boot" => b = Some(v.to_owned()),
                    "scope" => scope = v.parse().map_err(|_| parser.err(format!("bad scope {v:?}")))?,
                    _ => return Err(parser.err(format!("unknown registry key {k:?}"))),
                }
            }
            let (Some(pts), Some(pid), Some(uuid)) = (pts, pid, uuid) else {
                return Err(parser.err("registry needs pts, pid and uuid"));
            };
            let starttime = start
                .or_else(|| procs.iter().find(|p| p.pid == pid).map(|p| p.start))
                .unwrap_or(u64::from(pid) * 100);
            fx.registry.insert(
                pts,
                RegistryEntry {
                    format: REGISTRY_FORMAT,
                    pid,
                    starttime,
                    boot_id: b.unwrap_or_else(|| boot.clone()),
                    uuid,
                    nonce: Nonce::from_bytes([0x5a; 16]),
                    scope,
                    cast: PathBuf::from(format!("/logs/{pts}.cast")),
                    pts: format!("/dev/pts/{pts}"),
                    host_tty: String::new(),
                    shell_pid: 0,
                    parent_uuid: None,
                    state: RecState::Live,
                    state_reason: None,
                    part: 1,
                    version: String::new(),
                    opts: String::new(),
                },
            );
        }
        Ok(fx)
    }

    pub fn procfs(&self) -> RealProcFs {
        RealProcFs::at(&self.root)
    }

    /// A detector over this tree with its registry, users and `selfexe`.
    pub fn detector(&self) -> ProcDetector<RealProcFs, HashMap<u32, RegistryEntry>> {
        let users = self.users.clone();
        let det =
            ProcDetector::new(self.procfs(), self.registry.clone()).with_user_names(move |u| users.get(&u).cloned());
        match &self.self_exe {
            Some(exe) => det.with_self_exe(exe),
            None => det,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ProcFs;

    #[test]
    fn tokenizer() {
        assert_eq!(
            tokens(r#"a=1  comm="tmux: server" b"#),
            ["a=1", "comm=tmux: server", "b"]
        );
        assert_eq!(tokens(""), Vec::<String>::new());
        assert_eq!(tokens(r#"x="""#), ["x="]);
    }

    #[test]
    fn tty_encoding() {
        assert_eq!(encode_tty("pts/3"), Some(34819));
        assert_eq!(encode_tty("tty1"), Some(1025));
        assert_eq!(
            encode_tty("pts/300"),
            Some((300 & 0xff) | (136 << 8) | ((300 & !0xff) << 12))
        );
        assert_eq!(encode_tty("ttyX"), None);
    }

    #[test]
    fn roundtrip_through_realprocfs() {
        let dir = tempfile::tempdir().unwrap();
        let spec = r#"
            boot b-1
            self 20
            hidepid invisible
            user 1000 alice
            proc pid=10 comm="tmux: server" ptmx=5:3,6:4@7,7:5@- fds=0:/dev/null deny=exe loginuid=1000
            proc pid=20 ppid=10 comm=zsh tty=pts/3 tpgid=21 ruid=1000 euid=0 args=-zsh|-i
            registry pts=3 pid=10 uuid=u-1
            expect check tty=pts:3 start=20 euid=1000 => start personal tmux-server
        "#;
        let fx = Fixture::materialize(spec, dir.path()).unwrap();
        let p = fx.procfs();
        assert_eq!(p.self_pid(), 20);
        assert_eq!(p.pids().unwrap(), [10, 20]);
        assert_eq!(p.boot_id().unwrap(), "b-1");
        assert_eq!(p.hidepid().unwrap(), crate::Hidepid::Invisible);
        let st = p.stat(20).unwrap();
        assert_eq!(
            (st.ppid, st.pgrp, st.session, st.tty_nr, st.tpgid, st.starttime),
            (10, 20, 20, 34819, 21, 2000)
        );
        assert_eq!(p.comm(10).unwrap(), "tmux: server");
        assert_eq!(p.cmdline(20).unwrap(), ["-zsh", "-i"]);
        assert_eq!(p.uids(20).unwrap().real, 1000);
        assert_eq!(p.owner_uid(20).unwrap(), 0);
        assert_eq!(p.loginuid(10).unwrap(), Some(1000));
        assert_eq!(p.loginuid(20).unwrap(), None);
        assert_eq!(p.exe(10).unwrap_err().kind(), io::ErrorKind::PermissionDenied);
        let fds = p.fds(10).unwrap();
        assert_eq!(fds.iter().map(|f| f.fd).collect::<Vec<_>>(), [0, 5, 6, 7]);
        assert_eq!(p.fdinfo_tty_index(10, 6).unwrap(), Some(4));
        assert_eq!(p.fdinfo_tty_index(10, 0).unwrap(), None);
        assert_eq!(p.devpts_dev(10, 5).unwrap(), Some(1));
        assert_eq!(p.devpts_dev(10, 0).unwrap(), None);
        assert_eq!(p.devpts_dev(10, 6).unwrap(), Some(7));
        assert_eq!(p.devpts_dev(10, 7).unwrap(), None);
        assert_eq!(p.fds(20).unwrap().iter().map(|f| f.fd).collect::<Vec<_>>(), [0, 1, 2]);
        assert_eq!(p.devpts_dev(20, 2).unwrap(), Some(1));
        assert_eq!(fx.registry[&3].starttime, 1000);
        assert_eq!(fx.registry[&3].boot_id, "b-1");
        assert_eq!(fx.expectations.len(), 1);
    }

    #[test]
    fn syntax_errors_name_the_line() {
        let dir = tempfile::tempdir().unwrap();
        let e = Fixture::materialize("boot x\nproc pid=1\n", dir.path()).unwrap_err();
        assert!(e.to_string().contains("line 2"), "{e}");
        let e = Fixture::materialize("frobnicate\n", dir.path()).unwrap_err();
        assert!(e.to_string().contains("unknown directive"), "{e}");
    }
}
