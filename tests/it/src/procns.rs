//! Detection under `hidepid=2` and devpts instances (I17), without root.
//!
//! [`run`] builds this layout and runs a command on the slave of a PTY inside it:
//!
//! ```text
//! unshare --map-users=0:<uid>:1,1:<subuid>:1 -mpf     L1: root = our uid, uid 1 = a subuid
//!   mount -t proc -o hidepid=… proc /proc            private pid namespace, own /proc
//!   anchor: unshare -U --map-user=<uid> sleep         L2: the nested userns everything
//!                                                     "ordinary" joins via nsenter
//!   [decoy]  devpts instance A, L2 process holding a master with tty-index 0, comm `script`
//!   devpts instance B on /dev/pts (+ /dev/ptmx bind)
//!   holder: python3 pty.fork() with comm <holder_comm>
//!     readable: runs in L2 (same credentials as the shell: its fds are readable)
//!     denied:   runs in L1 and drops to uid 1 after the fork (another uid: its /proc/<pid>/fd
//!               is unreadable and, under hidepid=2, the whole process is invisible)
//!     └─ the command, in L2 as our uid with no capabilities, on /dev/pts/0 of instance B
//! ```
//!
//! The decoy holds the same tty index in another devpts instance, so a detector that trusts
//! the index alone picks the decoy (`script`) instead of the real holder.

use std::fs;
use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};

use crate::sandbox::Sandbox;
use crate::util::{output_with_timeout, scaled, scratch_dir, sh_quote};

/// How the PTY holder (the shell's parent) looks to the shell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Holder {
    /// Same uid and user namespace as the shell: found by the ancestor walk.
    Readable,
    /// Another uid: its fds cannot be read; under hidepid=2 it is invisible.
    Denied,
}

#[derive(Debug, Clone)]
pub struct Layout {
    /// Mount the private `/proc` with `hidepid=invisible`.
    pub hidepid: bool,
    /// A decoy holds a master with the same tty index in another devpts instance.
    pub decoy: bool,
    pub holder: Holder,
    /// comm of the holder (`PR_SET_NAME`, at most 15 bytes).
    pub holder_comm: String,
}

/// What the command printed on the PTY, and the tty names involved.
#[derive(Debug, Clone)]
pub struct Outcome {
    /// `tty` of the command (inside the namespace).
    pub tty: String,
    /// `ttyname` of the decoy's slave, if there is a decoy.
    pub decoy_tty: Option<String>,
    /// Everything else the command printed (`\r\n` → `\n`).
    pub output: String,
}

const HOLDER_PY: &str = r#"import ctypes, os, pty, sys
libc = ctypes.CDLL(None, use_errno=True)
name, mode, argv = sys.argv[1].encode(), sys.argv[2], sys.argv[3:]
libc.prctl(15, ctypes.c_char_p(name[:15]), 0, 0, 0)
pid, fd = pty.fork()
if pid == 0:
    os.execvp(argv[0], argv)
if mode == "drop":
    os.setgroups([])
    os.setresgid(1, 1, 1)
    os.setresuid(1, 1, 1)
    # Changing credentials clears "dumpable", which would make /proc/<pid> owned by the
    # namespace root again (our uid); set it back so the entries belong to uid 1.
    libc.prctl(4, 1, 0, 0, 0)
out = b""
while True:
    try:
        d = os.read(fd, 65536)
    except OSError:
        break
    if not d:
        break
    out += d
_, status = os.waitpid(pid, 0)
sys.stdout.buffer.write(out)
sys.stdout.flush()
sys.exit(os.waitstatus_to_exitcode(status))
"#;

const DECOY_PY: &str = r#"import ctypes, os, sys, time
libc = ctypes.CDLL(None, use_errno=True)
libc.prctl(15, ctypes.c_char_p(sys.argv[1].encode()[:15]), 0, 0, 0)
m, s = os.openpty()
with open(sys.argv[2] + ".tmp", "w") as f:
    f.write(os.ttyname(s))
os.rename(sys.argv[2] + ".tmp", sys.argv[2])
time.sleep(3600)
"#;

const DECOY_COMM: &str = "script";
const TTY_TAG: &str = "CLIT-TTY=";

