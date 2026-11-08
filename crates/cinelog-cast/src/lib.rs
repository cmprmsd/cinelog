//! asciicast I/O for cinelog: v3 writer and reader, v2 reader, tolerant line parser,
//! transparent zstd (`.cast.zst`, multi-frame) and rotation parts.
//!
//! - Writing: [`V3Writer`] implements [`CastWriter`]; [`create`] opens a new cast file
//!   (`O_EXCL`, mode 0600). [`Utf8Accumulator`] turns PTY chunks into event text.
//! - Reading: [`open`] / [`open_at`] return a tolerant [`CastReader`] for v2 or v3, plain or
//!   zstd (detected by magic). [`parse_event_line`] is the allocation-free line splitter.
//! - Time: [`ClockMap`] maps cast offsets to wall time from `session`/`clock` markers.
//! - zstd: [`FrameEncoder`], [`scan_frames`] and [`compress_file`] (independent frames cut
//!   at newlines, so a checkpoint offset can be reached by seeking to its frame).
//! - Rotation: [`PartName`], [`find_parts`], [`PartsReader`] and [`RotationPolicy`].

mod clock;
mod header;
mod json;
mod line;
mod parts;
mod reader;
mod utf8;
mod writer;
mod zst;

pub use clock::{ClockAnchor, ClockMap};
pub use header::parse_header;
pub use json::decode_json_string;
pub use line::{MAX_TIME_US, parse_event_line};
pub use parts::{
    PartName, PartsReader, RotateTrigger, RotationPolicy, existing_part, find_parts, part_file_name, part_path,
};
pub use reader::{MAX_HEADER, MAX_LINE, Reader, ResumePoint, open, open_at, read_header};
pub use utf8::Utf8Accumulator;
pub use writer::{SyncFn, V3Writer, WRITE_BUFFER, create};
pub use zst::{
    CompressOptions, Compressed, DEFAULT_FRAME_BYTES, DEFAULT_LEVEL, FrameEncoder, ZSTD_MAGIC, ZstFrame, compress_file,
    compress_from, is_zstd_magic, scan_frames, tmp_path, verify_zst,
};

use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// Header `version` written by cinelog.
pub const CAST_VERSION: u32 = 3;

/// `term` object of a v3 header.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TermInfo {
    pub cols: u16,
    pub rows: u16,
    #[serde(rename = "type", default, skip_serializing_if = "Option::is_none")]
    pub term_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub theme: Option<Value>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// Cast header, normalised to the v3 shape. Field order is the serialisation order, so
/// `version` is always the first key (some players require that).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Header {
    pub version: u32,
    pub term: TermInfo,
    /// Unix seconds at t=0.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timestamp: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idle_time_limit: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env: Option<BTreeMap<String, String>>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl Header {
    /// The header cinelog writes: `{"version":3,"term":{cols,rows,type},"timestamp",env:{SHELL,TERM}}`.
    pub fn new_v3(cols: u16, rows: u16, term: Option<String>, timestamp: i64, shell: Option<String>) -> Self {
        let mut env = BTreeMap::new();
        if let Some(s) = shell {
            env.insert("SHELL".to_owned(), s);
        }
        if let Some(t) = &term {
            env.insert("TERM".to_owned(), t.clone());
        }
        Self {
            version: CAST_VERSION,
            term: TermInfo {
                cols,
                rows,
                term_type: term,
                version: None,
                theme: None,
                extra: Map::new(),
            },
            timestamp: Some(timestamp),
            idle_time_limit: None,
            command: None,
            title: None,
            env: (!env.is_empty()).then_some(env),
            extra: Map::new(),
        }
    }
}

/// On-disk format of a cast being read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    /// asciicast v2: absolute event times.
    V2,
    /// asciicast v3: intervals since the previous event.
    V3,
}

/// Event code (second element of an event line).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EventCode {
    /// `o`: terminal output.
    Output,
    /// `i`: keyboard input.
    Input,
    /// `m`: marker; cinelog markers carry `CastMarker` JSON (`cinelog-proto`).
    Marker,
    /// `r`: resize, data `"COLSxROWS"`.
    Resize,
    /// `x`: exit status (v3).
    Exit,
    /// Any other single-character code, kept for tolerance.
    Other(char),
}

impl EventCode {
    pub const fn as_char(self) -> char {
        match self {
            Self::Output => 'o',
            Self::Input => 'i',
            Self::Marker => 'm',
            Self::Resize => 'r',
            Self::Exit => 'x',
            Self::Other(c) => c,
        }
    }

    pub const fn from_char(c: char) -> Self {
        match c {
            'o' => Self::Output,
            'i' => Self::Input,
            'm' => Self::Marker,
            'r' => Self::Resize,
            'x' => Self::Exit,
            c => Self::Other(c),
        }
    }
}

impl fmt::Display for EventCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.as_char())
    }
}

/// One event with its absolute offset from the start of the cast (or part).
///
/// v3 intervals are parsed as integer microseconds and summed exactly; v2 absolute times
/// are converted to microseconds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Event {
    pub t_us: u64,
    pub code: EventCode,
    pub data: String,
}

