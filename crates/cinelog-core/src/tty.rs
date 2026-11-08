//! The controlling terminal of the calling process, as a registry key (§6.5: `cinelog
//! pause/resume/link` and subshells find their registry entry through `/dev/tty`).

use std::os::fd::AsFd;
use std::path::PathBuf;

use crate::registry::TtyKey;

/// Unix98 pty slave majors.
const PTS_MAJORS: std::ops::RangeInclusive<u64> = 136..=143;
/// VTs (`tty0`–`tty63`) and the 8250 serial ports (`ttyS0`…, minors 64+).
const TTY_MAJOR: u64 = 4;

/// Splits a `tty_nr` / `st_rdev` value (Linux `new_encode_dev` layout).
pub const fn split_dev(dev: u64) -> (u64, u64) {
    let major = (dev >> 8) & 0xfff;
    let minor = (dev & 0xff) | ((dev >> 12) & 0xfff00);
    (major, minor)
}

/// Maps a device number to a registry key when its name follows from the number alone.
pub fn key_from_dev(dev: u64) -> Option<TtyKey> {
    let (major, minor) = split_dev(dev);
    if PTS_MAJORS.contains(&major) {
        let index = (major - PTS_MAJORS.start()) * 256 + minor;
        return u32::try_from(index).ok().map(TtyKey::Pts);
    }
    match (major, minor) {
        (TTY_MAJOR, 1..=63) => Some(TtyKey::Tty(format!("tty{minor}"))),
        (TTY_MAJOR, 64..=255) => Some(TtyKey::Tty(format!("ttyS{}", minor - 64))),
        _ => None,
    }
}

/// `tty_nr` (field 7) of a `/proc/<pid>/stat` line; 0 means no controlling tty.
pub fn tty_nr_from_stat(stat: &str) -> Option<u64> {
    // comm (field 2) may contain spaces and parentheses: fields restart after the last ')'.
    let rest = &stat[stat.rfind(')')? + 1..];
    rest.split_ascii_whitespace().nth(4)?.parse().ok()
}

/// Registry key of the controlling terminal, or `None` without one.
pub fn controlling_tty() -> Option<TtyKey> {
    let stat = std::fs::read_to_string("/proc/self/stat").ok()?;
    let dev = tty_nr_from_stat(&stat).filter(|&d| d != 0)?;
    key_from_dev(dev).or_else(|| named_std_tty(dev))
}

/// Path of the controlling terminal (`/dev/pts/N`, `/dev/tty1`), if any.
pub fn controlling_tty_path() -> Option<PathBuf> {
    controlling_tty().map(|k| PathBuf::from(k.to_string()))
}

/// Finds the name of an unusual tty (USB serial, console) through a std fd that is open on it.
fn named_std_tty(dev: u64) -> Option<TtyKey> {
    use std::os::unix::fs::MetadataExt;
    let fds = [
        std::io::stdin().as_fd().try_clone_to_owned().ok()?,
        std::io::stdout().as_fd().try_clone_to_owned().ok()?,
        std::io::stderr().as_fd().try_clone_to_owned().ok()?,
    ];
    fds.into_iter().find_map(|fd| {
        let file = std::fs::File::from(fd);
        (file.metadata().ok()?.rdev() == dev)
            .then(|| nix::unistd::ttyname(&file).ok())
            .flatten()
            .and_then(|p| TtyKey::from_path(&p))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dev(major: u64, minor: u64) -> u64 {
        (minor & 0xff) | (major << 8) | ((minor & !0xff) << 12)
    }

    #[test]
    fn pts_numbers() {
        assert_eq!(key_from_dev(dev(136, 0)), Some(TtyKey::Pts(0)));
        assert_eq!(key_from_dev(dev(136, 7)), Some(TtyKey::Pts(7)));
        // Large indexes stay on major 136 with an extended minor …
        assert_eq!(key_from_dev(dev(136, 300)), Some(TtyKey::Pts(300)));
        // … older layouts spread them over majors 137–143.
        assert_eq!(key_from_dev(dev(137, 4)), Some(TtyKey::Pts(260)));
    }

    #[test]
    fn vts_and_serial() {
        assert_eq!(key_from_dev(dev(4, 1)), Some(TtyKey::Tty("tty1".into())));
        assert_eq!(key_from_dev(dev(4, 64)), Some(TtyKey::Tty("ttyS0".into())));
        assert_eq!(key_from_dev(dev(4, 0)), None);
        assert_eq!(key_from_dev(dev(5, 0)), None);
    }

    #[test]
    fn stat_parsing_survives_odd_comms() {
        let line = "4242 (tmux: server) S 1 4242 4242 34823 4242 4194560 1 2 3";
        assert_eq!(tty_nr_from_stat(line), Some(34823));
        let line = "7 (a) b) (c) R 1 7 7 0 -1 0";
        assert_eq!(tty_nr_from_stat(line), Some(0));
        assert_eq!(tty_nr_from_stat("garbage"), None);
        assert_eq!(split_dev(34823), (136, 7));
    }
}
