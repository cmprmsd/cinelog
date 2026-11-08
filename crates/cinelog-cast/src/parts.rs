//! Rotation parts (`…_partN.cast`, §5.8): naming, discovery, stitching and the rotation
//! trigger.

use std::collections::VecDeque;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;

use crate::line::MAX_TIME_US;
use crate::reader::{ResumePoint, open, open_at};
use crate::{CastError, CastReader, Event, EventCode, Format, Header, ReadStats, format_resize, parse_resize};

/// Parsed cast file name: `<stem>.cast`, `<stem>_partN.cast`, optionally `.zst`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PartName {
    pub stem: String,
    /// 1 for the first part (no suffix).
    pub part: u32,
    pub compressed: bool,
}

impl PartName {
    /// Parses a file name; `None` if it is not a `.cast` / `.cast.zst` name.
    pub fn parse(file_name: &str) -> Option<Self> {
        let (rest, compressed) = match file_name.strip_suffix(".zst") {
            Some(r) => (r, true),
            None => (file_name, false),
        };
        let rest = rest.strip_suffix(".cast")?;
        if let Some((stem, num)) = rest.rsplit_once("_part")
            && !stem.is_empty()
            && !num.starts_with('0')
            && num.bytes().all(|b| b.is_ascii_digit())
            && let Ok(part @ 1..) = num.parse::<u32>()
        {
            return Some(Self {
                stem: stem.to_owned(),
                part,
                compressed,
            });
        }
        (!rest.is_empty()).then(|| Self {
            stem: rest.to_owned(),
            part: 1,
            compressed,
        })
    }

    pub fn file_name(&self) -> String {
        part_file_name(&self.stem, self.part, self.compressed)
    }
}

/// `<stem>.cast` for part 1, `<stem>_partN.cast` otherwise (plus `.zst` if compressed).
pub fn part_file_name(stem: &str, part: u32, compressed: bool) -> String {
    let zst = if compressed { ".zst" } else { "" };
    if part <= 1 {
        format!("{stem}.cast{zst}")
    } else {
        format!("{stem}_part{part}.cast{zst}")
    }
}

/// Plain (`.cast`) path of part `part` of the session that `path` (any part, plain or
/// compressed) belongs to. `None` if `path` is not a cast file name.
pub fn part_path(path: &Path, part: u32) -> Option<PathBuf> {
    let name = PartName::parse(path.file_name()?.to_str()?)?;
    Some(path.with_file_name(part_file_name(&name.stem, part, false)))
}

/// The existing file of part `part`: the plain cast if present (authoritative until the
/// compressor unlinks it), else the `.zst`.
pub fn existing_part(path: &Path, part: u32) -> Option<PathBuf> {
    let name = PartName::parse(path.file_name()?.to_str()?)?;
    [false, true]
        .into_iter()
        .map(|z| path.with_file_name(part_file_name(&name.stem, part, z)))
        .find(|p| p.is_file())
}

/// All parts of the session `path` belongs to, in order: a contiguous run starting at
/// part 1 (or at `path`'s own part if part 1 is gone). A non-cast name yields `[path]`.
pub fn find_parts(path: &Path) -> Vec<PathBuf> {
    let Some(name) = path.file_name().and_then(|n| n.to_str()).and_then(PartName::parse) else {
        return vec![path.to_path_buf()];
    };
    let first = if existing_part(path, 1).is_some() { 1 } else { name.part };
    let mut parts: Vec<PathBuf> = (first..).map_while(|n| existing_part(path, n)).collect();
    if parts.is_empty() {
        parts.push(path.to_path_buf());
    }
    parts
}

/// What `rotate_mb` / `rotate_days` decided.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RotateTrigger {
    Size,
    Age,
}

/// Rotation thresholds of a live cast (default 512 MiB or 7 days).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RotationPolicy {
    pub max_bytes: Option<u64>,
    pub max_age: Option<Duration>,
}

