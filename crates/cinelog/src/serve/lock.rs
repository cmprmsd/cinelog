//! The `serve.lock` singleton and `serve --ensure` (§10.4).
//!
//! `serve` holds an exclusive flock(2) on `<runtime_dir>/serve.lock` for its whole lifetime
//! and writes its pid into the file. `--ensure` returns at once while the lock is held,
//! otherwise it starts the systemd unit (when installed) or a detached `serve`.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;

use cinelog_core::config::Autostart;
use nix::errno::Errno;
use nix::fcntl::{Flock, FlockArg};

pub const LOCK_FILE: &str = "serve.lock";

/// Attempts and pause of [`ServeLock::acquire`]: a concurrent `--ensure` probe holds a
/// shared lock for a moment, which must not make a starting server give up.
const ACQUIRE_ATTEMPTS: u32 = 5;
const ACQUIRE_PAUSE: Duration = Duration::from_millis(40);

pub fn lock_path(runtime_dir: &Path) -> PathBuf {
    runtime_dir.join(LOCK_FILE)
}

/// The held `serve.lock`; released when dropped.
#[derive(Debug)]
pub struct ServeLock {
    _lock: Flock<File>,
    path: PathBuf,
}

/// Result of [`ServeLock::acquire`].
#[derive(Debug)]
pub enum Acquire {
    Acquired(ServeLock),
    /// Another process holds the lock; its pid if the file names one.
    Held {
        pid: Option<u32>,
    },
}

