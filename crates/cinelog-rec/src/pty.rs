//! PTY, fd and child-process plumbing (plan §5.1).

use std::ffi::{CString, OsStr, OsString};
use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd, RawFd};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::sync::atomic::{AtomicBool, Ordering};

use nix::sys::termios::{self, LocalFlags, SetArg, Termios};

use crate::input::TermFlags;

pub(crate) type Winsize = libc::winsize;

pub(crate) const DEFAULT_WINSIZE: Winsize = Winsize {
    ws_row: 24,
    ws_col: 80,
    ws_xpixel: 0,
    ws_ypixel: 0,
};

pub(crate) fn winsize(fd: RawFd) -> Option<Winsize> {
    let mut ws = DEFAULT_WINSIZE;
    // SAFETY: TIOCGWINSZ writes a winsize into the provided struct.
    let rc = unsafe { libc::ioctl(fd, libc::TIOCGWINSZ, &mut ws) };
    (rc == 0 && ws.ws_col > 0 && ws.ws_row > 0).then_some(ws)
}

pub(crate) fn set_winsize(fd: RawFd, ws: &Winsize) {
    // SAFETY: TIOCSWINSZ reads a winsize from the provided struct.
    unsafe { libc::ioctl(fd, libc::TIOCSWINSZ, ws) };
}

