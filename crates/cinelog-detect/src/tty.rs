//! tty identity of a file descriptor (§5.2 step 1) and the foreground pgrp of a master.

use std::os::fd::{BorrowedFd, RawFd};
use std::path::PathBuf;

use nix::sys::stat::fstat;

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
pub fn tty_of_fd(fd: RawFd) -> TtyIdentity {
    if fd < 0 {
        return TtyIdentity::NotATty;
    }
    // SAFETY: fd is not -1 and is only borrowed for this call; a closed fd makes
    // isatty/fstat fail with EBADF, which is handled.
    let fd = unsafe { BorrowedFd::borrow_raw(fd) };
    if !nix::unistd::isatty(fd).unwrap_or(false) {
        return TtyIdentity::NotATty;
    }
    let Ok(st) = fstat(fd) else {
        return TtyIdentity::NotATty;
    };
    let rdev = st.st_rdev;
    if let Some(n) = pts_index_of_rdev(rdev) {
        return TtyIdentity::Pts(n);
    }
    let path = nix::unistd::ttyname(fd)
        .unwrap_or_else(|_| PathBuf::from(format!("/dev/char/{}:{}", libc::major(rdev), libc::minor(rdev))));
    TtyIdentity::Other(path)
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
        assert_eq!(tty_of_fd(pty.slave.as_raw_fd()), TtyIdentity::Pts(n));

        let file = tempfile::tempfile().unwrap();
        assert_eq!(tty_of_fd(file.as_raw_fd()), TtyIdentity::NotATty);
        assert_eq!(tty_of_fd(-1), TtyIdentity::NotATty);
    }
}