impl Event {
    pub fn t_secs(&self) -> f64 {
        self.t_us as f64 / 1e6
    }
}

/// A line split into its parts without decoding the data string (fast path for scanners).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RawLine<'a> {
    /// The time field in µs as written (interval for v3, absolute for v2).
    pub time_us: u64,
    pub code: EventCode,
    /// The JSON string literal including its quotes, still escaped.
    pub data_json: &'a [u8],
}

/// Parses resize data `"COLSxROWS"`.
pub fn parse_resize(data: &str) -> Option<(u16, u16)> {
    let (c, r) = data.split_once('x')?;
    Some((c.trim().parse().ok()?, r.trim().parse().ok()?))
}

/// Formats resize data `"COLSxROWS"`.
pub fn format_resize(cols: u16, rows: u16) -> String {
    format!("{cols}x{rows}")
}

/// Counters of a read pass. Tolerant readers skip bad lines instead of failing.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ReadStats {
    pub events: u64,
    /// Lines that are not valid events (including NUL holes), skipped.
    pub bad_lines: u64,
    pub nul_bytes: u64,
    /// The last line had no newline and did not parse (cast still being written or crashed).
    pub truncated_tail: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum CastError {
    #[error("cast I/O: {0}")]
    Io(#[from] std::io::Error),
    #[error("cast header: {0}")]
    Header(String),
    #[error("cast format: {0}")]
    Format(String),
}

/// Append-only asciicast v3 writer.
///
/// Times are absolute µs offsets from t=0; the writer converts them to intervals with six
/// decimals. A time smaller than the previous one is clamped (interval 0). Data is any
/// valid UTF-8 string and is JSON-escaped by the writer.
pub trait CastWriter {
    /// Must be called exactly once, before any event.
    fn write_header(&mut self, header: &Header) -> Result<(), CastError>;
    fn write_event(&mut self, t_us: u64, code: EventCode, data: &str) -> Result<(), CastError>;
    /// Pushes buffered lines to the OS (called whenever the writer queue runs empty).
    fn flush(&mut self) -> Result<(), CastError>;
    /// `flush` plus `fsync` (every `record.fsync_secs` and at exit).
    fn sync(&mut self) -> Result<(), CastError>;
    /// Bytes written so far including the header (drives `rotate_mb`).
    fn bytes_written(&self) -> u64;
}

/// Streaming, tolerant reader over v2 or v3 casts (plain or zstd).
pub trait CastReader {
    fn header(&self) -> &Header;
    fn format(&self) -> Format;
    /// Next event. Bad lines are skipped and counted in [`ReadStats`]; only I/O errors
    /// are returned as `Err`. `None` at end of data.
    fn next_event(&mut self) -> Option<Result<Event, CastError>>;
    /// Uncompressed byte offset of the next unread line (checkpoint position).
    fn position(&self) -> u64;
    fn stats(&self) -> ReadStats;
}

impl<T: CastWriter + ?Sized> CastWriter for Box<T> {
    fn write_header(&mut self, header: &Header) -> Result<(), CastError> {
        (**self).write_header(header)
    }
    fn write_event(&mut self, t_us: u64, code: EventCode, data: &str) -> Result<(), CastError> {
        (**self).write_event(t_us, code, data)
    }
    fn flush(&mut self) -> Result<(), CastError> {
        (**self).flush()
    }
    fn sync(&mut self) -> Result<(), CastError> {
        (**self).sync()
    }
    fn bytes_written(&self) -> u64 {
        (**self).bytes_written()
    }
}

impl<T: CastReader + ?Sized> CastReader for Box<T> {
    fn header(&self) -> &Header {
        (**self).header()
    }
    fn format(&self) -> Format {
        (**self).format()
    }
    fn next_event(&mut self) -> Option<Result<Event, CastError>> {
        (**self).next_event()
    }
    fn position(&self) -> u64 {
        (**self).position()
    }
    fn stats(&self) -> ReadStats {
        (**self).stats()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_serialises_version_first() {
        let h = Header::new_v3(
            80,
            24,
            Some("xterm-256color".into()),
            1_731_070_000,
            Some("/bin/zsh".into()),
        );
        let s = serde_json::to_string(&h).unwrap();
        assert!(
            s.starts_with(r#"{"version":3,"term":{"cols":80,"rows":24,"type":"xterm-256color"}"#),
            "{s}"
        );
        assert_eq!(serde_json::from_str::<Header>(&s).unwrap(), h);
    }

    #[test]
    fn event_codes_roundtrip() {
        for c in ['o', 'i', 'm', 'r', 'x', 'q'] {
            assert_eq!(EventCode::from_char(c).as_char(), c);
        }
    }

    #[test]
    fn resize_data() {
        assert_eq!(parse_resize(&format_resize(120, 40)), Some((120, 40)));
        assert_eq!(parse_resize("garbage"), None);
    }
}