/// First subordinate id of the current user in `/etc/subuid` or `/etc/subgid`.
fn subordinate(file: &str) -> Result<u32> {
    let uid = nix::unistd::getuid();
    let name = nix::unistd::User::from_uid(uid).ok().flatten().map(|u| u.name);
    let text = fs::read_to_string(file).with_context(|| format!("reading {file}"))?;
    text.lines()
        .filter_map(|l| {
            let f: Vec<&str> = l.trim().split(':').collect();
            (f.len() == 3).then_some(f)
        })
        .filter(|f| Some(f[0]) == name.as_deref() || f[0] == uid.to_string())
        .find_map(|f| {
            let (start, count) = (f[1].parse::<u32>().ok()?, f[2].parse::<u32>().ok()?);
            (count >= 1).then_some(start)
        })
        .with_context(|| format!("no subordinate range for uid {uid} in {file}"))
}

/// The `sh` script run as root of L1.
fn script(layout: &Layout, work: &Path, inner: &[String]) -> String {
    let (uid, gid) = (nix::unistd::getuid(), nix::unistd::getgid());
    let q = |p: &Path| sh_quote(&p.to_string_lossy());
    let devpts = "mount -t devpts -o newinstance,ptmxmode=0666,mode=0620 devpts /dev/pts || exit 92\n\
                  mount --bind /dev/pts/ptmx /dev/ptmx || exit 93\n";
    let mut s = format!(
        // Wait for both maps (written from outside, see `run`): mount(8) refuses an unmapped
        // uid, and a nested `unshare -U` fails with EPERM while the gid is still unmapped.
        "n=0; while [ \"$(id -u):$(id -g)\" != 0:0 ]; do n=$((n+1)); [ $n -gt 500 ] && {{ id >&2; exit 90; }}; sleep 0.01; done\n\
         mount -t proc -o {proc} proc /proc || {{ id >&2; exit 91; }}\n\
         unshare -U --map-user={uid} --map-group={gid} sleep 3600 & anchor=$!\n\
         n=0; while [ \"$(readlink /proc/$anchor/ns/user)\" = \"$(readlink /proc/self/ns/user)\" ]; do\n\
         n=$((n+1)); [ $n -gt 500 ] && exit 94; sleep 0.01; done\n\
         IN=\"nsenter --user=/proc/$anchor/ns/user --preserve-credentials\"\n",
        proc = if layout.hidepid { "hidepid=invisible" } else { "rw" },
    );
    if layout.decoy {
        let tty = work.join("decoy-tty");
        s.push_str(devpts);
        s.push_str(&format!(
            "$IN python3 {py} {comm} {tty} </dev/null >/dev/null 2>&1 &\n\
             n=0; while [ ! -s {tty} ]; do n=$((n+1)); [ $n -gt 500 ] && exit 95; sleep 0.01; done\n",
            py = q(&work.join("decoy.py")),
            comm = DECOY_COMM,
            tty = q(&tty),
        ));
    }
    s.push_str(devpts);
    let shell = [
        "sh".to_owned(),
        "-c".into(),
        format!("printf '{TTY_TAG}%s\\n' \"$(tty)\"; exec \"$@\""),
        "sh".into(),
    ]
    .into_iter()
    .chain(inner.iter().cloned())
    .map(|a| sh_quote(&a))
    .collect::<Vec<_>>()
    .join(" ");
    let holder = q(&work.join("holder.py"));
    let comm = sh_quote(&layout.holder_comm);
    match layout.holder {
        Holder::Readable => s.push_str(&format!("$IN python3 {holder} {comm} keep {shell}\n")),
        Holder::Denied => s.push_str(&format!("python3 {holder} {comm} drop $IN {shell}\n")),
    }
    s
}