impl Default for RotationPolicy {
    fn default() -> Self {
        Self {
            max_bytes: Some(512 << 20),
            max_age: Some(Duration::from_secs(7 * 24 * 3600)),
        }
    }
}

impl RotationPolicy {
    /// `bytes` is [`crate::CastWriter::bytes_written`] of the current part, `age` its age.
    pub fn check(&self, bytes: u64, age: Duration) -> Option<RotateTrigger> {
        if self.max_bytes.is_some_and(|m| bytes >= m) {
            Some(RotateTrigger::Size)
        } else if self.max_age.is_some_and(|m| age >= m) {
            Some(RotateTrigger::Age)
        } else {
            None
        }
    }
}

/// Clock anchors of a part's `session` marker.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct PartClock {
    boottime_us: Option<u64>,
    start_epoch_us: Option<i64>,
}

impl PartClock {
    fn from_event(ev: &Event) -> Self {
        #[derive(Deserialize)]
        struct S<'a> {
            cl: Option<u64>,
            #[serde(borrow)]
            k: &'a str,
            boottime_us: Option<u64>,
            start_epoch_us: Option<i64>,
        }
        if ev.code != EventCode::Marker || ev.t_us != 0 {
            return Self::default();
        }
        match serde_json::from_str::<S<'_>>(&ev.data) {
            Ok(s) if s.cl.is_some() && s.k == "session" => Self {
                boottime_us: s.boottime_us.filter(|&b| b > 0),
                start_epoch_us: s.start_epoch_us,
            },
            _ => Self::default(),
        }
    }

    /// Offset of `self`'s t=0 relative to `base`'s t=0, if both carry a clock.
    fn offset_from(&self, base: &PartClock) -> Option<u64> {
        if let (Some(b), Some(s)) = (base.boottime_us, self.boottime_us) {
            return s.checked_sub(b);
        }
        let (b, s) = (base.start_epoch_us?, self.start_epoch_us?);
        u64::try_from(s.checked_sub(b)?).ok()
    }
}

/// Reads the parts of one session as one stream.
///
/// Times of part N are shifted by the part's offset: the `CLOCK_BOOTTIME` (else wall
/// clock) distance between its `session` marker and part 1's, never less than the last
/// event time so far (plain concatenation is the fallback). If a part's header size
/// differs from the current terminal size, a synthetic `r` event is inserted at the
/// boundary. All events, including later parts' `session` markers, are passed through.
///
/// Errors while moving to the next part:
/// - A part that cannot be read as a cast (bad or empty header, or missing even after
///   looking for its compressed/plain sibling) is skipped and counted as one bad line;
///   its offset stays `None`. If it is the last part it is retried on the next call
///   instead (a live part whose header is not written yet) and `truncated_tail` is set.
/// - Any other I/O error is returned once; the reader then ends (`None`).
pub struct PartsReader {
    paths: Vec<PathBuf>,
    idx: usize,
    /// Index of the next part to open (beyond `idx` when parts were skipped).
    next: usize,
    bad_parts: u64,
    /// The last part could not be opened yet.
    tail_pending: bool,
    failed: bool,
    cur: Box<dyn CastReader + Send>,
    header: Header,
    format: Format,
    base: PartClock,
    /// Offsets by part index; `None` for parts before a resume point.
    offsets: Vec<Option<u64>>,
    last_t: u64,
    size: (u16, u16),
    pending: VecDeque<Event>,
    pending_pos: Option<u64>,
    done_stats: ReadStats,
}

impl PartsReader {
    /// Opens all parts of the session that `path` belongs to ([`find_parts`]).
    pub fn open(path: &Path) -> Result<Self, CastError> {
        Self::from_paths(find_parts(path))
    }

    /// Reads `paths` in order as parts of one session.
    pub fn from_paths(paths: Vec<PathBuf>) -> Result<Self, CastError> {
        let first = paths.first().ok_or_else(|| CastError::Format("no cast parts".into()))?;
        let cur = open(first)?;
        let mut me = Self::with_reader(paths, 0, cur, 0, 0);
        me.peek_first(None)?;
        Ok(me)
    }

