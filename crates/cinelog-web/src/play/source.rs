//! The cast files of a session and readers over them: every rotation part in order, plain
//! or zstd (`cinelog_cast::PartsReader`), resumable at a playback segment's byte offset.

use std::fs::File;
use std::io::{BufRead, BufReader, Read};
use std::path::PathBuf;

use cinelog_cast::{
    CastError, CastReader, Event, EventCode, Format, Header, PartsReader, ResumePoint, existing_part, is_zstd_magic,
    parse_event_line, parse_sane_resize, read_header, sane_size,
};

use crate::api::db::{IndexDb, SessionRow};
use crate::error::{Result, WebError};

/// A cast read error as a response: a vanished file is 404, anything else internal.
pub(crate) fn cast_err(e: CastError) -> WebError {
    match e {
        CastError::Io(io) if io.kind() == std::io::ErrorKind::NotFound => WebError::NotFound,
        e => WebError::Index(cinelog_index::IndexError::Cast(e)),
    }
}

/// One rotation part.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct PartFile {
    pub part: u32,
    pub path: PathBuf,
    /// Uncompressed size: the file size of a plain part, the frame total of a zstd part.
    pub bytes: u64,
    /// Session time at the part's t=0.
    pub t_offset_us: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct CastFiles {
    pub parts: Vec<PartFile>,
}

/// Seconds to whole µs (negative and non-finite values become 0).
pub(crate) fn secs_to_us(t: f64) -> u64 {
    if t.is_finite() && t > 0.0 {
        (t * 1e6).round() as u64
    } else {
        0
    }
}

pub(crate) fn us_to_secs(t: u64) -> f64 {
    t as f64 / 1e6
}

/// A size from a header or index.db as served and emulated: itself when it is a
/// [`sane_size`], otherwise [`cinelog_cast::FALLBACK_SIZE`]. avt allocates every cell of
/// its screen, and the recorder stores the tty's size as it is (`stty cols 65535 rows
/// 65535` is 4e9 cells).
pub(crate) fn plausible_size((cols, rows): (u16, u16)) -> (u16, u16) {
    sane_size(cols, rows).unwrap_or(cinelog_cast::FALLBACK_SIZE)
}

/// The size of resize data `COLSxROWS`; `None` when it is not one, or not a [`sane_size`]
/// (a resize to 3350 columns from the old plugin's tmux bugs): such resizes are dropped,
/// not clamped, so the player keeps a readable size.
pub(crate) fn parse_size(data: &str) -> Option<(u16, u16)> {
    parse_sane_resize(data)
}

fn is_zstd(path: &std::path::Path) -> std::io::Result<bool> {
    let mut magic = [0u8; 4];
    let mut f = File::open(path)?;
    let mut n = 0;
    while n < magic.len() {
        match f.read(&mut magic[n..])? {
            0 => break,
            k => n += k,
        }
    }
    Ok(n == magic.len() && is_zstd_magic(&magic))
}

impl CastFiles {
    /// The parts recorded in index.db (the first part alone when none are). A part that was
    /// compressed or decompressed since it was indexed is found under its other name.
    pub fn load(db: &IndexDb, row: &SessionRow) -> Result<Self> {
        let mut rows = db.parts(row.id)?;
        if rows.is_empty() {
            rows.push(cinelog_index::model::CastPart {
                part: 1,
                path: row.cast_path.clone(),
                compressed: row.session.compressed,
                bytes: row.session.cast_bytes,
                t_offset: 0.0,
                frames: None,
            });
        }
        let mut parts = Vec::with_capacity(rows.len());
        for r in rows {
            let path = if r.path.exists() {
                r.path
            } else {
                match existing_part(&r.path, r.part) {
                    Some(p) => p,
                    // A part that is gone (deleted, or never written) ends the session.
                    None if !parts.is_empty() => break,
                    None => return Err(WebError::NotFound),
                }
            };
            let bytes = if is_zstd(&path)? {
                match r.bytes {
                    Some(b) => b,
                    None => {
                        let frames = db.frames(row.id, r.part)?;
                        match frames.last() {
                            Some(f) => f.u_offset + f.u_len,
                            None => {
                                let frames = cinelog_cast::scan_frames(&mut File::open(&path)?)?;
                                frames.last().map_or(0, |f| f.u_offset + f.u_len)
                            }
                        }
                    }
                }
            } else {
                std::fs::metadata(&path)?.len()
            };
            parts.push(PartFile {
                part: r.part,
                path,
                bytes,
                t_offset_us: secs_to_us(r.t_offset),
            });
        }
        Ok(Self { parts })
    }