/// Runs `inner` (an argv; usually [`Sandbox::env_argv`]) on the PTY of the layout.
pub fn run(work: &Path, layout: &Layout, inner: &[String]) -> Result<Outcome> {
    let (uid, gid) = (nix::unistd::getuid(), nix::unistd::getgid());
    let (subuid, subgid) = (subordinate("/etc/subuid")?, subordinate("/etc/subgid")?);
    fs::write(work.join("holder.py"), HOLDER_PY)?;
    fs::write(work.join("decoy.py"), DECOY_PY)?;
    let _ = fs::remove_file(work.join("decoy-tty"));
    // The two-range maps are written with newuidmap/newgidmap directly: util-linux 2.39
    // (Ubuntu 24.04) keeps only the last of several --map-users options, which leaves uid 0
    // unmapped. Once `unshare` is in its new user namespace, map it; the script waits for that.
    let outer = format!(
        "unshare -U -mpf --kill-child sh -c \"$1\" & p=$!\n\
         n=0; while [ \"$(readlink /proc/$p/ns/user)\" = \"$(readlink /proc/self/ns/user)\" ]; do\n\
         n=$((n+1)); [ $n -gt 500 ] && exit 89; sleep 0.01; done\n\
         newuidmap $p 0 {uid} 1 1 {subuid} 1 || exit 88\n\
         newgidmap $p 0 {gid} 1 1 {subgid} 1 || exit 88\n\
         wait $p\n"
    );
    let mut cmd = Command::new("sh");
    cmd.args(["-c".into(), outer, "sh".into(), script(layout, work, inner)])
        .stdin(Stdio::null());
    let out = output_with_timeout(cmd, scaled(60.0))?;
    let text = String::from_utf8_lossy(&out.stdout).replace("\r\n", "\n");
    if !out.status.success() && !text.contains(TTY_TAG) {
        bail!(
            "namespace setup failed ({:?}): {}{}",
            out.status,
            text,
            String::from_utf8_lossy(&out.stderr)
        );
    }
    let mut tty = None;
    let mut rest = Vec::new();
    for line in text.lines() {
        match line.strip_prefix(TTY_TAG) {
            Some(t) if tty.is_none() => tty = Some(t.trim().to_owned()),
            _ => rest.push(line),
        }
    }
    let decoy_tty = layout
        .decoy
        .then(|| fs::read_to_string(work.join("decoy-tty")).map(|s| s.trim().to_owned()))
        .transpose()?;
    Ok(Outcome {
        tty: tty.with_context(|| format!("the command never ran: {text:?}"))?,
        decoy_tty,
        output: rest.join("\n"),
    })
}

impl Sandbox {
    /// [`run`] with `program` in the sandbox environment (`env -i <sandbox env> …`).
    pub fn in_procns(&self, layout: &Layout, program: &[String]) -> Result<Outcome> {
        run(&self.work, layout, &self.env_argv(&[], program))
    }
}

/// The full layout (hidepid, decoy, denied holder) runs a trivial command.
pub fn probe() -> Result<()> {
    let dir = scratch_dir()?;
    let layout = Layout {
        hidepid: true,
        decoy: true,
        holder: Holder::Denied,
        holder_comm: "clit-probe".into(),
    };
    let o = run(dir.path(), &layout, &["sh".into(), "-c".into(), "id -u".into()])?;
    let uid = nix::unistd::getuid().to_string();
    if o.output.trim() != uid || o.tty != "/dev/pts/0" || o.decoy_tty.as_deref() != Some("/dev/pts/0") {
        bail!("unexpected namespace probe result {o:?}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn script_layouts() {
        let w = Path::new("/w");
        let inner = vec!["echo".to_owned(), "a b".to_owned()];
        let mut l = Layout {
            hidepid: true,
            decoy: false,
            holder: Holder::Denied,
            holder_comm: "tmux: server".into(),
        };
        let s = script(&l, w, &inner);
        assert!(s.contains("hidepid=invisible"));
        assert_eq!(s.matches("newinstance").count(), 1);
        assert!(s.contains("python3 /w/holder.py 'tmux: server' drop $IN sh -c"), "{s}");
        assert!(s.ends_with("echo 'a b'\n"), "{s}");
        l.decoy = true;
        l.hidepid = false;
        l.holder = Holder::Readable;
        let s = script(&l, w, &inner);
        assert!(s.contains("\nmount -t proc -o rw "), "{s}");
        assert_eq!(s.matches("newinstance").count(), 2);
        assert!(s.contains("$IN python3 /w/decoy.py script /w/decoy-tty"));
        assert!(s.contains("$IN python3 /w/holder.py 'tmux: server' keep sh"));
    }
}
