//! Per-owner output accounting: logical lines become [`Chunk`]s.
//!
//! Every owner has its own line ordinals (primary and alt counted separately) and byte
//! budget; one chunk is open at a time and closes when the owner or kind changes, at
//! `chunk_bytes`/`chunk_lines`, and whenever the renderer asks (span end, resize).
//! Commands are only tracked until they are closed ([`Chunker::forget`]), so the state
//! stays as small as the open command stack.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::exact::{XChunk, XStamp};
use crate::model::{Chunk, ChunkKind, OutputStats, Owner, RenderOptions, Stamp};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
struct OwnerStats {
    out: OutputStats,
    /// Next alt snapshot line ordinal.
    alt_lines: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct OpenChunk {
    owner: Owner,
    line0: u64,
    at: XStamp,
    text: String,
    lines: usize,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct Chunker {
    gap: OwnerStats,
    commands: BTreeMap<u64, OwnerStats>,
    open: Option<OpenChunk>,
    done: Vec<XChunk>,
}

/// Output wanted at all for `owner` (`index.output`, `index.gap_output`).
pub(crate) fn wanted(opts: &RenderOptions, owner: Owner) -> bool {
    opts.output && (owner != Owner::Gap || opts.gap_output)
}

fn kind_of(owner: Owner) -> ChunkKind {
    match owner {
        Owner::Gap => ChunkKind::Gap,
        Owner::Command(_) => ChunkKind::Primary,
    }
}

impl Chunker {
    fn entry(&mut self, owner: Owner) -> &mut OwnerStats {
        match owner {
            Owner::Gap => &mut self.gap,
            Owner::Command(seq) => self.commands.entry(seq).or_default(),
        }
    }

    /// Drops a closed command's accounting; it never owns output again.
    pub(crate) fn forget(&mut self, owner: Owner) {
        if let Owner::Command(seq) = owner {
            self.commands.remove(&seq);
        }
    }

    /// Charges `bytes` to `owner`; `false` once the per-command cap is reached (the gap
    /// owner has no cap).
    fn admit(&mut self, opts: &RenderOptions, owner: Owner, bytes: usize) -> bool {
        if !wanted(opts, owner) {
            return false;
        }
        let max = opts.max_output_per_command;
        let s = &mut self.entry(owner).out;
        if s.truncated {
            return false;
        }
        if owner != Owner::Gap && s.bytes.saturating_add(bytes as u64) > max {
            s.truncated = true;
            return false;
        }
        s.bytes += bytes as u64;
        true
    }

    /// Appends one finished logical line (already trimmed) of `owner`'s primary output.
    pub(crate) fn line(&mut self, opts: &RenderOptions, owner: Owner, text: &str, at: Stamp) {
        if !self.admit(opts, owner, text.len()) {
            return;
        }
        let s = &mut self.entry(owner).out;
        let ordinal = s.lines;
        s.lines += 1;
        if self
            .open
            .as_ref()
            .is_some_and(|o| o.owner != owner || o.text.len() + 1 + text.len() > opts.chunk_bytes)
        {
            self.close();
        }
        let o = match &mut self.open {
            Some(o) => {
                o.text.push('\n');
                o.text.push_str(text);
                o.lines += 1;
                o
            }
            // A chunk starts at its first non-blank line, so its `t` is a real write.
            None if text.is_empty() => return,
            None => self.open.insert(OpenChunk {
                owner,
                line0: ordinal,
                at: at.into(),
                text: text.to_owned(),
                lines: 1,
            }),
        };
        if o.text.len() >= opts.chunk_bytes || o.lines >= opts.chunk_lines {
            self.close();
        }
    }

    /// Appends de-duplicated alt-screen snapshot lines as `alt` chunks.
    pub(crate) fn alt(&mut self, opts: &RenderOptions, owner: Owner, lines: Vec<(String, Stamp)>) {
        let mut cur: Option<OpenChunk> = None;
        for (text, at) in lines {
            if !self.admit(opts, owner, text.len()) {
                break;
            }
            let s = self.entry(owner);
            let ordinal = s.alt_lines;
            s.alt_lines += 1;
            if cur
                .as_ref()
                .is_some_and(|c| c.text.len() + 1 + text.len() > opts.chunk_bytes)
            {
                self.push(cur.take().expect("checked"), ChunkKind::Alt);
            }
            let c = match &mut cur {
                Some(c) => {
                    c.text.push('\n');
                    c.text.push_str(&text);
                    c.lines += 1;
                    c
                }
                None => cur.insert(OpenChunk {
                    owner,
                    line0: ordinal,
                    at: at.into(),
                    text,
                    lines: 1,
                }),
            };
            if c.text.len() >= opts.chunk_bytes || c.lines >= opts.chunk_lines {
                self.push(cur.take().expect("just filled"), ChunkKind::Alt);
            }
        }
        if let Some(c) = cur {
            self.push(c, ChunkKind::Alt);
        }
    }

    /// Ends the open chunk (span end, resize, size limit).
    pub(crate) fn close(&mut self) {
        if let Some(o) = self.open.take() {
            let kind = kind_of(o.owner);
            self.push(o, kind);
        }
    }

    fn push(&mut self, mut o: OpenChunk, kind: ChunkKind) {
        o.text.truncate(o.text.trim_end().len());
        if o.text.is_empty() {
            return;
        }
        self.done.push(
            Chunk {
                kind,
                cmd_seq: o.owner.cmd_seq(),
                t: Stamp::from(o.at).t,
                ts: Stamp::from(o.at).ts,
                line0: o.line0,
                text: o.text,
            }
            .into(),
        );
    }

    pub(crate) fn take(&mut self) -> Vec<Chunk> {
        self.done.drain(..).map(Chunk::from).collect()
    }

    pub(crate) fn stats(&self, owner: Owner) -> OutputStats {
        match owner {
            Owner::Gap => self.gap.out,
            Owner::Command(seq) => self.commands.get(&seq).map(|s| s.out).unwrap_or_default(),
        }
    }

    /// The open chunk of `owner`: (line0, time, text).
    pub(crate) fn open_of(&self, owner: Owner) -> Option<(u64, Stamp, &str)> {
        self.open
            .as_ref()
            .filter(|o| o.owner == owner)
            .map(|o| (o.line0, o.at.into(), o.text.as_str()))
    }

    /// Ordinal the next primary line of `owner` gets.
    pub(crate) fn next_line(&self, owner: Owner) -> u64 {
        self.stats(owner).lines
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(t: f64) -> Stamp {
        Stamp { t, ts: 100.0 + t }
    }

    fn opts() -> RenderOptions {
        RenderOptions {
            chunk_bytes: 16,
            chunk_lines: 3,
            max_output_per_command: 40,
            ..RenderOptions::default()
        }
    }

    #[test]
    fn chunks_split_by_lines_bytes_and_owner() {
        let o = opts();
        let mut c = Chunker::default();
        let a = Owner::Command(1);
        c.line(&o, a, "", at(0.0));
        for (i, l) in ["one", "", "two", "three"].iter().enumerate() {
            c.line(&o, a, l, at(1.0 + i as f64));
        }
        c.line(&o, Owner::Gap, "$ prompt", at(9.0));
        c.close();
        let got = c.take();
        let view: Vec<_> = got
            .iter()
            .map(|c| (c.kind, c.cmd_seq, c.line0, c.t, c.text.as_str()))
            .collect();
        assert_eq!(
            view,
            [
                (ChunkKind::Primary, Some(1), 1, 1.0, "one\n\ntwo"),
                (ChunkKind::Primary, Some(1), 4, 4.0, "three"),
                (ChunkKind::Gap, None, 0, 9.0, "$ prompt"),
            ]
        );
        assert_eq!(c.stats(a).lines, 5);
        assert_eq!(c.stats(a).bytes, 11);
    }

    #[test]
    fn byte_limit_closes_and_cap_truncates_commands_only() {
        let o = opts();
        let mut c = Chunker::default();
        let a = Owner::Command(7);
        c.line(&o, a, "0123456789abcdef", at(0.0));
        assert_eq!(c.take().len(), 1, "a full chunk closes at chunk_bytes");
        c.line(&o, a, "0123456789abcdefghijklmnop", at(1.0));
        c.line(&o, a, "dropped", at(2.0));
        c.line(&o, a, "x", at(3.0));
        assert!(c.stats(a).truncated);
        assert_eq!(c.stats(a).bytes, 16);
        for _ in 0..10 {
            c.line(&o, Owner::Gap, "gap output is never capped", at(4.0));
        }
        c.close();
        assert!(!c.stats(Owner::Gap).truncated);
        assert!(c.take().iter().all(|ch| ch.cmd_seq.is_none()));
    }

    #[test]
    fn options_switch_output_off() {
        let mut c = Chunker::default();
        let gap_off = RenderOptions {
            gap_output: false,
            ..opts()
        };
        c.line(&gap_off, Owner::Gap, "prompt", at(0.0));
        c.line(&gap_off, Owner::Command(1), "kept", at(0.0));
        c.close();
        assert_eq!(c.take().len(), 1);
        let off = RenderOptions {
            output: false,
            ..opts()
        };
        c.line(&off, Owner::Command(1), "nothing", at(0.0));
        c.alt(&off, Owner::Command(1), vec![("alt".into(), at(0.0))]);
        c.close();
        assert!(c.take().is_empty());
    }

    #[test]
    fn forgotten_commands_leave_no_state() {
        let o = opts();
        let mut c = Chunker::default();
        for seq in 1..=1000 {
            c.line(&o, Owner::Gap, "$ x", at(0.0));
            c.line(&o, Owner::Command(seq), "out", at(0.0));
            c.close();
            c.forget(Owner::Command(seq));
        }
        c.forget(Owner::Gap);
        assert_eq!(c.stats(Owner::Gap).lines, 1000, "the gap owner is never forgotten");
        assert!(c.commands.is_empty());
        assert_eq!(c.take().len(), 2000);
        assert!(serde_json::to_string(&c).unwrap().len() < 200);
    }

    #[test]
    fn alt_lines_have_their_own_ordinals() {
        let o = opts();
        let mut c = Chunker::default();
        let a = Owner::Command(2);
        c.line(&o, a, "primary", at(0.0));
        c.alt(&o, a, vec![("a".into(), at(1.0)), ("b".into(), at(1.5))]);
        c.alt(&o, a, vec![("c".into(), at(2.0))]);
        c.close();
        let got: Vec<_> = c.take().into_iter().map(|c| (c.kind, c.line0, c.text)).collect();
        assert_eq!(
            got,
            [
                (ChunkKind::Alt, 0, "a\nb".to_owned()),
                (ChunkKind::Alt, 2, "c".to_owned()),
                (ChunkKind::Primary, 0, "primary".to_owned()),
            ]
        );
    }
}
