//! Streaming, tolerant cast reader (v2/v3, plain or zstd).

use std::fs::File;
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom};
use std::os::unix::fs::FileExt;
use std::path::Path;

use memchr::{memchr, memchr_iter, memrchr};

use crate::header::parse_header;
use crate::line::parse_event_line;
use crate::zst::{self, ZstFrame};
use crate::{CastError, CastReader, Event, Format, Header, ReadStats};

/// Lines longer than this are skipped and counted as bad (corrupt file without newlines).
pub const MAX_LINE: usize = 32 << 20;
/// Longest accepted header line.
pub const MAX_HEADER: u64 = 1 << 20;
const READ_BUFFER: usize = 256 * 1024;

/// Where a reader resumes: an uncompressed byte offset from [`CastReader::position`] and
/// the absolute event time at that point.
///
/// The offset is a line boundary: a line start, or the byte right after the `]` of an
/// event that had no newline yet when it was read (the end of a cast without a final
/// newline, or a live cast caught between the writer's `]` and `\n`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ResumePoint {
    pub offset: u64,
    pub t_us: u64,
}

/// Reads the header line. Returns the header, its format and the bytes consumed.
pub fn read_header<R: BufRead>(src: &mut R) -> Result<(Header, Format, u64), CastError> {
    let mut consumed = 0u64;
    let mut line = Vec::new();
    loop {
        line.clear();
        let n = src.by_ref().take(MAX_HEADER).read_until(b'\n', &mut line)? as u64;
        consumed += n;
        if n == 0 {
            return Err(CastError::Header("empty cast".into()));
        }
        if line.last() != Some(&b'\n') && n >= MAX_HEADER {
            return Err(CastError::Header("header line too long".into()));
        }
        if !line.iter().all(|b| b.is_ascii_whitespace()) {
            let (header, format) = parse_header(&line)?;
            return Ok((header, format, consumed));
        }
    }
}

enum LineStatus {
    /// A full line (without its newline) is in `line`.
    Complete,
    /// End of data with an unterminated line pending.
    Partial,
    /// End of data at a line boundary.
    Eof,
}

/// Tolerant reader over any buffered byte stream positioned after the header.
///
/// At end of data it returns `None` but stays usable: if the underlying source grows (a
/// live cast), later calls continue, including a line that was incomplete before. A
/// complete event without its newline is returned as soon as it is seen.
pub struct Reader<R> {
    src: R,
    header: Header,
    format: Format,
    line: Vec<u8>,
    /// Raw bytes of the current (pending) line consumed so far, including discarded ones.
    line_raw: u64,
    discarding: bool,
    /// The pending line contained a NUL hole, already counted in `stats` and cut from `line`.
    holed: bool,
    pos: u64,
    t_us: u64,
    stats: ReadStats,
    failed: bool,
}

impl<R: BufRead> Reader<R> {
    /// Reads the header from `src` and positions the reader at the first event.
    pub fn new(mut src: R) -> Result<Self, CastError> {
        let (header, format, offset) = read_header(&mut src)?;
        Ok(Self::resume(src, header, format, ResumePoint { offset, t_us: 0 }))
    }

    /// A reader over `src`, which must already be positioned at `at.offset`.
    pub fn resume(src: R, header: Header, format: Format, at: ResumePoint) -> Self {
        Self {
            src,
            header,
            format,
            line: Vec::new(),
            line_raw: 0,
            discarding: false,
            holed: false,
            pos: at.offset,
            t_us: at.t_us,
            stats: ReadStats::default(),
            failed: false,
        }
    }

    /// Absolute time of the last event read (the resume time for [`Self::position`]).
    pub fn t_us(&self) -> u64 {
        self.t_us
    }

    /// The checkpoint for the next unread line.
    pub fn resume_point(&self) -> ResumePoint {
        ResumePoint {
            offset: self.pos,
            t_us: self.t_us,
        }
    }