pub(crate) fn set_nonblocking(fd: RawFd) -> io::Result<()> {
    // SAFETY: plain fcntl calls on a valid fd.
    unsafe {
        let fl = libc::fcntl(fd, libc::F_GETFL);
        if fl < 0 || libc::fcntl(fd, libc::F_SETFL, fl | libc::O_NONBLOCK) < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

pub(crate) fn set_cloexec(fd: RawFd) -> io::Result<()> {
    // SAFETY: plain fcntl call on a valid fd.
    if unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

pub(crate) fn is_tty(fd: RawFd) -> bool {
    // SAFETY: isatty only inspects the fd.
    unsafe { libc::isatty(fd) == 1 }
}

/// Echo/canonical flags of the slave, read through the master (Linux `tty_mode_ioctl`).
pub(crate) fn term_flags(master: BorrowedFd<'_>) -> TermFlags {
    termios::tcgetattr(master).map_or(TermFlags::UNKNOWN, |t| TermFlags {
        echo: t.local_flags.contains(LocalFlags::ECHO),
        icanon: t.local_flags.contains(LocalFlags::ICANON),
    })
}

/// Waits until `fd` is writable or 100 ms passed.
fn wait_writable(fd: RawFd) {
    let mut p = libc::pollfd {
        fd,
        events: libc::POLLOUT,
        revents: 0,
    };
    // SAFETY: one valid pollfd.
    unsafe { libc::poll(&mut p, 1, 100) };
}

/// Writes all of `buf`, waiting on EAGAIN (the host fd may be shared non-blocking).
/// Gives up when `stop` is set.
pub(crate) fn write_all(fd: RawFd, mut buf: &[u8], stop: &AtomicBool) -> io::Result<()> {
    while !buf.is_empty() {
        // SAFETY: buf is a valid readable slice.
        let n = unsafe { libc::write(fd, buf.as_ptr().cast(), buf.len()) };
        if n < 0 {
            let e = io::Error::last_os_error();
            match e.raw_os_error() {
                Some(libc::EINTR) => continue,
                Some(libc::EAGAIN) => {
                    if stop.load(Ordering::Relaxed) {
                        return Err(e);
                    }
                    wait_writable(fd);
                    continue;
                }
                _ => return Err(e),
            }
        }
        buf = &buf[n as usize..];
    }
    Ok(())
}

/// Waits until `fd` is writable; `false` once `stop` is set and it still is not.
fn writable_or_stop(fd: RawFd, stop: &AtomicBool) -> bool {
    loop {
        let stopping = stop.load(Ordering::Relaxed);
        let mut p = libc::pollfd {
            fd,
            events: libc::POLLOUT,
            revents: 0,
        };
        // SAFETY: one valid pollfd.
        let n = unsafe { libc::poll(&mut p, 1, if stopping { 0 } else { 100 }) };
        // POLLERR/POLLHUP: the write that follows reports the error.
        if n > 0 && p.revents != 0 {
            return true;
        }
        if stopping {
            return false;
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HostMode {
    /// Private non-blocking open file description: EAGAIN, then POLLOUT or `stop`.
    NonBlocking,
    /// Shared blocking fd that may stall (a tty or pipe we could not reopen): POLLOUT
    /// before every write of at most `PIPE_BUF` bytes, so a stalled reader blocks at worst
    /// one short write.
    Guarded,
    /// Regular file or similar: writes never wait for a reader.
    Plain,
}

/// Where the recorder writes terminal output on the host side (§5.1).
///
/// The host fd is shared with the parent (often blocking), and setting `O_NONBLOCK` on it
/// would change it for everyone. A tty or pipe is therefore reopened through
/// `/proc/self/fd`, which yields a new open file description that can be non-blocking on
/// its own. A host reader that stops reading (a frozen terminal emulator, a stopped outer
/// recorder) then never keeps the recorder from shutting down.
#[derive(Debug)]
pub(crate) struct HostOut {
    fd: RawFd,
    _own: Option<OwnedFd>,
    mode: HostMode,
}

fn file_id(fd: RawFd) -> Option<(libc::mode_t, libc::dev_t, libc::ino_t, libc::dev_t)> {
    // SAFETY: fstat writes into the zeroed struct.
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    (unsafe { libc::fstat(fd, &mut st) } == 0).then_some((st.st_mode & libc::S_IFMT, st.st_dev, st.st_ino, st.st_rdev))
}

impl HostOut {
    pub(crate) fn open(fd: RawFd) -> Self {
        let Some(id @ (kind, ..)) = file_id(fd) else {
            return Self::shared(fd, HostMode::Guarded);
        };
        if kind != libc::S_IFCHR && kind != libc::S_IFIFO {
            return Self::shared(fd, HostMode::Plain);
        }
        match reopen_nonblocking(fd) {
            Some(own) if file_id(own.as_raw_fd()) == Some(id) => Self {
                fd: own.as_raw_fd(),
                _own: Some(own),
                mode: HostMode::NonBlocking,
            },
            _ => Self::shared(fd, HostMode::Guarded),
        }
    }

    fn shared(fd: RawFd, mode: HostMode) -> Self {
        Self { fd, _own: None, mode }
    }

    /// Writes all of `buf`; gives up with an error once `stop` is set and the host does
    /// not accept more.
    pub(crate) fn write_all(&self, mut buf: &[u8], stop: &AtomicBool) -> io::Result<()> {
        match self.mode {
            HostMode::NonBlocking | HostMode::Plain => write_all(self.fd, buf, stop),
            HostMode::Guarded => {
                while !buf.is_empty() {
                    if !writable_or_stop(self.fd, stop) {
                        return Err(io::Error::new(io::ErrorKind::TimedOut, "host output stalled"));
                    }
                    let chunk = &buf[..buf.len().min(libc::PIPE_BUF)];
                    // SAFETY: chunk is a valid readable slice.
                    let n = unsafe { libc::write(self.fd, chunk.as_ptr().cast(), chunk.len()) };
                    if n < 0 {
                        let e = io::Error::last_os_error();
                        if matches!(e.raw_os_error(), Some(libc::EINTR | libc::EAGAIN)) {
                            continue;
                        }
                        return Err(e);
                    }
                    buf = &buf[n as usize..];
                }
                Ok(())
            }
        }
    }
}

fn reopen_nonblocking(fd: RawFd) -> Option<OwnedFd> {
    let path = CString::new(format!("/proc/self/fd/{fd}")).ok()?;
    let flags = libc::O_WRONLY | libc::O_NONBLOCK | libc::O_NOCTTY | libc::O_CLOEXEC;
    // SAFETY: valid C string; the returned fd is owned below.
    let new = unsafe { libc::open(path.as_ptr(), flags) };
    // SAFETY: a fresh fd nobody else owns.
    (new >= 0).then(|| unsafe { <OwnedFd as std::os::fd::FromRawFd>::from_raw_fd(new) })
}

/// `read(2)` into `buf`; `Ok(0)` is EOF.
pub(crate) fn read(fd: RawFd, buf: &mut [u8]) -> io::Result<usize> {
    // SAFETY: buf is a valid writable slice.
    let n = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) };
    if n < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(n as usize)
    }
}

/// Puts the host terminal into raw mode and restores it (TCSAFLUSH) on drop.
pub(crate) struct RawGuard {
    fd: RawFd,
    orig: Termios,
}

impl RawGuard {
    pub(crate) fn new(fd: RawFd, orig: Termios) -> io::Result<Self> {
        let mut raw = orig.clone();
        termios::cfmakeraw(&mut raw);
        // SAFETY: fd stays open for the guard's lifetime (it is the host stdin).
        let bfd = unsafe { BorrowedFd::borrow_raw(fd) };
        termios::tcsetattr(bfd, SetArg::TCSANOW, &raw).map_err(io::Error::from)?;
        Ok(Self { fd, orig })
    }
}

impl Drop for RawGuard {
    fn drop(&mut self) {
        // SAFETY: see `new`; SIGTTOU is ignored by the recorder, so this cannot stop us
        // even if we lost the foreground.
        let bfd = unsafe { BorrowedFd::borrow_raw(self.fd) };
        let _ = termios::tcsetattr(bfd, SetArg::TCSAFLUSH, &self.orig);
    }
}

/// Everything the child needs, prepared before `fork` (only async-signal-safe calls follow).
#[derive(Debug)]
pub(crate) struct ChildSpec {
    path: CString,
    argv: Vec<CString>,
    envp: Vec<CString>,
}

/// `K=V` pairs of `vars` with every `CINELOG_*` removed and `extra` appended.
pub(crate) fn child_env(
    vars: impl IntoIterator<Item = (OsString, OsString)>,
    extra: &[(&str, String)],
) -> Vec<CString> {
    let mut out: Vec<CString> = vars
        .into_iter()
        .filter(|(k, _)| !k.as_bytes().starts_with(b"CINELOG_") && !k.is_empty())
        .filter_map(|(k, v)| {
            let mut kv = k.into_vec();
            kv.push(b'=');
            kv.extend_from_slice(v.as_bytes());
            CString::new(kv).ok()
        })
        .collect();
    out.extend(extra.iter().filter_map(|(k, v)| CString::new(format!("{k}={v}")).ok()));
    out
}

impl ChildSpec {
    /// `argv[0]` is the shell's basename (`zsh`), so `$0` stays `zsh` (§5.1).
    pub(crate) fn new(shell: &OsStr, args: &[OsString], envp: Vec<CString>) -> io::Result<Self> {
        let invalid = |_| io::Error::new(io::ErrorKind::InvalidInput, "NUL in shell path or argument");
        let path = CString::new(shell.as_bytes()).map_err(invalid)?;
        let base = std::path::Path::new(shell)
            .file_name()
            .map_or_else(|| b"sh".to_vec(), |n| n.as_bytes().to_vec());
        let mut argv = vec![CString::new(base).map_err(invalid)?];
        for a in args {
            argv.push(CString::new(a.as_bytes()).map_err(invalid)?);
        }
        Ok(Self { path, argv, envp })
    }
}

/// Forks the shell on `slave` as its controlling terminal. `keep_fd` (the nonce pipe) is
/// inherited across exec; everything else the recorder opened is close-on-exec.
pub(crate) fn spawn(spec: &ChildSpec, slave: &OwnedFd, master: &OwnedFd, keep_fd: RawFd) -> io::Result<u32> {
    let argv: Vec<*const libc::c_char> = spec
        .argv
        .iter()
        .map(|a| a.as_ptr())
        .chain(std::iter::once(std::ptr::null()))
        .collect();
    let envp: Vec<*const libc::c_char> = spec
        .envp
        .iter()
        .map(|a| a.as_ptr())
        .chain(std::iter::once(std::ptr::null()))
        .collect();
    let (s, m) = (slave.as_raw_fd(), master.as_raw_fd());
    // SAFETY: after fork the child only calls async-signal-safe functions on data prepared
    // above, then execs or _exits.
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        return Err(io::Error::last_os_error());
    }
    if pid == 0 {
        // SAFETY: child side, see above.
        unsafe { child_exec(&spec.path, &argv, &envp, s, m, keep_fd) }
    }
    Ok(pid as u32)
}

/// # Safety
/// Must only be called in a freshly forked child.
unsafe fn child_exec(
    path: &CString,
    argv: &[*const libc::c_char],
    envp: &[*const libc::c_char],
    slave: RawFd,
    master: RawFd,
    keep_fd: RawFd,
) -> ! {
    // SAFETY: async-signal-safe libc calls only.
    unsafe {
        libc::setsid();
        libc::ioctl(slave, libc::TIOCSCTTY, 0);
        for fd in 0..3 {
            libc::dup2(slave, fd);
        }
        if slave > 2 {
            libc::close(slave);
        }
        libc::close(master);
        if keep_fd >= 0 {
            libc::fcntl(keep_fd, libc::F_SETFD, 0);
        }
        // Ignored dispositions survive exec; the shell must start with defaults.
        for sig in [
            libc::SIGPIPE,
            libc::SIGTSTP,
            libc::SIGTTIN,
            libc::SIGTTOU,
            libc::SIGINT,
            libc::SIGQUIT,
        ] {
            libc::signal(sig, libc::SIG_DFL);
        }
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        libc::sigprocmask(libc::SIG_SETMASK, &set, std::ptr::null_mut());
        libc::execve(path.as_ptr(), argv.as_ptr(), envp.as_ptr());
        libc::_exit(127)
    }
}

/// Pipe whose read end carries the nonce line to the shell (§4.3): the line is written and
/// the write end closed before the fork, so only the read end is ever inherited.
pub(crate) fn nonce_pipe(line: &[u8]) -> io::Result<OwnedFd> {
    let (r, w) = nix::unistd::pipe2(nix::fcntl::OFlag::O_CLOEXEC).map_err(io::Error::from)?;
    let stop = AtomicBool::new(true);
    write_all(w.as_raw_fd(), line, &stop)?;
    drop(w);
    Ok(r)
}

/// Non-blocking close-on-exec pipe used to wake a poll loop.
pub(crate) fn wake_pipe() -> io::Result<(OwnedFd, OwnedFd)> {
    let (r, w) =
        nix::unistd::pipe2(nix::fcntl::OFlag::O_CLOEXEC | nix::fcntl::OFlag::O_NONBLOCK).map_err(io::Error::from)?;
    Ok((r, w))
}

/// Reads and discards everything currently in a non-blocking fd.
pub(crate) fn drain(fd: BorrowedFd<'_>) {
    let mut buf = [0u8; 64];
    while matches!(read(fd.as_fd().as_raw_fd(), &mut buf), Ok(n) if n > 0) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn child_env_strips_cinelog_vars() {
        let vars = [
            ("PATH", "/bin"),
            ("CINELOG_FAILED", "x"),
            ("CINELOG_NONCE_FD", "9"),
            ("TERM", "xterm"),
        ]
        .map(|(k, v)| (OsString::from(k), OsString::from(v)));
        let env = child_env(vars, &[("CINELOG_SESSION", "u".into())]);
        let env: Vec<_> = env.iter().map(|c| c.to_str().unwrap().to_owned()).collect();
        assert_eq!(env, ["PATH=/bin", "TERM=xterm", "CINELOG_SESSION=u"]);
    }

    #[test]
    fn argv0_is_the_basename() {
        let s = ChildSpec::new("/usr/bin/zsh".as_ref(), &["-l".into()], Vec::new()).unwrap();
        assert_eq!(s.argv[0].to_str().unwrap(), "zsh");
        assert_eq!(s.argv[1].to_str().unwrap(), "-l");
        assert!(ChildSpec::new("/bin/z\0sh".as_ref(), &[], Vec::new()).is_err());
    }

    /// A host reader that never reads must not block a write once `stop` is set
    /// (regression: shutdown joined an output thread stuck in a blocking write(2)).
    fn assert_gives_up(out: HostOut) {
        let stop = std::sync::Arc::new(AtomicBool::new(false));
        let (tx, rx) = std::sync::mpsc::channel();
        let s = std::sync::Arc::clone(&stop);
        let h = std::thread::spawn(move || {
            let r = out.write_all(&vec![b'x'; 4 << 20], &s);
            let _ = tx.send(r.is_err());
        });
        std::thread::sleep(std::time::Duration::from_millis(150));
        assert!(rx.try_recv().is_err(), "still waiting for the reader");
        stop.store(true, Ordering::SeqCst);
        let gave_up = rx
            .recv_timeout(std::time::Duration::from_secs(3))
            .expect("write_all returned after stop");
        assert!(gave_up);
        h.join().unwrap();
    }

    #[test]
    fn stalled_tty_host_is_reopened_non_blocking() {
        let p = nix::pty::openpty(None, None).unwrap();
        set_cloexec(p.master.as_raw_fd()).unwrap();
        set_cloexec(p.slave.as_raw_fd()).unwrap();
        let out = HostOut::open(p.slave.as_raw_fd());
        assert_eq!(out.mode, HostMode::NonBlocking);
        // SAFETY: plain fcntl on a valid fd.
        let shared_fl = unsafe { libc::fcntl(p.slave.as_raw_fd(), libc::F_GETFL) };
        assert_eq!(shared_fl & libc::O_NONBLOCK, 0, "the shared fd stays blocking");
        assert_gives_up(out);
    }

    #[test]
    fn stalled_pipe_host_gives_up_in_both_modes() {
        let (r, w) = nix::unistd::pipe2(nix::fcntl::OFlag::O_CLOEXEC).unwrap();
        let out = HostOut::open(w.as_raw_fd());
        assert_eq!(out.mode, HostMode::NonBlocking);
        assert_gives_up(out);
        // The fallback when no private open is possible.
        assert_gives_up(HostOut::shared(w.as_raw_fd(), HostMode::Guarded));
        drop(r);
    }

    #[test]
    fn host_output_reaches_the_reader_and_files_are_not_reopened() {
        let (r, w) = nix::unistd::pipe2(nix::fcntl::OFlag::O_CLOEXEC).unwrap();
        let stop = AtomicBool::new(true);
        HostOut::open(w.as_raw_fd()).write_all(b"hello", &stop).unwrap();
        HostOut::shared(w.as_raw_fd(), HostMode::Guarded)
            .write_all(b"!", &stop)
            .unwrap();
        let mut buf = [0u8; 16];
        assert_eq!(read(r.as_raw_fd(), &mut buf).unwrap(), 6);
        assert_eq!(&buf[..6], b"hello!");
        let f = tempfile::tempfile().unwrap();
        let out = HostOut::open(f.as_raw_fd());
        assert_eq!(out.mode, HostMode::Plain, "a reopen would rewind a regular file");
        out.write_all(b"abc", &stop).unwrap();
        assert_eq!(f.metadata().unwrap().len(), 3);
    }

    #[test]
    fn nonce_pipe_delivers_one_line_then_eof() {
        let r = nonce_pipe(b"abc\n").unwrap();
        let mut buf = [0u8; 16];
        assert_eq!(read(r.as_raw_fd(), &mut buf).unwrap(), 4);
        assert_eq!(read(r.as_raw_fd(), &mut buf).unwrap(), 0, "write end is closed");
    }
}