    /// Resumes at part index `part_index` (into `paths`), whose offset is `part_offset_us`
    /// (from [`Self::part_offsets`]); `at` is this reader's position and absolute time.
    pub fn resume(
        paths: Vec<PathBuf>,
        part_index: usize,
        part_offset_us: u64,
        at: ResumePoint,
    ) -> Result<Self, CastError> {
        let path = paths
            .get(part_index)
            .ok_or_else(|| CastError::Format(format!("no cast part #{part_index}")))?;
        let local = ResumePoint {
            offset: at.offset,
            t_us: at.t_us.saturating_sub(part_offset_us),
        };
        let cur = open_at(path, local, None)?;
        let mut me = Self::with_reader(paths.clone(), part_index, cur, part_offset_us, at.t_us);
        if let Some(first) = paths.first() {
            let mut r = open(first)?;
            if let Some(ev) = r.next_event().transpose()? {
                me.base = PartClock::from_event(&ev);
            }
        }
        Ok(me)
    }

    fn with_reader(paths: Vec<PathBuf>, idx: usize, cur: Box<dyn CastReader + Send>, offset: u64, last_t: u64) -> Self {
        let header = cur.header().clone();
        let mut offsets = vec![None; idx];
        offsets.push(Some(offset));
        Self {
            idx,
            next: idx + 1,
            bad_parts: 0,
            tail_pending: false,
            failed: false,
            format: cur.format(),
            size: (header.term.cols, header.term.rows),
            header,
            cur,
            base: PartClock::default(),
            offsets,
            last_t,
            pending: VecDeque::new(),
            pending_pos: None,
            done_stats: ReadStats::default(),
            paths,
        }
    }

    /// Reads the first event of the current part to learn its clock and offset.
    fn peek_first(&mut self, prev_end: Option<u64>) -> Result<(), CastError> {
        let pos = self.cur.position();
        let first = self.cur.next_event().transpose()?;
        let clock = first.as_ref().map(PartClock::from_event).unwrap_or_default();
        let offset = match prev_end {
            None => {
                self.base = clock;
                0
            }
            Some(end) => clock
                .offset_from(&self.base)
                .filter(|&o| o <= MAX_TIME_US)
                .unwrap_or(end)
                .max(end),
        };
        if let Some(last) = self.offsets.last_mut() {
            *last = Some(offset);
        }
        if prev_end.is_some() {
            let size = (self.cur.header().term.cols, self.cur.header().term.rows);
            if size != self.size {
                self.size = size;
                self.pending.push_back(Event {
                    t_us: offset,
                    code: EventCode::Resize,
                    data: format_resize(size.0, size.1),
                });
            }
        }
        if let Some(mut ev) = first {
            ev.t_us = ev.t_us.saturating_add(offset);
            self.pending.push_back(ev);
        }
        self.pending_pos = (!self.pending.is_empty()).then_some(pos);
        Ok(())
    }

    /// Opens part `n`; if its file is gone (compressed and unlinked meanwhile), retries
    /// with the file that exists now and records it in `paths`.
    fn open_part(&mut self, n: usize) -> Result<Box<dyn CastReader + Send>, CastError> {
        match open(&self.paths[n]) {
            Err(CastError::Io(e)) if e.kind() == io::ErrorKind::NotFound => {
                let path = &self.paths[n];
                let other = path
                    .file_name()
                    .and_then(|f| f.to_str())
                    .and_then(PartName::parse)
                    .and_then(|name| existing_part(path, name.part))
                    .filter(|other| other != path)
                    .ok_or(CastError::Io(e))?;
                let r = open(&other)?;
                self.paths[n] = other;
                Ok(r)
            }
            r => r,
        }
    }