    pub fn total_bytes(&self) -> u64 {
        self.parts.iter().map(|p| p.bytes).sum()
    }

    pub fn paths(&self) -> Vec<PathBuf> {
        self.parts.iter().map(|p| p.path.clone()).collect()
    }

    /// Index into [`Self::parts`] of part number `part`.
    pub fn index_of(&self, part: u32) -> Option<usize> {
        self.parts.iter().position(|p| p.part == part)
    }

    /// Uncompressed bytes from `at` to the end of the last part.
    pub fn bytes_after(&self, part_idx: usize, offset: u64) -> u64 {
        self.parts
            .iter()
            .enumerate()
            .filter(|(i, _)| *i >= part_idx)
            .map(|(i, p)| {
                if i == part_idx {
                    p.bytes.saturating_sub(offset)
                } else {
                    p.bytes
                }
            })
            .sum()
    }

    /// The header of the first part.
    pub fn header(&self) -> Result<Header> {
        let first = self.parts.first().ok_or(WebError::NotFound)?;
        Ok(cinelog_cast::open(&first.path).map_err(cast_err)?.header().clone())
    }
}

/// Where a reader starts: a line boundary in a part, with the reader's absolute time there.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct StartPoint {
    pub part_idx: usize,
    pub offset: u64,
    /// Absolute session time (µs) the reader resumes with (the time of the event before the
    /// offset; only v3 intervals depend on it).
    pub t_us: u64,
    pub shift: Shift,
}

/// Correction of the reader's times.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Shift {
    /// Add this many µs.
    Fixed(i64),
    /// The first event is at exactly this time (legacy segments are cut before an event;
    /// for v3 casts the resume time is that of the event before, which is unknown).
    FirstAt(u64),
}

impl StartPoint {
    /// The beginning of the cast.
    pub const ORIGIN: Self = Self {
        part_idx: 0,
        offset: 0,
        t_us: 0,
        shift: Shift::Fixed(0),
    };

    /// Total order of reader positions.
    pub fn pos(&self) -> (usize, u64) {
        (self.part_idx, self.offset)
    }
}

/// Events of a session from a [`StartPoint`] on, times corrected.
pub(crate) struct Events {
    reader: PartsReader,
    shift: Option<i64>,
    first_at: Option<u64>,
}

impl Events {
    pub fn open(files: &CastFiles, at: StartPoint) -> Result<Self> {
        let paths = files.paths();
        if paths.is_empty() {
            return Err(WebError::NotFound);
        }
        let reader = if at.pos() == (0, 0) {
            PartsReader::from_paths(paths).map_err(cast_err)?
        } else {
            // The part offset is whatever makes `at.t_us` absolute; PartsReader only uses it
            // to shift later events of this part, which the resume time already includes.
            let part_offset = part_offset_us(files, at)?;
            PartsReader::resume(
                paths,
                at.part_idx,
                part_offset,
                ResumePoint {
                    offset: at.offset,
                    t_us: at.t_us,
                },
            )
            .map_err(cast_err)?
        };
        let (shift, first_at) = match at.shift {
            Shift::Fixed(s) => (Some(s), None),
            Shift::FirstAt(t) if reader.format() == Format::V3 => (None, Some(t)),
            Shift::FirstAt(_) => (Some(0), None),
        };
        Ok(Self {
            reader,
            shift,
            first_at,
        })
    }

    /// Reader position of the next unread event.
    pub fn pos(&self) -> (usize, u64) {
        (self.reader.part_index(), self.reader.position())
    }