impl ServeLock {
    /// Takes the lock (creating the runtime dir 0700 and the file 0600) and records our pid.
    pub fn acquire(runtime_dir: &Path) -> io::Result<Acquire> {
        fs::DirBuilder::new().recursive(true).mode(0o700).create(runtime_dir)?;
        let path = lock_path(runtime_dir);
        let mut file = open_lock_file(&path)?;
        for attempt in 1..=ACQUIRE_ATTEMPTS {
            match Flock::lock(file, FlockArg::LockExclusiveNonblock) {
                Ok(mut lock) => {
                    lock.set_len(0)?;
                    lock.rewind()?;
                    writeln!(lock, "{}", std::process::id())?;
                    lock.flush()?;
                    return Ok(Acquire::Acquired(Self { _lock: lock, path }));
                }
                Err((f, Errno::EWOULDBLOCK)) => {
                    file = f;
                    if attempt < ACQUIRE_ATTEMPTS {
                        thread::sleep(ACQUIRE_PAUSE);
                    }
                }
                Err((_, e)) => return Err(e.into()),
            }
        }
        Ok(Acquire::Held {
            pid: holder_pid(runtime_dir),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

fn open_lock_file(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(path)
}

/// Whether some process holds `serve.lock`, probed with a non-blocking shared lock so the
/// probe never blocks a server that is starting up for long (see [`ACQUIRE_ATTEMPTS`]).
pub fn is_held(runtime_dir: &Path) -> bool {
    let Ok(file) = File::open(lock_path(runtime_dir)) else {
        return false;
    };
    match Flock::lock(file, FlockArg::LockSharedNonblock) {
        Ok(lock) => {
            drop(lock);
            false
        }
        Err((_, e)) => e == Errno::EWOULDBLOCK,
    }
}

/// Attempts and pause of [`holder_pid`]: a server that just took the lock rewrites the pid
/// right after.
const HOLDER_ATTEMPTS: u32 = 5;
const HOLDER_PAUSE: Duration = Duration::from_millis(20);

/// The pid of the process holding `serve.lock`: the pid in the file, confirmed by the
/// kernel's lock table (`/proc/locks`) to hold the exclusive flock on that very file. A
/// stale pid (the previous server's, before the new one rewrote it) is never returned.
pub fn holder_pid(runtime_dir: &Path) -> Option<u32> {
    use std::os::unix::fs::MetadataExt;
    let path = lock_path(runtime_dir);
    for attempt in 1..=HOLDER_ATTEMPTS {
        if !is_held(runtime_dir) {
            return None;
        }
        let md = fs::metadata(&path).ok()?;
        let mut s = String::new();
        File::open(&path).ok()?.read_to_string(&mut s).ok()?;
        let locks = fs::read_to_string("/proc/locks").ok()?;
        if let Ok(pid) = s.trim().parse()
            && flock_writers(&locks, &lock_file_devs(&path, md.dev()), md.ino()).contains(&pid)
        {
            return Some(pid);
        }
        if attempt < HOLDER_ATTEMPTS {
            thread::sleep(HOLDER_PAUSE);
        }
    }
    None
}

/// Candidate devices the kernel's lock table (`/proc/locks`) may key a flock on `path`
/// under: `st_dev`, plus the device of the mount containing the file as mountinfo names
/// it. The two differ on btrfs — `st_dev` is the (nested) subvolume's anonymous device
/// while the lock table prints the mount's superblock device — and on ext4 or tmpfs they
/// are the same, so both must be tried. Best effort: without mountinfo, `st_dev` only.
fn lock_file_devs(path: &Path, st_dev: u64) -> Vec<u64> {
    let mut devs = vec![st_dev];
    let Ok(file) = fs::canonicalize(path) else {
        return devs;
    };
    let Ok(mounts) = fs::read_to_string("/proc/self/mountinfo") else {
        return devs;
    };
    if let Some(dev) = mount_dev(&mounts, &file)
        && dev != 0
        && !devs.contains(&dev)
    {
        devs.push(dev);
    }
    devs
}

/// The device (mountinfo field 3) of the mount whose mount point is the longest
/// component-wise prefix of `file`: the mount that contains it. A mount hidden behind
/// another mount's root defeats path matching; callers then fall back to `st_dev`.
fn mount_dev(mounts: &str, file: &Path) -> Option<u64> {
    let file = file.to_str()?;
    let (_, dev) = mounts
        .lines()
        .filter_map(|line| {
            let f: Vec<&str> = line.split_whitespace().collect();
            if f.len() < 5 || !f[4].starts_with('/') {
                return None;
            }
            (f[4] == "/" || file.starts_with(f[4]) && file[f[4].len()..].starts_with('/')).then(|| (f[4], f[2]))
        })
        .max_by_key(|(mp, _)| mp.len())?;
    let (major, minor) = dev.split_once(':')?;
    let d = libc::makedev(major.parse().ok()?, minor.parse().ok()?);
    (d != 0).then_some(d)
}

/// Pids holding an exclusive flock on a file with inode `ino` on any of `devs`, from
/// `/proc/locks` lines like `4: FLOCK  ADVISORY  WRITE 2394 00:21:1783682 0 EOF` (major and
/// minor in hex; waiters, marked `->`, are skipped). A file has more than one candidate
/// device on btrfs; see [`lock_file_devs`].
fn flock_writers(locks: &str, devs: &[u64], ino: u64) -> Vec<u32> {
    let want: Vec<(u64, u64)> = devs
        .iter()
        .map(|d| (u64::from(libc::major(*d)), u64::from(libc::minor(*d))))
        .collect();
    locks
        .lines()
        .filter_map(|line| {
            let f: Vec<&str> = line.split_whitespace().collect();
            if f.len() < 6 || f[1] != "FLOCK" || f[3] != "WRITE" {
                return None;
            }
            let mut id = f[5].split(':');
            let major = u64::from_str_radix(id.next()?, 16).ok()?;
            let minor = u64::from_str_radix(id.next()?, 16).ok()?;
            let inode = id.next()?.parse::<u64>().ok()?;
            (want.contains(&(major, minor)) && inode == ino)
                .then(|| f[4].parse().ok())
                .flatten()
        })
        .collect()
}

/// What `serve --ensure` does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EnsureAction {
    AlreadyRunning,
    /// `server.autostart = "off"`.
    Disabled,
    /// `server.autostart = "systemd"` but no unit file is installed.
    NoUnit,
    /// `systemctl [--user] start <unit>`; `fallback` starts a detached server if that fails.
    Systemd {
        fallback: bool,
    },
    Detached,
}

pub fn decide(held: bool, autostart: Autostart, unit_installed: bool) -> EnsureAction {
    match (held, autostart, unit_installed) {
        (true, ..) => EnsureAction::AlreadyRunning,
        (_, Autostart::Off, _) => EnsureAction::Disabled,
        (_, Autostart::Systemd, false) => EnsureAction::NoUnit,
        (_, Autostart::Systemd, true) => EnsureAction::Systemd { fallback: false },
        (_, Autostart::Auto, true) => EnsureAction::Systemd { fallback: true },
        (_, Autostart::Auto | Autostart::Shell, _) => EnsureAction::Detached,
    }
}

/// Starts `exe args…` as a detached daemon: new session, stdio on /dev/null, no inherited
/// descriptors, nice 10 and idle I/O priority (what the unit sets with `Nice=` and
/// `IOSchedulingClass=`). The child is not waited for; it is reparented to init when we
/// exit.
pub fn spawn_detached(exe: &Path, args: &[&str]) -> io::Result<u32> {
    let mut cmd = Command::new(exe);
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .current_dir("/");
    // SAFETY: the closure runs between fork and exec and only makes async-signal-safe
    // syscalls (setsid, close_range, nice, ioprio_set); it allocates nothing.
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() < 0 {
                return Err(io::Error::last_os_error());
            }
            // Descriptors of the caller (a recorder's PTY, its nonce fd) must not stay open
            // in a long-lived server: close_range(3, ~0, CLOSE_RANGE_CLOEXEC), best effort.
            // CLOEXEC instead of closing keeps std's exec-error pipe working.
            libc::syscall(libc::SYS_close_range, 3u32, u32::MAX, 1u32 << 2);
            libc::nice(10);
            // ioprio_set(IOPRIO_WHO_PROCESS, self, IOPRIO_CLASS_IDLE << 13); best effort.
            libc::syscall(libc::SYS_ioprio_set, 1, 0, 3 << 13);
            Ok(())
        });
    }
    Ok(cmd.spawn()?.id())
}

