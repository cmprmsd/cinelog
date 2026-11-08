//! tty identity of a file descriptor (§5.2 step 1) and the foreground pgrp of a master.

use std::ffi::{CStr, OsStr};
use std::mem::MaybeUninit;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;

use crate::{Pid, TtyIdentity};

/// Majors of Unix98 pty slaves (`/dev/pts/*`).
const PTS_MAJORS: std::ops::RangeInclusive<u32> = 136..=143;

/// pts index of a device number, if it is a Unix98 pty slave.
pub fn pts_index_of_rdev(rdev: u64) -> Option<u32> {
    let (major, minor) = (libc::major(rdev), libc::minor(rdev));
    PTS_MAJORS
        .contains(&major)
        .then(|| (major - PTS_MAJORS.start()) * 256 + minor)
}

/// The tty on `fd`: a pts index from `st_rdev`, another tty by device path, or not a tty.
pub fn tty_of(fd: impl AsFd) -> TtyIdentity {
    tty_of_fd(fd.as_fd().as_raw_fd())
}

/// [`tty_of`] for a raw descriptor number such as `0`. Only side-effect-free metadata
/// queries (`isatty`, `fstat`, `ttyname_r`) touch `fd`, and a closed or invalid number
/// yields [`TtyIdentity::NotATty`]; no borrowed or owned fd is ever made from it.
pub fn tty_of_fd(fd: RawFd) -> TtyIdentity {
    if fd < 0 {
        return TtyIdentity::NotATty;
    }
    // SAFETY: isatty takes a plain int and reports EBADF/ENOTTY for anything else.
    if unsafe { libc::isatty(fd) } != 1 {
        return TtyIdentity::NotATty;
    }
    let mut st = MaybeUninit::<libc::stat>::uninit();
    // SAFETY: st is a valid out-pointer; the struct is only read after fstat succeeded.
    let rdev = unsafe {
        if libc::fstat(fd, st.as_mut_ptr()) != 0 {
            return TtyIdentity::NotATty;
        }
        st.assume_init().st_rdev
    };
    if let Some(n) = pts_index_of_rdev(rdev) {
        return TtyIdentity::Pts(n);
    }
    let path =
        ttyname(fd).unwrap_or_else(|| PathBuf::from(format!("/dev/char/{}:{}", libc::major(rdev), libc::minor(rdev))));
    TtyIdentity::Other(path)
}

fn ttyname(fd: RawFd) -> Option<PathBuf> {
    let mut buf = vec![0u8; libc::PATH_MAX as usize];
    // SAFETY: buf is writable for buf.len() bytes; ttyname_r NUL-terminates on success.
    if unsafe { libc::ttyname_r(fd, buf.as_mut_ptr().cast(), buf.len()) } != 0 {
        return None;
    }
    let name = CStr::from_bytes_until_nul(&buf).ok()?;
    Some(PathBuf::from(OsStr::from_bytes(name.to_bytes())))
}

/// `tcgetpgrp(master)`: the foreground process group of the slave side (§5.3).
pub fn foreground_pgrp(master: BorrowedFd<'_>) -> nix::Result<Pid> {
    nix::unistd::tcgetpgrp(master).map(|p| p.as_raw() as Pid)
}

#[cfg(test)]
mod tests {
    use std::os::fd::AsRawFd;

    use super::*;

    #[test]
    fn rdev_mapping() {
        assert_eq!(pts_index_of_rdev(libc::makedev(136, 0)), Some(0));
        assert_eq!(pts_index_of_rdev(libc::makedev(136, 7)), Some(7));
        assert_eq!(pts_index_of_rdev(libc::makedev(136, 4100)), Some(4100));
        assert_eq!(pts_index_of_rdev(libc::makedev(137, 1)), Some(257));
        assert_eq!(pts_index_of_rdev(libc::makedev(4, 1)), None);
        assert_eq!(pts_index_of_rdev(libc::makedev(5, 0)), None);
    }

    #[test]
    fn live_pty_and_non_tty() {
        let pty = nix::pty::openpty(None, None).unwrap();
        let path = nix::unistd::ttyname(&pty.slave).unwrap();
        let n: u32 = path
            .to_str()
            .unwrap()
            .strip_prefix("/dev/pts/")
            .unwrap()
            .parse()
            .unwrap();
        assert_eq!(tty_of(&pty.slave), TtyIdentity::Pts(n));
        assert_eq!(tty_of_fd(pty.slave.as_raw_fd()), TtyIdentity::Pts(n));

        let file = tempfile::tempfile().unwrap();
        assert_eq!(tty_of(&file), TtyIdentity::NotATty);
        assert_eq!(tty_of_fd(file.as_raw_fd()), TtyIdentity::NotATty);
        assert_eq!(tty_of_fd(-1), TtyIdentity::NotATty);
        // Never open: an invalid number is only queried, never borrowed.
        assert_eq!(tty_of_fd(i32::MAX), TtyIdentity::NotATty);
    }
}
