//! Name mirroring (plan §5.7): tmux automatic-rename reads `/proc/<fg pgrp>/cmdline` of
//! the pane, which is now the recorder. The recorder rewrites its own argv area (the
//! setproctitle technique, bounded by the original argv size) to show the basename of
//! the inner foreground process's argv0. comm stays `cinelog`, so detection is unaffected.

/// The process's original argv strings, `[arg_start, arg_end)` from `/proc/self/stat`.
#[derive(Debug)]
pub(crate) struct ArgvArea {
    start: usize,
    len: usize,
    current: Vec<u8>,
}

/// Fields 48 and 49 (`arg_start`, `arg_end`) of a `/proc/<pid>/stat` line.
fn arg_bounds(stat: &str) -> Option<(usize, usize)> {
    let rest = &stat[stat.rfind(')')? + 1..];
    let mut f = rest.split_ascii_whitespace().skip(45);
    let start = f.next()?.parse().ok()?;
    let end = f.next()?.parse().ok()?;
    (end > start && start > 0).then_some((start, end))
}

/// What to show for an inner process: the basename of its argv0, keeping a login `-`.
pub(crate) fn display_name(cmdline: &[u8]) -> Option<&[u8]> {
    let argv0 = cmdline.split(|&b| b == 0).next().filter(|a| !a.is_empty())?;
    let base = match memchr::memrchr(b'/', argv0) {
        Some(i) if i + 1 < argv0.len() => &argv0[i + 1..],
        Some(_) => return None,
        None => argv0,
    };
    Some(base)
}

impl ArgvArea {
    /// The area of the running process; `None` if the kernel does not report it.
    pub(crate) fn of_self() -> Option<Self> {
        let stat = std::fs::read_to_string("/proc/self/stat").ok()?;
        let (start, end) = arg_bounds(&stat)?;
        Some(Self {
            start,
            len: end - start,
            current: Vec::new(),
        })
    }

    /// Replaces the visible argv with `name` (truncated) followed by NUL padding.
    pub(crate) fn set(&mut self, name: &[u8]) {
        let n = name.len().min(self.len - 1);
        if self.current == name[..n] {
            return;
        }
        // SAFETY: [start, start+len) is this process's own argv string area at the top of
        // the initial stack, mapped read-write for the life of the process. Only the main
        // thread writes it, and every byte written keeps it a sequence of NUL-terminated
        // strings, so any later reader of argv still sees valid C strings.
        unsafe {
            let p = self.start as *mut u8;
            std::ptr::copy_nonoverlapping(name.as_ptr(), p, n);
            std::ptr::write_bytes(p.add(n), 0, self.len - n);
        }
        self.current = name[..n].to_vec();
    }
}

/// argv0 basename of `pid`, from `/proc/<pid>/cmdline`.
pub(crate) fn name_of(pid: u32) -> Option<Vec<u8>> {
    let cmdline = std::fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    display_name(&cmdline).map(<[u8]>::to_vec)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stat_argv_bounds() {
        let own = std::fs::read_to_string("/proc/self/stat").unwrap();
        let (s, e) = arg_bounds(&own).expect("kernel reports arg_start/arg_end");
        assert!(e > s);
        let cmdline = std::fs::read("/proc/self/cmdline").unwrap();
        assert_eq!(cmdline.len(), e - s);
        assert_eq!(arg_bounds("1 (x) S 0"), None);
    }

    #[test]
    fn display_names() {
        assert_eq!(display_name(b"/usr/bin/vim\0file\0"), Some(&b"vim"[..]));
        assert_eq!(display_name(b"-zsh\0"), Some(&b"-zsh"[..]));
        assert_eq!(display_name(b"htop"), Some(&b"htop"[..]));
        assert_eq!(display_name(b""), None);
        assert_eq!(display_name(b"/usr/bin/\0"), None);
    }

    #[test]
    fn name_of_self() {
        let me = name_of(std::process::id()).unwrap();
        assert!(!me.is_empty() && !me.contains(&b'/'));
    }
}
