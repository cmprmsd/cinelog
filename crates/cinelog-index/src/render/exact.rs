//! Bit-exact serialization of times in renderer and pipeline state.
//!
//! serde_json parses floats on a best-effort basis (the workspace does not enable
//! `float_roundtrip`), so a value written into a checkpoint could come back one ULP off and
//! a resumed run would no longer equal a one-shot run (I12). Private state therefore keeps
//! every `f64` as its IEEE-754 bit pattern.

use serde::{Deserialize, Serialize};

use crate::model::{Chunk, ChunkKind, Command, Segment, Seq, Stamp};

/// An `f64` serialized as its bit pattern.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(transparent)]
pub(crate) struct F64(u64);

impl F64 {
    pub(crate) fn new(v: f64) -> Self {
        Self(v.to_bits())
    }

    pub(crate) fn get(self) -> f64 {
        f64::from_bits(self.0)
    }
}

/// A [`Stamp`] serialized bit-exactly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub(crate) struct XStamp(F64, F64);

impl From<Stamp> for XStamp {
    fn from(s: Stamp) -> Self {
        Self(F64::new(s.t), F64::new(s.ts))
    }
}

impl From<XStamp> for Stamp {
    fn from(x: XStamp) -> Self {
        Self {
            t: x.0.get(),
            ts: x.1.get(),
        }
    }
}

/// A finished [`Chunk`] that has not been taken yet, serialized bit-exactly.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct XChunk {
    kind: ChunkKind,
    seq: Option<Seq>,
    at: XStamp,
    line0: u64,
    text: String,
}

impl From<Chunk> for XChunk {
    fn from(c: Chunk) -> Self {
        Self {
            kind: c.kind,
            seq: c.cmd_seq,
            at: Stamp { t: c.t, ts: c.ts }.into(),
            line0: c.line0,
            text: c.text,
        }
    }
}

impl From<XChunk> for Chunk {
    fn from(x: XChunk) -> Self {
        let at = Stamp::from(x.at);
        Self {
            kind: x.kind,
            cmd_seq: x.seq,
            t: at.t,
            ts: at.ts,
            line0: x.line0,
            text: x.text,
        }
    }
}

/// An `Option<f64>` serialized bit-exactly.
fn opt(v: Option<f64>) -> Option<F64> {
    v.map(F64::new)
}

/// A [`Command`] serialized bit-exactly (open commands live in pipeline checkpoints).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct XCommand {
    seq: Seq,
    parent_seq: Option<Seq>,
    depth: u32,
    shell_pid: Option<u32>,
    t_start: F64,
    t_end: Option<F64>,
    ts_start: F64,
    ts_end: Option<F64>,
    exit: Option<i32>,
    pipestatus: Vec<i32>,
    cwd: Option<String>,
    user: Option<String>,
    euid: Option<u32>,
    typed: String,
    expanded: Option<String>,
    typed_trunc: bool,
    approx: bool,
    confidence: Option<F64>,
    out_bytes: u64,
    out_truncated: bool,
}

impl XCommand {
    pub(crate) fn seq(&self) -> Seq {
        self.seq
    }
}

impl From<Command> for XCommand {
    fn from(c: Command) -> Self {
        Self {
            seq: c.seq,
            parent_seq: c.parent_seq,
            depth: c.depth,
            shell_pid: c.shell_pid,
            t_start: F64::new(c.t_start),
            t_end: opt(c.t_end),
            ts_start: F64::new(c.ts_start),
            ts_end: opt(c.ts_end),
            exit: c.exit,
            pipestatus: c.pipestatus,
            cwd: c.cwd,
            user: c.user,
            euid: c.euid,
            typed: c.typed,
            expanded: c.expanded,
            typed_trunc: c.typed_trunc,
            approx: c.approx,
            confidence: opt(c.confidence),
            out_bytes: c.out_bytes,
            out_truncated: c.out_truncated,
        }
    }
}

impl From<XCommand> for Command {
    fn from(x: XCommand) -> Self {
        Self {
            seq: x.seq,
            parent_seq: x.parent_seq,
            depth: x.depth,
            shell_pid: x.shell_pid,
            t_start: x.t_start.get(),
            t_end: x.t_end.map(F64::get),
            ts_start: x.ts_start.get(),
            ts_end: x.ts_end.map(F64::get),
            exit: x.exit,
            pipestatus: x.pipestatus,
            cwd: x.cwd,
            user: x.user,
            euid: x.euid,
            typed: x.typed,
            expanded: x.expanded,
            typed_trunc: x.typed_trunc,
            approx: x.approx,
            confidence: x.confidence.map(F64::get),
            out_bytes: x.out_bytes,
            out_truncated: x.out_truncated,
        }
    }
}

/// A [`Segment`] serialized bit-exactly (the open playback segment of a pipeline).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct XSegment {
    seq: u32,
    part: u32,
    t_start: F64,
    t_end: Option<F64>,
    byte_offset: u64,
    vt_dump: String,
}

impl XSegment {
    pub(crate) fn t_start(&self) -> f64 {
        self.t_start.get()
    }
}

impl From<Segment> for XSegment {
    fn from(s: Segment) -> Self {
        Self {
            seq: s.seq,
            part: s.part,
            t_start: F64::new(s.t_start),
            t_end: opt(s.t_end),
            byte_offset: s.byte_offset,
            vt_dump: s.vt_dump,
        }
    }
}

impl From<XSegment> for Segment {
    fn from(x: XSegment) -> Self {
        Self {
            seq: x.seq,
            part: x.part,
            t_start: x.t_start.get(),
            t_end: x.t_end.map(F64::get),
            byte_offset: x.byte_offset,
            vt_dump: x.vt_dump,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn floats_survive_json_bit_exactly() {
        for v in [0.1 + 0.2, 1_731_070_000.123_456_7, f64::MIN_POSITIVE, -0.0, 5e-324] {
            let s = serde_json::to_string(&F64::new(v)).unwrap();
            let back: F64 = serde_json::from_str(&s).unwrap();
            assert_eq!(back.get().to_bits(), v.to_bits());
        }
        let c = Chunk {
            kind: ChunkKind::Alt,
            cmd_seq: Some(3),
            t: 1.000_000_000_000_000_2,
            ts: 1_731_070_001.000_001,
            line0: 7,
            text: "x".into(),
        };
        let x: XChunk = serde_json::from_str(&serde_json::to_string(&XChunk::from(c.clone())).unwrap()).unwrap();
        assert_eq!(Chunk::from(x), c);
        let s = Segment {
            seq: 2,
            part: 1,
            t_start: 0.1 + 0.2,
            t_end: Some(60.000_000_000_000_01),
            byte_offset: 9,
            vt_dump: "\u{1b}[H".into(),
        };
        let x: XSegment = serde_json::from_str(&serde_json::to_string(&XSegment::from(s.clone())).unwrap()).unwrap();
        assert_eq!(Segment::from(x), s);
    }
}