    pub fn into_inner(self) -> R {
        self.src
    }

    fn fill_line(&mut self) -> io::Result<LineStatus> {
        loop {
            let buf = match self.src.fill_buf() {
                Ok(buf) => buf,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            };
            if buf.is_empty() {
                return Ok(if self.line_raw == 0 {
                    LineStatus::Eof
                } else {
                    LineStatus::Partial
                });
            }
            let (take, complete) = match memchr(b'\n', buf) {
                Some(i) => (i, true),
                None => (buf.len(), false),
            };
            if !self.discarding {
                if self.line.len() + take > MAX_LINE {
                    self.discarding = true;
                    self.line = Vec::new();
                } else {
                    self.line.extend_from_slice(&buf[..take]);
                }
            }
            let consumed = take + usize::from(complete);
            self.src.consume(consumed);
            self.line_raw += consumed as u64;
            if complete {
                return Ok(LineStatus::Complete);
            }
        }
    }

    fn end_line(&mut self) {
        self.pos += self.line_raw;
        self.line_raw = 0;
        self.line.clear();
        self.discarding = false;
        self.holed = false;
    }

    /// Ends an unterminated line that held a complete event. The position stops right
    /// after the event's `]`; trailing whitespace already consumed stays pending, so the
    /// newline that may follow later is read as an empty line.
    fn end_tail_event(&mut self) {
        let trailing = (self.line.len() - self.line.trim_ascii_end().len()) as u64;
        self.pos += self.line_raw - trailing;
        self.line_raw = trailing;
        self.line.clear();
        self.holed = false;
    }

    /// A NUL hole (crash before delayed allocation) destroys everything before its end;
    /// only the text after the last NUL can still be a whole event. Counts the hole once
    /// per line and drops it from the pending line, so a line that grows across follow
    /// calls is not counted twice.
    fn absorb_hole(&mut self) {
        if self.discarding {
            return;
        }
        if let Some(last_nul) = memrchr(0, &self.line) {
            self.stats.nul_bytes += memchr_iter(0, &self.line[..=last_nul]).count() as u64;
            if !self.holed {
                self.stats.bad_lines += 1;
                self.holed = true;
            }
            self.line.drain(..=last_nul);
        }
    }

    /// Classifies a complete line; returns the event if it is one.
    fn take_complete_line(&mut self) -> Option<Event> {
        let ev = if self.discarding {
            self.stats.bad_lines += 1;
            None
        } else {
            self.absorb_hole();
            self.classify_pending(true)
        };
        self.end_line();
        ev
    }

    /// Parses the pending line (with any NUL hole already absorbed). Bad lines are counted
    /// only when `final_` is set, i.e. the line is complete and will not grow any more.
    fn classify_pending(&mut self, final_: bool) -> Option<Event> {
        let line = std::mem::take(&mut self.line);
        let text = line.trim_ascii();
        let ev = if text.is_empty() || (!self.holed && text[0] == b'#') {
            None
        } else {
            let ev = self.event_from(text);
            if ev.is_none() && final_ && !self.holed {
                self.stats.bad_lines += 1;
            }
            ev
        };
        self.line = line;
        ev
    }

    fn event_from(&mut self, line: &[u8]) -> Option<Event> {
        let raw = parse_event_line(line)?;
        self.t_us = match self.format {
            Format::V3 => self.t_us.saturating_add(raw.time_us),
            Format::V2 => self.t_us.max(raw.time_us),
        };
        self.stats.events += 1;
        Some(Event {
            t_us: self.t_us,
            code: raw.code,
            data: raw.decode_data(),
        })
    }
}

impl<R: BufRead> CastReader for Reader<R> {
    fn header(&self) -> &Header {
        &self.header
    }

    fn format(&self) -> Format {
        self.format
    }

