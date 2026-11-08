//! Recorder-assigned command sequence numbers (PROTOCOL.md §4.2).
//!
//! Every shell's `n` restarts at 1 (subshells, nested zsh), so `(sh, n)` is ambiguous on its
//! own; `seq` is unique per session and continues across rotation parts. A `done` closes
//! the most recent open `cmd` with the same `(sh, n)`.

use std::collections::VecDeque;

/// Open commands kept for matching; older ones (shells killed mid-command) are forgotten.
const MAX_OPEN: usize = 4096;

#[derive(Debug)]
pub(crate) struct SeqAssigner {
    next: u64,
    open: VecDeque<(u32, u64, u64)>,
}

impl Default for SeqAssigner {
    fn default() -> Self {
        Self {
            next: 1,
            open: VecDeque::new(),
        }
    }
}

impl SeqAssigner {
    pub(crate) fn on_cmd(&mut self, sh: u32, n: u64) -> u64 {
        let seq = self.next;
        self.next += 1;
        if self.open.len() == MAX_OPEN {
            self.open.pop_front();
        }
        self.open.push_back((sh, n, seq));
        seq
    }

    pub(crate) fn on_done(&mut self, sh: u32, n: u64) -> Option<u64> {
        let i = self.open.iter().rposition(|&(s, k, _)| s == sh && k == n)?;
        self.open.remove(i).map(|(_, _, seq)| seq)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interleaved_shells_get_distinct_seqs() {
        let mut s = SeqAssigner::default();
        // outer shell 100 runs `zsh` (n=1); inner shell 200 runs two commands; both start at 1.
        assert_eq!(s.on_cmd(100, 1), 1);
        assert_eq!(s.on_cmd(200, 1), 2);
        assert_eq!(s.on_done(200, 1), Some(2));
        assert_eq!(s.on_cmd(200, 2), 3);
        assert_eq!(s.on_done(200, 2), Some(3));
        assert_eq!(s.on_done(100, 1), Some(1));
        assert_eq!(s.on_done(100, 1), None, "each cmd is closed once");
        assert_eq!(s.on_done(300, 7), None, "unmatched done");
        assert_eq!(s.on_cmd(100, 2), 4);
    }

    #[test]
    fn pid_reuse_matches_the_most_recent_command() {
        let mut s = SeqAssigner::default();
        assert_eq!(s.on_cmd(5, 1), 1); // shell killed, never sends done
        assert_eq!(s.on_cmd(5, 1), 2); // new shell with the same pid
        assert_eq!(s.on_done(5, 1), Some(2));
    }

    #[test]
    fn open_set_is_bounded() {
        let mut s = SeqAssigner::default();
        for i in 0..(MAX_OPEN as u64 + 10) {
            s.on_cmd(1, i);
        }
        assert_eq!(s.open.len(), MAX_OPEN);
        assert_eq!(s.on_done(1, 0), None);
        assert!(s.on_done(1, MAX_OPEN as u64 + 9).is_some());
    }
}