    /// Moves to the next readable part. `Ok(false)` when there is none (yet).
    fn advance_part(&mut self) -> Result<bool, CastError> {
        loop {
            let n = self.next;
            if n >= self.paths.len() {
                return Ok(false);
            }
            let next = match self.open_part(n) {
                Ok(r) => r,
                Err(CastError::Io(e)) if e.kind() != io::ErrorKind::NotFound => return Err(e.into()),
                Err(_) if n + 1 == self.paths.len() => {
                    self.tail_pending = true;
                    return Ok(false);
                }
                Err(_) => {
                    self.bad_parts += 1;
                    self.next += 1;
                    continue;
                }
            };
            self.tail_pending = false;
            let st = self.cur.stats();
            self.done_stats.events += st.events;
            self.done_stats.bad_lines += st.bad_lines;
            self.done_stats.nul_bytes += st.nul_bytes;
            self.idx = n;
            self.next = n + 1;
            self.cur = next;
            self.offsets.resize(n, None);
            self.offsets.push(Some(self.last_t));
            self.peek_first(Some(self.last_t))?;
            return Ok(true);
        }
    }

    fn emit(&mut self, ev: Event) -> Event {
        self.last_t = self.last_t.max(ev.t_us);
        if let (EventCode::Resize, Some(size)) = (ev.code, parse_resize(&ev.data)) {
            self.size = size;
        }
        ev
    }

    /// Index into the paths of the part being read.
    pub fn part_index(&self) -> usize {
        self.idx
    }

    pub fn paths(&self) -> &[PathBuf] {
        &self.paths
    }

    /// Time offsets by part index of the parts opened so far (`cast_parts.t_offset`);
    /// `None` for parts before the resume point of [`Self::resume`] and for skipped parts.
    pub fn part_offsets(&self) -> &[Option<u64>] {
        &self.offsets
    }

    /// Offset of the current part.
    pub fn part_offset(&self) -> u64 {
        self.offsets.last().copied().flatten().unwrap_or(0)
    }
}

impl CastReader for PartsReader {
    fn header(&self) -> &Header {
        &self.header
    }

    fn format(&self) -> Format {
        self.format
    }

    /// Errors of the current part are passed through (that part's reader then ends and
    /// reading continues with the next part); see [`PartsReader`] for part errors.
    fn next_event(&mut self) -> Option<Result<Event, CastError>> {
        if self.failed {
            return None;
        }
        loop {
            if let Some(ev) = self.pending.pop_front() {
                if self.pending.is_empty() {
                    self.pending_pos = None;
                }
                return Some(Ok(self.emit(ev)));
            }
            match self.cur.next_event() {
                Some(Ok(mut ev)) => {
                    ev.t_us = ev.t_us.saturating_add(self.part_offset());
                    return Some(Ok(self.emit(ev)));
                }
                Some(Err(e)) => return Some(Err(e)),
                None => match self.advance_part() {
                    Ok(true) => {}
                    Ok(false) => return None,
                    Err(e) => {
                        self.failed = true;
                        return Some(Err(e));
                    }
                },
            }
        }
    }

    /// Position inside the current part ([`Self::part_index`]).
    fn position(&self) -> u64 {
        self.pending_pos.unwrap_or_else(|| self.cur.position())
    }