    fn next_event(&mut self) -> Option<Result<Event, CastError>> {
        if self.failed {
            return None;
        }
        loop {
            match self.fill_line() {
                Err(e) => {
                    self.failed = true;
                    return Some(Err(e.into()));
                }
                Ok(LineStatus::Eof) => {
                    self.stats.truncated_tail = false;
                    return None;
                }
                Ok(LineStatus::Partial) => {
                    // A complete event that just lacks its newline is accepted; anything
                    // else stays pending until more data arrives.
                    if !self.discarding {
                        self.absorb_hole();
                        if let Some(ev) = self.classify_pending(false) {
                            self.end_tail_event();
                            self.stats.truncated_tail = false;
                            return Some(Ok(ev));
                        }
                    }
                    // Unterminated whitespace (e.g. after an accepted tail event) loses nothing.
                    self.stats.truncated_tail = self.discarding || self.holed || !self.line.trim_ascii().is_empty();
                    return None;
                }
                Ok(LineStatus::Complete) => {
                    self.stats.truncated_tail = false;
                    if let Some(ev) = self.take_complete_line() {
                        return Some(Ok(ev));
                    }
                }
            }
        }
    }

    fn position(&self) -> u64 {
        self.pos
    }

    fn stats(&self) -> ReadStats {
        self.stats
    }
}

fn boxed<R: BufRead + Send + 'static>(reader: Reader<R>) -> Box<dyn CastReader + Send> {
    Box::new(reader)
}

/// Opens a cast: v2 or v3, plain or zstd (detected by magic, not by extension).
pub fn open(path: &Path) -> Result<Box<dyn CastReader + Send>, CastError> {
    let file = File::open(path)?;
    if zst::sniff(&file)? {
        let dec = zstd::stream::read::Decoder::new(file)?;
        Ok(boxed(Reader::new(BufReader::with_capacity(READ_BUFFER, dec))?))
    } else {
        Ok(boxed(Reader::new(BufReader::with_capacity(READ_BUFFER, file))?))
    }
}

/// Opens a cast and resumes at a checkpoint. `at.offset == 0` starts after the header.
///
/// For zstd casts the frame table is used to seek (`frames`, e.g. from the DB, or scanned
/// from the file when `None`). The offset must be a line boundary (see [`ResumePoint`]):
/// otherwise, or if it lies beyond the data, this fails with [`CastError::Format`].
pub fn open_at(
    path: &Path,
    at: ResumePoint,
    frames: Option<&[ZstFrame]>,
) -> Result<Box<dyn CastReader + Send>, CastError> {
    let file = File::open(path)?;
    if zst::sniff(&file)? {
        open_zst_at(file, at, frames)
    } else {
        open_plain_at(file, at)
    }
}

fn open_plain_at(file: File, at: ResumePoint) -> Result<Box<dyn CastReader + Send>, CastError> {
    let mut src = BufReader::with_capacity(READ_BUFFER, file);
    let (header, format, header_len) = read_header(&mut src)?;
    if at.offset <= header_len {
        return Ok(boxed(Reader::resume(
            src,
            header,
            format,
            ResumePoint {
                offset: header_len,
                ..at
            },
        )));
    }
    let file = src.get_ref();
    if at.offset > file.metadata()?.len() {
        return Err(CastError::Format(format!(
            "resume offset {} beyond end of cast",
            at.offset
        )));
    }
    let mut prev = [0u8; 1];
    file.read_exact_at(&mut prev, at.offset - 1)?;
    let mut next = [0u8; 1];
    let next = (file.read_at(&mut next, at.offset)? == 1).then_some(next[0]);
    check_boundary(at.offset, prev[0], next)?;
    src.seek(SeekFrom::Start(at.offset))?;
    Ok(boxed(Reader::resume(src, header, format, at)))
}

/// Accepts a line start, or the end of an event line whose newline is not read yet (the
/// position a reader reports after an unterminated tail event).
fn check_boundary(offset: u64, prev: u8, next: Option<u8>) -> Result<(), CastError> {
    if prev == b'\n' || (prev == b']' && next.is_none_or(|b| b.is_ascii_whitespace())) {
        Ok(())
    } else {
        Err(CastError::Format(format!(
            "resume offset {offset} is not a line boundary"
        )))
    }
}