    /// A resume point at reader position `pos` (from [`Self::pos`]), where the event before
    /// had raw reader time `t_raw`.
    pub fn start_point(&self, pos: (usize, u64), t_raw: u64) -> StartPoint {
        StartPoint {
            part_idx: pos.0,
            offset: pos.1,
            t_us: t_raw,
            shift: Shift::Fixed(self.shift.unwrap_or(0)),
        }
    }

    pub fn header(&self) -> &Header {
        self.reader.header()
    }

    /// The next event as `(raw reader time, event with corrected time)`.
    pub fn next_event(&mut self) -> Option<Result<(u64, Event)>> {
        let mut ev = match self.reader.next_event()? {
            Ok(ev) => ev,
            Err(e) => return Some(Err(cast_err(e))),
        };
        let raw = ev.t_us;
        let shift = *self
            .shift
            .get_or_insert_with(|| self.first_at.map_or(0, |t| t as i64 - raw as i64));
        ev.t_us = ev.t_us.saturating_add_signed(shift);
        Some(Ok((raw, ev)))
    }
}

/// The time offset (µs) of the part `at` resumes in, as `PartsReader::resume` wants it.
/// Absolute times are `local + offset`: v3 casts only need the offset to stay within the
/// resume time (intervals continue from it), v2 casts carry part-local absolute times and
/// need the real one (cinelog writes v3; legacy v2 casts have a single part).
fn part_offset_us(files: &CastFiles, at: StartPoint) -> Result<u64> {
    let part = files.parts.get(at.part_idx).ok_or(WebError::NotFound)?;
    Ok(part.t_offset_us.min(at.t_us))
}

/// Terminal size before each of `bounds` (reader positions, ascending): the first part's
/// header size, changed by `r` events and by the header size of every later part. A raw
/// line scan (no data decoding except for `r` events).
pub(crate) fn sizes_at(
    files: &CastFiles,
    from: (usize, u64, (u16, u16)),
    bounds: &[(usize, u64)],
) -> Result<Vec<(u16, u16)>> {
    let mut out = Vec::with_capacity(bounds.len());
    let (start_idx, start_off, mut size) = from;
    let mut next = 0;
    let mut line = Vec::new();
    for (idx, part) in files.parts.iter().enumerate().skip(start_idx) {
        if next >= bounds.len() {
            break;
        }
        let file = File::open(&part.path)?;
        let mut src: Box<dyn BufRead> = if is_zstd(&part.path)? {
            Box::new(BufReader::with_capacity(
                256 << 10,
                zstd::stream::read::Decoder::new(file)?,
            ))
        } else {
            Box::new(BufReader::with_capacity(256 << 10, file))
        };
        let (header, _, mut pos) = read_header(&mut src).map_err(cast_err)?;
        if idx > start_idx {
            // A later part's implausible header is dropped like its synthetic resize
            // (`PartsReader`) is: the size carries over.
            if let Some(s) = sane_size(header.term.cols, header.term.rows) {
                size = s;
            }
        }
        if idx == start_idx && start_off > pos {
            std::io::copy(&mut (&mut src).take(start_off - pos), &mut std::io::sink())?;
            pos = start_off;
        }
        loop {
            while next < bounds.len() && bounds[next] <= (idx, pos) {
                out.push(size);
                next += 1;
            }
            if next >= bounds.len() || bounds[next].0 != idx {
                break;
            }
            line.clear();
            let n = src.read_until(b'\n', &mut line)? as u64;
            if n == 0 {
                break;
            }
            pos += n;
            if let Some(ev) = parse_event_line(line.trim_ascii())
                && ev.code == EventCode::Resize
                && let Some(s) = parse_size(&ev.decode_data())
            {
                size = s;
            }
        }
        // Bounds past the end of this part's data (a segment that starts at its end).
        while next < bounds.len() && bounds[next].0 == idx {
            out.push(size);
            next += 1;
        }
    }
    out.resize(bounds.len(), size);
    Ok(out)
}