    fn stats(&self) -> ReadStats {
        let st = self.cur.stats();
        ReadStats {
            events: self.done_stats.events + st.events,
            bad_lines: self.done_stats.bad_lines + st.bad_lines + self.bad_parts,
            nul_bytes: self.done_stats.nul_bytes + st.nul_bytes,
            truncated_tail: st.truncated_tail || self.tail_pending,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{CastWriter, Header, create};

    #[test]
    fn names() {
        let p = PartName::parse("host_20261108T101500.123456_0193abcd.cast").unwrap();
        assert_eq!(
            (p.stem.as_str(), p.part, p.compressed),
            ("host_20261108T101500.123456_0193abcd", 1, false)
        );
        let p = PartName::parse("h_t_u_part12.cast.zst").unwrap();
        assert_eq!((p.stem.as_str(), p.part, p.compressed), ("h_t_u", 12, true));
        assert_eq!(p.file_name(), "h_t_u_part12.cast.zst");
        assert_eq!(PartName::parse("x_part0.cast").unwrap().stem, "x_part0");
        assert_eq!(PartName::parse("x_part02.cast").unwrap().part, 1);
        assert_eq!(PartName::parse("x_part.cast").unwrap().stem, "x_part");
        assert_eq!(PartName::parse("_part2.cast").unwrap().stem, "_part2");
        assert_eq!(PartName::parse("x_part1.cast").unwrap().part, 1);
        assert_eq!(PartName::parse("x.cast.commands.log"), None);
        assert_eq!(PartName::parse(".cast"), None);
        assert_eq!(part_file_name("s", 1, false), "s.cast");
        assert_eq!(part_file_name("s", 3, true), "s_part3.cast.zst");
        assert_eq!(
            part_path(Path::new("/l/2026-11/s_part2.cast.zst"), 5).unwrap(),
            Path::new("/l/2026-11/s_part5.cast")
        );
        assert_eq!(part_path(Path::new("/l/s.cast"), 1).unwrap(), Path::new("/l/s.cast"));
    }

    #[test]
    fn policy() {
        let p = RotationPolicy::default();
        assert_eq!(p.check(0, Duration::ZERO), None);
        assert_eq!(p.check(512 << 20, Duration::ZERO), Some(RotateTrigger::Size));
        assert_eq!(p.check(1, Duration::from_secs(7 * 86400)), Some(RotateTrigger::Age));
        let off = RotationPolicy {
            max_bytes: None,
            max_age: None,
        };
        assert_eq!(off.check(u64::MAX, Duration::MAX), None);
    }

    fn session(part: u32, boottime_us: u64, epoch_us: i64) -> String {
        format!(
            r#"{{"cl":1,"k":"session","uuid":"u","part":{part},"start_epoch_us":{epoch_us},"boottime_us":{boottime_us}}}"#
        )
    }

    fn write_part(path: &Path, size: (u16, u16), events: &[(u64, EventCode, String)]) {
        let mut w = create(path).unwrap();
        w.write_header(&Header::new_v3(size.0, size.1, None, 1, None)).unwrap();
        for (t, c, d) in events {
            w.write_event(*t, *c, d).unwrap();
        }
        w.flush().unwrap();
    }

    fn collect(r: &mut PartsReader) -> Vec<(u64, char, String)> {
        std::iter::from_fn(|| r.next_event())
            .map(Result::unwrap)
            .map(|e| (e.t_us, e.code.as_char(), e.data))
            .collect()
    }

    #[test]
    fn stitches_parts_by_boottime() {
        let dir = tempfile::tempdir().unwrap();
        let p1 = dir.path().join("s.cast");
        let p2 = dir.path().join("s_part2.cast");
        let p3 = dir.path().join("s_part3.cast");
        write_part(
            &p1,
            (80, 24),
            &[
                (0, EventCode::Marker, session(1, 1_000_000, 5_000_000)),
                (1_000_000, EventCode::Output, "a".into()),
                (
                    9_000_000,
                    EventCode::Marker,
                    r#"{"cl":1,"k":"rotate","part":1,"next":"s_part2.cast","reason":"size"}"#.into(),
                ),
            ],
        );
        write_part(
            &p2,
            (100, 30),
            &[
                (0, EventCode::Marker, session(2, 10_000_000, 14_000_000)),
                (500_000, EventCode::Output, "b".into()),
            ],
        );
        write_part(
            &p3,
            (100, 30),
            &[
                (0, EventCode::Marker, session(3, 0, 25_000_000)),
                (1, EventCode::Output, "c".into()),
            ],
        );
        // A compressed duplicate of part 2 is ignored while the plain file exists.
        crate::compress_file(&p2, &dir.path().join("s_part2.cast.zst"), Default::default()).unwrap();

        assert_eq!(find_parts(&p3), [p1.clone(), p2.clone(), p3.clone()]);
        let mut r = PartsReader::open(&p2).unwrap();
        assert_eq!(r.header().term.cols, 80);
        let evs = collect(&mut r);
        let times: Vec<(u64, char)> = evs.iter().map(|e| (e.0, e.1)).collect();
        assert_eq!(
            times,
            [
                (0, 'm'),
                (1_000_000, 'o'),
                (9_000_000, 'm'),
                (9_000_000, 'r'),
                (9_000_000, 'm'),
                (9_500_000, 'o'),
                // Part 3 has no boottime: wall-clock distance from part 1.
                (20_000_000, 'm'),
                (20_000_001, 'o'),
            ]
        );
        assert_eq!(evs[3].2, "100x30");
        assert_eq!(r.part_offsets(), [Some(0), Some(9_000_000), Some(20_000_000)]);
        assert_eq!(r.stats().events, 7);
    }

    #[test]
    fn concatenates_without_session_markers_and_resumes() {
        let dir = tempfile::tempdir().unwrap();
        let p1 = dir.path().join("x.cast");
        let p2 = dir.path().join("x_part2.cast");
        write_part(&p1, (80, 24), &[(2_000_000, EventCode::Output, "a".into())]);
        write_part(
            &p2,
            (80, 24),
            &[
                (1_000_000, EventCode::Output, "b".into()),
                (3_000_000, EventCode::Output, "c".into()),
            ],
        );
        let mut r = PartsReader::open(&p1).unwrap();
        assert_eq!(r.next_event().unwrap().unwrap().t_us, 2_000_000);
        let b = r.next_event().unwrap().unwrap();
        assert_eq!((b.t_us, r.part_index(), r.part_offset()), (3_000_000, 1, 2_000_000));
        let at = ResumePoint {
            offset: r.position(),
            t_us: b.t_us,
        };
        assert_eq!(collect(&mut r), [(5_000_000, 'o', "c".into())]);

        let mut resumed = PartsReader::resume(find_parts(&p1), 1, 2_000_000, at).unwrap();
        assert_eq!(resumed.part_offsets(), [None, Some(2_000_000)]);
        assert_eq!(collect(&mut resumed), [(5_000_000, 'o', "c".into())]);
    }

    #[test]
    fn huge_corrupt_times_do_not_overflow_stitching() {
        let dir = tempfile::tempdir().unwrap();
        let p1 = dir.path().join("z.cast");
        let p2 = dir.path().join("z_part2.cast");
        let p3 = dir.path().join("z_part3.cast");
        std::fs::write(
            &p1,
            format!(
                "{}\n[1.0, \"m\", {}]\n[18446744073709, \"o\", \"\"]\n[0.5, \"o\", \"a\"]\n",
                r#"{"version":3,"term":{"cols":80,"rows":24}}"#,
                serde_json::to_string(&session(1, 0, 0)).unwrap()
            ),
        )
        .unwrap();
        // A session marker claiming an absurd boot time distance falls back to concatenation.
        write_part(&p2, (80, 24), &[(0, EventCode::Marker, session(2, u64::MAX, 0))]);
        write_part(&p3, (80, 24), &[(1_000_000, EventCode::Output, "b".into())]);

        let mut r = PartsReader::open(&p1).unwrap();
        let times: Vec<u64> = collect(&mut r).iter().map(|e| e.0).collect();
        assert_eq!(times, [1_000_000, 1_500_000, 1_500_000, 2_500_000]);
        assert_eq!(r.stats().bad_lines, 1);

        let at = ResumePoint {
            offset: 0,
            t_us: u64::MAX,
        };
        let mut resumed = PartsReader::resume(find_parts(&p1), 2, u64::MAX - 10, at).unwrap();
        assert_eq!(collect(&mut resumed)[0].0, u64::MAX);
    }

    #[test]
    fn position_accounts_for_peeked_event() {
        let dir = tempfile::tempdir().unwrap();
        let p1 = dir.path().join("y.cast");
        write_part(
            &p1,
            (80, 24),
            &[(0, EventCode::Output, "a".into()), (1, EventCode::Output, "b".into())],
        );
        let mut r = PartsReader::open(&p1).unwrap();
        let header_len = std::fs::read(&p1).unwrap().iter().position(|&b| b == b'\n').unwrap() as u64 + 1;
        assert_eq!(r.position(), header_len);
        r.next_event().unwrap().unwrap();
        assert!(r.position() > header_len);
        assert!(PartsReader::from_paths(Vec::new()).is_err());
    }

    #[test]
    fn corrupt_middle_part_is_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let p1 = dir.path().join("c.cast");
        let p2 = dir.path().join("c_part2.cast");
        let p3 = dir.path().join("c_part3.cast");
        write_part(&p1, (80, 24), &[(1_000_000, EventCode::Output, "a".into())]);
        std::fs::write(&p2, b"{not a header\n[0.1, \"o\", \"lost\"]\n").unwrap();
        write_part(&p3, (80, 24), &[(500_000, EventCode::Output, "c".into())]);
        let mut r = PartsReader::open(&p1).unwrap();
        let mut seen = Vec::new();
        for _ in 0..10 {
            match r.next_event() {
                Some(Ok(ev)) => seen.push((ev.t_us, ev.data)),
                Some(Err(e)) => panic!("unexpected error {e}"),
                None => break,
            }
        }
        assert_eq!(seen, [(1_000_000, "a".into()), (1_500_000, "c".into())]);
        assert!(r.next_event().is_none());
        assert_eq!(r.part_index(), 2);
        assert_eq!(r.part_offsets(), [Some(0), None, Some(1_000_000)]);
        let st = r.stats();
        assert_eq!((st.events, st.bad_lines, st.truncated_tail), (2, 1, false));
    }

    #[test]
    fn unwritten_last_part_is_retried() {
        let dir = tempfile::tempdir().unwrap();
        let p1 = dir.path().join("l.cast");
        let p2 = dir.path().join("l_part2.cast");
        write_part(&p1, (80, 24), &[(1_000_000, EventCode::Output, "a".into())]);
        std::fs::write(&p2, b"").unwrap();
        let mut r = PartsReader::open(&p1).unwrap();
        assert_eq!(r.next_event().unwrap().unwrap().data, "a");
        assert!(r.next_event().is_none());
        assert!(r.next_event().is_none());
        assert!(r.stats().truncated_tail);
        assert_eq!(r.stats().bad_lines, 0);

        std::fs::remove_file(&p2).unwrap();
        write_part(&p2, (80, 24), &[(2, EventCode::Output, "b".into())]);
        let ev = r.next_event().unwrap().unwrap();
        assert_eq!((ev.t_us, ev.data.as_str()), (1_000_002, "b"));
        assert!(r.next_event().is_none());
        assert!(!r.stats().truncated_tail);
    }

    #[test]
    fn part_compressed_while_reading_is_found() {
        let dir = tempfile::tempdir().unwrap();
        let p1 = dir.path().join("z.cast");
        let p2 = dir.path().join("z_part2.cast");
        let p3 = dir.path().join("z_part3.cast");
        write_part(&p1, (80, 24), &[(1, EventCode::Output, "a".into())]);
        write_part(&p2, (80, 24), &[(1, EventCode::Output, "b".into())]);
        write_part(&p3, (80, 24), &[(1, EventCode::Output, "c".into())]);
        let mut r = PartsReader::open(&p1).unwrap();
        assert_eq!(r.next_event().unwrap().unwrap().data, "a");
        // The compressor replaces part 2; part 3 vanishes without a replacement.
        crate::compress_file(&p2, &dir.path().join("z_part2.cast.zst"), Default::default()).unwrap();
        std::fs::remove_file(&p2).unwrap();
        std::fs::remove_file(&p3).unwrap();
        assert_eq!(r.next_event().unwrap().unwrap().data, "b");
        assert!(r.next_event().is_none());
        assert!(r.next_event().is_none());
        assert_eq!(r.paths()[1], dir.path().join("z_part2.cast.zst"));
        assert!(r.stats().truncated_tail);
    }
}