fn open_zst_at(
    mut file: File,
    at: ResumePoint,
    frames: Option<&[ZstFrame]>,
) -> Result<Box<dyn CastReader + Send>, CastError> {
    let (header, format, header_len) = {
        let mut dec = BufReader::new(zstd::stream::read::Decoder::new(&file)?);
        read_header(&mut dec)?
    };
    let offset = at.offset.max(header_len);
    let scanned;
    let frames = match frames {
        Some(f) => f,
        None => {
            scanned = zst::scan_frames(&mut file)?;
            &scanned
        }
    };
    let frame = frames
        .iter()
        .rev()
        .find(|f| f.u_offset <= offset)
        .copied()
        .unwrap_or_default();
    file.seek(SeekFrom::Start(frame.c_offset))?;
    let mut src = BufReader::with_capacity(READ_BUFFER, zstd::stream::read::Decoder::new(file)?);
    let skip = offset - frame.u_offset;
    if skip > 0 {
        let skipped = io::copy(&mut src.by_ref().take(skip - 1), &mut io::sink())?;
        let mut prev = [0u8; 1];
        if skipped != skip - 1 || src.read(&mut prev)? != 1 {
            return Err(CastError::Format(format!("resume offset {offset} beyond end of cast")));
        }
        let next = src.fill_buf()?.first().copied();
        check_boundary(offset, prev[0], next)?;
    }
    Ok(boxed(Reader::resume(src, header, format, ResumePoint { offset, ..at })))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{CastWriter, EventCode, V3Writer};
    use std::io::{Cursor, Write};

    const HEADER: &str = r#"{"version":3,"term":{"cols":80,"rows":24}}"#;

    fn read_all(r: &mut dyn CastReader) -> Vec<(u64, char, String)> {
        std::iter::from_fn(|| r.next_event())
            .map(|e| e.unwrap())
            .map(|e| (e.t_us, e.code.as_char(), e.data))
            .collect()
    }

    fn reader(s: &[u8]) -> Reader<Cursor<Vec<u8>>> {
        Reader::new(Cursor::new(s.to_vec())).unwrap()
    }

    #[test]
    fn sums_v3_intervals_exactly() {
        let mut cast = format!("{HEADER}\n");
        for _ in 0..1000 {
            cast.push_str("[0.1, \"o\", \"x\"]\n");
        }
        let mut r = reader(cast.as_bytes());
        let evs = read_all(&mut r);
        assert_eq!(evs.len(), 1000);
        assert_eq!(evs.last().unwrap().0, 100_000_000);
        assert_eq!(
            r.stats(),
            ReadStats {
                events: 1000,
                ..Default::default()
            }
        );
        assert_eq!(r.position(), cast.len() as u64);
    }

    #[test]
    fn v2_absolute_times() {
        let cast = concat!(
            r#"{"version": 2, "width": 100, "height": 30, "timestamp": 1700000000}"#,
            "\n[0.5, \"o\", \"a\"]\n[1.25, \"i\", \"b\"]\n[1.0, \"o\", \"late\"]\n[1e-05, \"o\", \"c\"]\n",
        );
        let mut r = reader(cast.as_bytes());
        assert_eq!(r.format(), Format::V2);
        assert_eq!(r.header().term.cols, 100);
        let times: Vec<u64> = read_all(&mut r).into_iter().map(|e| e.0).collect();
        assert_eq!(times, [500_000, 1_250_000, 1_250_000, 1_250_000]);
    }

    #[test]
    fn writer_reader_roundtrip() {
        let samples = ["plain", "\u{1b}[31mred\u{1b}[0m\r\n", "q\"\\\t\u{0}", "😀 ü", ""];
        let mut w = V3Writer::new(Vec::new());
        w.write_header(&crate::Header::new_v3(80, 24, None, 1, None)).unwrap();
        let mut t = 0;
        for (i, s) in samples.iter().enumerate() {
            t += 123_457 * i as u64;
            w.write_event(t, EventCode::Output, s).unwrap();
        }
        let mut r = Reader::new(Cursor::new(w.into_inner())).unwrap();
        let evs = read_all(&mut r);
        let mut t = 0;
        for (i, (ev, s)) in evs.iter().zip(samples).enumerate() {
            t += 123_457 * i as u64;
            assert_eq!((ev.0, ev.2.as_str()), (t, s));
        }
    }

    #[test]
    fn skips_bad_lines_comments_and_crlf() {
        let cast =
            format!("{HEADER}\n\n# comment\n[0.1, \"o\", \"a\"]\r\ngarbage\n[0.1, \"o\"\n  \n[0.2, \"m\", \"{{}}\"]\n");
        let mut r = reader(cast.as_bytes());
        let evs = read_all(&mut r);
        assert_eq!(evs, [(100_000, 'o', "a".into()), (300_000, 'm', "{}".into())]);
        assert_eq!(r.stats().bad_lines, 2);
        assert!(!r.stats().truncated_tail);
    }

    #[test]
    fn nul_hole() {
        let mut cast = format!("{HEADER}\n[0.1, \"o\", \"a\"]\n[0.1, \"o\", \"lo").into_bytes();
        cast.extend_from_slice(&[0; 4096]);
        cast.extend_from_slice(b"[0.5, \"o\", \"b\"]\n");
        cast.extend_from_slice(&[0; 10]);
        cast.extend_from_slice(b"\n[0.1, \"o\", \"c\"]\n");
        let mut r = reader(&cast);
        let evs = read_all(&mut r);
        assert_eq!(
            evs,
            [
                (100_000, 'o', "a".into()),
                (600_000, 'o', "b".into()),
                (700_000, 'o', "c".into())
            ]
        );
        let st = r.stats();
        assert_eq!((st.bad_lines, st.nul_bytes, st.events), (2, 4106, 3));
    }

    #[test]
    fn truncated_tail_and_follow() {
        let path = tempfile::NamedTempFile::new().unwrap();
        let mut f = path.reopen().unwrap();
        write!(f, "{HEADER}\n[0.1, \"o\", \"a\"]\n[0.2, \"o\", \"b").unwrap();
        let mut r = open(path.path()).unwrap();
        assert_eq!(r.next_event().unwrap().unwrap().data, "a");
        let after_a = r.position();
        assert!(r.next_event().is_none());
        assert!(r.stats().truncated_tail);
        assert_eq!(r.position(), after_a);

        write!(f, "c\"]").unwrap();
        let ev = r.next_event().unwrap().unwrap();
        assert_eq!((ev.t_us, ev.data.as_str()), (300_000, "bc"));
        assert!(!r.stats().truncated_tail);
        write!(f, "\n[0.1, \"x\", \"0\"]\n").unwrap();
        let ev = r.next_event().unwrap().unwrap();
        assert_eq!((ev.t_us, ev.code), (400_000, EventCode::Exit));
        assert!(r.next_event().is_none());
        assert_eq!(r.position(), std::fs::metadata(path.path()).unwrap().len());
        assert_eq!(r.stats().bad_lines, 0);
    }

    #[test]
    fn overlong_line_is_skipped() {
        let mut cast = format!("{HEADER}\n[0.1, \"o\", \"").into_bytes();
        cast.resize(cast.len() + MAX_LINE + 10, b'x');
        cast.extend_from_slice(b"\"]\n[0.1, \"o\", \"ok\"]\n");
        let mut r = reader(&cast);
        let evs = read_all(&mut r);
        assert_eq!(evs, [(100_000, 'o', "ok".into())]);
        assert_eq!(r.stats().bad_lines, 1);
        assert_eq!(r.position(), cast.len() as u64);
    }

    #[test]
    fn header_errors() {
        assert!(matches!(
            Reader::new(Cursor::new(Vec::new())),
            Err(CastError::Header(_))
        ));
        assert!(matches!(
            Reader::new(Cursor::new(b"\n\n".to_vec())),
            Err(CastError::Header(_))
        ));
        assert!(matches!(
            Reader::new(Cursor::new(b"[0.1, \"o\", \"x\"]\n".to_vec())),
            Err(CastError::Header(_))
        ));
        let r = reader(b"\n{\"version\":3,\"term\":{\"cols\":2,\"rows\":3}}");
        assert_eq!(r.header().term.rows, 3);
    }

    #[test]
    fn resume_plain() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.cast");
        let cast = format!("{HEADER}\n[0.1, \"o\", \"a\"]\n[0.2, \"o\", \"b\"]\n[0.3, \"o\", \"c\"]\n");
        std::fs::write(&path, &cast).unwrap();
        let mut r = open(&path).unwrap();
        r.next_event().unwrap().unwrap();
        let ck = ResumePoint {
            offset: r.position(),
            t_us: 100_000,
        };
        let mut r2 = open_at(&path, ck, None).unwrap();
        assert_eq!(
            read_all(&mut *r2),
            [(300_000, 'o', "b".into()), (600_000, 'o', "c".into())]
        );

        let mut r0 = open_at(&path, ResumePoint::default(), None).unwrap();
        assert_eq!(read_all(&mut *r0).len(), 3);
        let bad = ResumePoint {
            offset: ck.offset + 2,
            t_us: 0,
        };
        assert!(matches!(open_at(&path, bad, None), Err(CastError::Format(_))));
        let beyond = ResumePoint {
            offset: 10_000,
            t_us: 0,
        };
        assert!(matches!(open_at(&path, beyond, None), Err(CastError::Format(_))));
        let end = ResumePoint {
            offset: cast.len() as u64,
            t_us: 0,
        };
        assert!(open_at(&path, end, None).unwrap().next_event().is_none());
    }
    fn zst_copy(path: &Path) -> std::path::PathBuf {
        let dst = path.with_extension("cast.zst");
        crate::compress_file(path, &dst, Default::default()).unwrap();
        dst
    }

    #[test]
    fn resume_at_eof_without_final_newline() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("n.cast");
        let cast = format!("{HEADER}\n[0.1, \"o\", \"a\"]\n[0.2, \"o\", \"b\"]");
        std::fs::write(&path, &cast).unwrap();
        for p in [path.clone(), zst_copy(&path)] {
            let mut r = open(&p).unwrap();
            assert_eq!(read_all(&mut *r).len(), 2);
            assert!(!r.stats().truncated_tail);
            assert_eq!(r.position(), cast.len() as u64);
            let ck = ResumePoint {
                offset: r.position(),
                t_us: 300_000,
            };
            let mut resumed = open_at(&p, ck, None).unwrap();
            assert!(resumed.next_event().is_none());
            assert_eq!(resumed.stats(), ReadStats::default());
        }
        // A `]` in the middle of a line is still rejected.
        let mid = ResumePoint {
            offset: cast.find("a\"]").unwrap() as u64 + 2,
            t_us: 0,
        };
        assert!(matches!(open_at(&path, mid, None), Err(CastError::Format(_))));
    }

    #[test]
    fn checkpoint_between_bracket_and_newline() {
        let path = tempfile::NamedTempFile::new().unwrap();
        let mut f = path.reopen().unwrap();
        write!(f, "{HEADER}\n[0.1, \"o\", \"a\"]\n[0.2, \"o\", \"b\"]").unwrap();
        let mut r = open(path.path()).unwrap();
        assert_eq!(read_all(&mut *r).len(), 2);
        let ck = ResumePoint {
            offset: r.position(),
            t_us: 300_000,
        };
        write!(f, "\n[0.4, \"o\", \"c\"]\n").unwrap();
        let mut resumed = open_at(path.path(), ck, None).unwrap();
        assert_eq!(read_all(&mut *resumed), [(700_000, 'o', "c".into())]);
        assert_eq!(resumed.stats().bad_lines, 0);
        // The live reader continues the same way and ends at the same position.
        assert_eq!(read_all(&mut *r), [(700_000, 'o', "c".into())]);
        assert_eq!(r.position(), resumed.position());
        assert_eq!(r.stats().bad_lines, 0);

        let zst = zst_copy(path.path());
        let mut resumed = open_at(&zst, ck, None).unwrap();
        assert_eq!(read_all(&mut *resumed), [(700_000, 'o', "c".into())]);
    }

    #[test]
    fn trailing_whitespace_after_tail_event() {
        let path = tempfile::NamedTempFile::new().unwrap();
        let mut f = path.reopen().unwrap();
        write!(f, "{HEADER}\n[0.1, \"o\", \"a\"]\r").unwrap();
        let mut r = open(path.path()).unwrap();
        assert_eq!(read_all(&mut *r).len(), 1);
        assert!(!r.stats().truncated_tail);
        let ck = ResumePoint {
            offset: r.position(),
            t_us: 100_000,
        };
        assert_eq!(ck.offset, std::fs::metadata(path.path()).unwrap().len() - 1);
        write!(f, "\n[0.1, \"o\", \"b\"]\n").unwrap();
        assert_eq!(read_all(&mut *r), [(200_000, 'o', "b".into())]);
        let mut resumed = open_at(path.path(), ck, None).unwrap();
        assert_eq!(read_all(&mut *resumed), [(200_000, 'o', "b".into())]);
        assert_eq!(r.position(), resumed.position());
    }

    #[test]
    fn nul_hole_in_unterminated_tail() {
        let mut cast = format!("{HEADER}\n[0.1, \"o\", \"a\"]\n[0.1, \"o\", \"partial").into_bytes();
        cast.extend_from_slice(&[0; 8192]);
        cast.extend_from_slice(b"[0.2,\"o\",\"after-hole\"]");
        let mut r = reader(&cast);
        let evs = read_all(&mut r);
        assert_eq!(evs, [(100_000, 'o', "a".into()), (300_000, 'o', "after-hole".into())]);
        let st = r.stats();
        assert_eq!(
            (st.events, st.bad_lines, st.nul_bytes, st.truncated_tail),
            (2, 1, 8192, false)
        );
        assert_eq!(r.position(), cast.len() as u64);
    }

    #[test]
    fn growing_nul_hole_is_counted_once() {
        let path = tempfile::NamedTempFile::new().unwrap();
        let mut f = path.reopen().unwrap();
        write!(f, "{HEADER}\n[0.1, \"o\", \"lo").unwrap();
        f.write_all(&[0; 100]).unwrap();
        let mut r = open(path.path()).unwrap();
        assert!(r.next_event().is_none());
        assert!(r.stats().truncated_tail);
        assert!(r.next_event().is_none());
        f.write_all(&[0; 50]).unwrap();
        write!(f, "[0.5, \"o\", \"b").unwrap();
        assert!(r.next_event().is_none());
        write!(f, "\"]\n[0.1, \"o\", \"c\"]\n").unwrap();
        assert_eq!(
            read_all(&mut *r),
            [(500_000, 'o', "b".into()), (600_000, 'o', "c".into())]
        );
        let st = r.stats();
        assert_eq!(
            (st.events, st.bad_lines, st.nul_bytes, st.truncated_tail),
            (2, 1, 150, false)
        );

        // A fresh reader over the finished file counts the same.
        let mut fresh = open(path.path()).unwrap();
        assert_eq!(read_all(&mut *fresh).len(), 2);
        assert_eq!(fresh.stats(), st);
    }
}