/// The binary to start a detached server from: the running executable, or `/proc/self/exe`
/// when it has been replaced on disk (an upgrade while this process runs).
pub fn self_exe() -> PathBuf {
    std::env::current_exe()
        .ok()
        .filter(|p| p.is_file())
        .unwrap_or_else(|| PathBuf::from("/proc/self/exe"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lock_is_exclusive_and_records_the_pid() {
        let dir = tempfile::tempdir().unwrap();
        let run = dir.path().join("run/cinelog");
        assert!(!is_held(&run));
        assert_eq!(holder_pid(&run), None);
        let Acquire::Acquired(lock) = ServeLock::acquire(&run).unwrap() else {
            panic!("first acquire must succeed")
        };
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(fs::metadata(&run).unwrap().permissions().mode() & 0o777, 0o700);
        assert_eq!(fs::metadata(lock.path()).unwrap().permissions().mode() & 0o777, 0o600);
        assert!(is_held(&run));
        assert_eq!(holder_pid(&run), Some(std::process::id()));
        match ServeLock::acquire(&run).unwrap() {
            Acquire::Held { pid } => assert_eq!(pid, Some(std::process::id())),
            Acquire::Acquired(_) => panic!("second acquire must fail"),
        }
        drop(lock);
        assert!(!is_held(&run));
        assert!(matches!(ServeLock::acquire(&run).unwrap(), Acquire::Acquired(_)));
    }

    #[test]
    fn a_stale_pid_in_the_lock_file_is_not_reported() {
        let dir = tempfile::tempdir().unwrap();
        let Acquire::Acquired(lock) = ServeLock::acquire(dir.path()).unwrap() else {
            panic!("acquire")
        };
        // What a new server's file holds between taking the lock and rewriting it.
        fs::write(lock.path(), "1\n").unwrap();
        assert_eq!(holder_pid(dir.path()), None);
        fs::write(lock.path(), format!("{}\n", std::process::id())).unwrap();
        assert_eq!(holder_pid(dir.path()), Some(std::process::id()));
    }

    #[test]
    fn proc_locks_lines_are_matched_by_device_and_inode() {
        let dev = libc::makedev(0, 0x21);
        let table = "1: POSIX  ADVISORY  WRITE 63312 00:21:164952 0 EOF\n\
                     2: FLOCK  ADVISORY  READ 77 00:21:164952 0 EOF\n\
                     3: FLOCK  ADVISORY  WRITE 42 00:21:164952 0 EOF\n\
                     3: -> FLOCK  ADVISORY  WRITE 43 00:21:164952 0 EOF\n\
                     4: FLOCK  ADVISORY  WRITE 44 00:22:164952 0 EOF\n\
                     5: FLOCK  ADVISORY  WRITE 45 00:21:164953 0 EOF\n";
        assert_eq!(flock_writers(table, &[dev], 164_952), [42]);
        // btrfs: st_dev is the subvolume's anonymous device (0:24), which /proc/locks
        // never prints for this file; the mount's device (0:33 decimal = 00:21 hex) is.
        let st_dev = libc::makedev(0, 0x24);
        let mount_dev = libc::makedev(0, 33);
        assert!(flock_writers(table, &[st_dev], 164_952).is_empty());
        assert_eq!(flock_writers(table, &[st_dev, mount_dev], 164_952), [42]);
        assert!(flock_writers("garbage\n", &[dev], 1).is_empty());
    }

    #[test]
    fn the_mounts_device_is_found_by_longest_mount_point_prefix() {
        let mounts = "22 1 0:33 / / rw - btrfs /dev/mapper/rootbase rw\n\
                      23 22 0:38 / /tmp rw - tmpfs none rw\n\
                      24 22 0:39 /@/home /home rw - btrfs /dev/mapper/rootbase rw\n";
        assert_eq!(
            mount_dev(mounts, Path::new("/home/user/run")),
            Some(libc::makedev(0, 39))
        );
        assert_eq!(mount_dev(mounts, Path::new("/tmp/a")), Some(libc::makedev(0, 38)));
        // A path that merely starts with the mount point's text does not match it.
        assert_eq!(mount_dev(mounts, Path::new("/homey/a")), Some(libc::makedev(0, 33)));
        assert_eq!(mount_dev("garbage\n", Path::new("/x")), None);
    }

    #[test]
    fn a_probe_does_not_make_a_starting_server_give_up() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(lock_path(dir.path()), b"").unwrap();
        let probe = Flock::lock(File::open(lock_path(dir.path())).unwrap(), FlockArg::LockShared).unwrap();
        let release = thread::spawn(move || {
            thread::sleep(Duration::from_millis(60));
            drop(probe);
        });
        assert!(matches!(ServeLock::acquire(dir.path()).unwrap(), Acquire::Acquired(_)));
        release.join().unwrap();
    }

    #[test]
    fn ensure_decisions() {
        use Autostart::*;
        assert_eq!(decide(true, Off, true), EnsureAction::AlreadyRunning);
        assert_eq!(decide(false, Off, true), EnsureAction::Disabled);
        assert_eq!(decide(false, Systemd, false), EnsureAction::NoUnit);
        assert_eq!(decide(false, Systemd, true), EnsureAction::Systemd { fallback: false });
        assert_eq!(decide(false, Auto, true), EnsureAction::Systemd { fallback: true });
        assert_eq!(decide(false, Auto, false), EnsureAction::Detached);
        assert_eq!(decide(false, Shell, true), EnsureAction::Detached);
    }

    #[test]
    fn detached_child_runs_in_its_own_session() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("sid");
        // An inheritable descriptor, as a recorder might leak one.
        let leaked = File::open(dir.path()).unwrap();
        // SAFETY: dup of a valid fd; the copy is closed below.
        let fd = unsafe { libc::dup(std::os::fd::AsRawFd::as_raw_fd(&leaked)) };
        assert!(fd > 2);
        let o = out.display();
        let script = format!(
            "ps -o sid= -p $$ > '{o}'; ps -o ni= -p $$ >> '{o}'; [ -e /proc/$$/fd/{fd} ] && echo leaked >> '{o}'; true"
        );
        let pid = spawn_detached(Path::new("/bin/sh"), &["-c", &script]).unwrap();
        // SAFETY: closes our own dup.
        unsafe { libc::close(fd) };
        // The child is ours until it exits; reap it so no zombie is left behind.
        nix::sys::wait::waitpid(nix::unistd::Pid::from_raw(pid as i32), None).unwrap();
        let s = fs::read_to_string(&out).unwrap();
        let mut lines = s.lines().map(str::trim);
        assert_eq!(lines.next(), Some(pid.to_string().as_str()), "session leader");
        let nice: i32 = lines.next().unwrap().parse().unwrap();
        assert!(nice >= 10, "nice {nice}");
        assert_eq!(lines.next(), None, "fd {fd} was inherited");
    }
}
