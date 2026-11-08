//! The stack of open commands (plan §6.4 "Segmenter").
//!
//! - `cmd` pushes; a command already open from a different shell becomes the parent
//!   (`parent_seq`, `depth + 1`). An open command of the *same* shell lost its `done` (the
//!   shell was replaced or killed): it and everything above it are closed first.
//! - `done(sh, n)` pops down to the matching entry; entries above it are closed without
//!   their own `done` (exit NULL).
//! - Output always belongs to the innermost open command, or to the gap.

use crate::model::{OpenCommand, Owner, Seq};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Segmenter {
    stack: Vec<OpenCommand>,
}

/// What a `cmd` does to the stack.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Pushed {
    /// Closed without `done`, innermost first.
    pub closed: Vec<OpenCommand>,
    pub parent_seq: Option<Seq>,
    pub depth: u32,
}

/// What a matched `done` does to the stack.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Popped {
    /// Closed without `done`, innermost first.
    pub unmatched: Vec<OpenCommand>,
    pub matched: OpenCommand,
}

impl Segmenter {
    pub(crate) fn from_stack(stack: Vec<OpenCommand>) -> Self {
        Self { stack }
    }

    pub(crate) fn stack(&self) -> &[OpenCommand] {
        &self.stack
    }

    /// Owner of the output right now.
    pub(crate) fn owner(&self) -> Owner {
        self.stack.last().map_or(Owner::Gap, |e| Owner::Command(e.seq))
    }

    pub(crate) fn is_open(&self, seq: Seq) -> bool {
        self.stack.iter().any(|e| e.seq == seq)
    }

    /// Pushes a `cmd` of shell `sh`.
    pub(crate) fn push(&mut self, seq: Seq, sh: u32, n: u64) -> Pushed {
        let closed = match self.stack.iter().rposition(|e| e.sh == sh) {
            Some(i) => self.stack.split_off(i).into_iter().rev().collect(),
            None => Vec::new(),
        };
        let (parent_seq, depth) = self.stack.last().map_or((None, 0), |p| (Some(p.seq), p.depth + 1));
        self.stack.push(OpenCommand { seq, sh, n, depth });
        Pushed {
            closed,
            parent_seq,
            depth,
        }
    }

    /// Index of the entry a `done(sh, n)` closes: the innermost one with that `(sh, n)`,
    /// else the one with the recorder's matched `seq`.
    pub(crate) fn find(&self, sh: u32, n: u64, seq: Option<Seq>) -> Option<usize> {
        self.stack
            .iter()
            .rposition(|e| e.sh == sh && e.n == n)
            .or_else(|| seq.and_then(|s| self.stack.iter().rposition(|e| e.seq == s)))
    }

    /// Owner once the entry at `index` and everything above it are closed.
    pub(crate) fn owner_below(&self, index: usize) -> Owner {
        index
            .checked_sub(1)
            .map_or(Owner::Gap, |i| Owner::Command(self.stack[i].seq))
    }

    /// Pops the entry at `index` (from [`Self::find`]) and everything above it.
    pub(crate) fn pop(&mut self, index: usize) -> Popped {
        let mut above: Vec<OpenCommand> = self.stack.split_off(index);
        let matched = above.remove(0);
        above.reverse();
        Popped {
            unmatched: above,
            matched,
        }
    }

    /// Empties the stack, innermost first.
    pub(crate) fn drain(&mut self) -> Vec<OpenCommand> {
        let mut all = std::mem::take(&mut self.stack);
        all.reverse();
        all
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seqs(v: &[OpenCommand]) -> Vec<Seq> {
        v.iter().map(|e| e.seq).collect()
    }

    #[test]
    fn nested_shells_get_parents_and_depths() {
        let mut s = Segmenter::default();
        assert_eq!(s.owner(), Owner::Gap);
        let outer = s.push(1, 100, 1);
        assert_eq!((outer.parent_seq, outer.depth), (None, 0));
        // `zsh` started by command 1 runs its own commands.
        let inner = s.push(2, 200, 1);
        assert_eq!((inner.parent_seq, inner.depth, inner.closed.len()), (Some(1), 1, 0));
        assert_eq!(s.owner(), Owner::Command(2));
        let i = s.find(200, 1, Some(2)).unwrap();
        assert_eq!(s.owner_below(i), Owner::Command(1));
        let p = s.pop(i);
        assert_eq!((p.matched.seq, p.unmatched.len()), (2, 0));
        let inner2 = s.push(3, 200, 2);
        assert_eq!((inner2.parent_seq, inner2.depth), (Some(1), 1));
        assert_eq!(seqs(s.stack()), [1, 3]);
    }

    #[test]
    fn a_missing_done_is_closed_by_the_outer_done() {
        let mut s = Segmenter::default();
        s.push(1, 100, 1);
        s.push(2, 200, 1);
        s.push(3, 300, 1);
        // The inner shells were killed: only the outer shell's `done` arrives.
        let i = s.find(100, 1, None).unwrap();
        assert_eq!(s.owner_below(i), Owner::Gap);
        let p = s.pop(i);
        assert_eq!(p.matched.seq, 1);
        assert_eq!(seqs(&p.unmatched), [3, 2]);
        assert!(s.stack().is_empty());
    }

    #[test]
    fn same_shell_cmd_closes_the_lost_one() {
        let mut s = Segmenter::default();
        s.push(1, 100, 1);
        s.push(2, 200, 1);
        let p = s.push(3, 100, 2);
        assert_eq!(seqs(&p.closed), [2, 1]);
        assert_eq!((p.parent_seq, p.depth), (None, 0));
        assert_eq!(seqs(s.stack()), [3]);
    }

    #[test]
    fn done_matching() {
        let mut s = Segmenter::default();
        s.push(1, 100, 1);
        assert_eq!(s.find(100, 9, None), None, "unknown (sh, n) is ignored");
        assert_eq!(s.find(999, 9, Some(1)), Some(0), "falls back to the recorder's seq");
        assert!(s.is_open(1));
        assert_eq!(seqs(&s.drain()), [1]);
        assert_eq!(Segmenter::from_stack(vec![]).owner(), Owner::Gap);
    }
}
