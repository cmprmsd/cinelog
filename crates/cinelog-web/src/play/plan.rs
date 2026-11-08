//! Segmented playback plans (docs/API.md §6.2, §6.3; plan §7.5).
//!
//! A [`Plan`] cuts a session into segments, each with the terminal state at its start (an
//! avt dump) and the reader position its events start at. Indexed segments from index.db
//! are used as they are, as long as their dump cannot show anything forgotten (they start
//! before the first masked span); from the last usable one on, the rest of the cast is
//! read once with avt, masked, and cut about every 2 MiB or 60 s. A cast that is not
//! indexed yet is thus built entirely on demand (the 517 MB case: one pass, about 3–4 s).
//!
//! The player validates the format strictly; [`Plan::segment`] guarantees it: segment
//! `n > 0` starts with an event at exactly its start, every event lies in `[start, next)`
//! (last segment: `[start, duration]`), the last event is at exactly `duration`, and its
//! labels are exactly the index's markers in `[start, next)`.

use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};

use cinelog_cast::EventCode;
use cinelog_index::model::{PLAYBACK_SEGMENT_BYTES, PLAYBACK_SEGMENT_SECS};
use cinelog_proto::CastMarker;

use super::session::Loaded;
use super::source::{CastFiles, Events, Shift, StartPoint, clamp_size, parse_size, secs_to_us, sizes_at, us_to_secs};
use super::stream::{LabelMerge, Mask, OutEvent, Rewriter, label};
use crate::api::types::{PlayIndex, PlaySegment, PlaySegmentRef, PlaySnapshot, PlayTerm, PlayTheme};
use crate::error::{Result, WebError};

/// Bytes of event data counted per event line on top of its data (time, code, quoting).
const EVENT_OVERHEAD: u64 = 16;

/// Build progress 0…1, shared with the `/play` handler.
#[derive(Debug, Default)]
pub(crate) struct Progress(AtomicU64);

impl Progress {
    pub fn get(&self) -> f64 {
        f64::from_bits(self.0.load(Ordering::Relaxed))
    }

    pub fn set(&self, v: f64) {
        self.0.store(v.clamp(0.0, 1.0).to_bits(), Ordering::Relaxed);
    }
}

/// One playback segment.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct PlanSeg {
    /// Served start (µs of session time).
    pub start_us: u64,
    /// Where its events are read from.
    pub at: StartPoint,
    pub size: (u16, u16),
    /// Terminal state at `start_us`, masked.
    pub init: String,
    /// Mask placeholders already shown before `start_us`.
    pub shown: Vec<bool>,
}

#[derive(Debug, Clone)]
pub(crate) struct Plan {
    pub files: CastFiles,
    pub cols: u16,
    pub rows: u16,
    pub theme: Option<PlayTheme>,
    pub duration_us: u64,
    /// Reader position at the end of the data this plan covers (a live cast grows on).
    pub end: (usize, u64),
    /// Ascending, all `<= duration_us`.
    pub labels: Vec<(u64, String)>,
    pub mask: Mask,
    pub segments: Vec<PlanSeg>,
    /// What was forgotten when this plan was built (`Loaded::forgotten`).
    pub forgotten: String,
}

/// The cast header's `term.theme` when it has the asciicast v3 shape.
pub(crate) fn theme(header: &cinelog_cast::Header) -> Option<PlayTheme> {
    let t = header.term.theme.as_ref()?;
    let s = |k: &str| t.get(k).and_then(|v| v.as_str()).map(str::to_owned);
    Some(PlayTheme {
        fg: s("fg")?,
        bg: s("bg")?,
        palette: s("palette")?,
    })
}

/// A terminal of `size` (clamped, [`clamp_size`]) in the state `init` describes.
pub(crate) fn terminal(size: (u16, u16), init: &str) -> avt::Terminal {
    let (cols, rows) = clamp_size(size);
    let mut vt = avt::Terminal::new(usize::from(cols), usize::from(rows))
        .unwrap_or_else(|_| unreachable!("sizes are at least 1x1"));
    if !init.is_empty() {
        vt.feed(init);
    }
    vt
}

/// Feeds one served event to `vt` (output and resizes, clamped; everything else has no
/// effect).
pub(crate) fn apply(vt: &mut avt::Terminal, size: &mut (u16, u16), ev: &OutEvent) {
    match ev.code {
        'o' => vt.feed(ev.data.as_str()),
        'r' => {
            if let Some((c, r)) = parse_size(&ev.data) {
                *size = (c, r);
                vt.resize(usize::from(c), usize::from(r));
            }
        }
        _ => {}
    }
}

/// An indexed segment as a starting point of this session's reader.
fn db_start(files: &CastFiles, legacy: bool, seg: &cinelog_index::model::Segment) -> Option<(u64, StartPoint)> {
    let start_us = secs_to_us(seg.t_start);
    if seg.seq == 0 || start_us == 0 {
        return Some((0, StartPoint::ORIGIN));
    }
    let at = StartPoint {
        part_idx: files.index_of(seg.part)?,
        offset: seg.byte_offset,
        t_us: start_us,
        // cinelog segments are cut after an event (resume with its time); legacy segments
        // before one (the first event is at exactly the start).
        shift: if legacy {
            Shift::FirstAt(start_us)
        } else {
            Shift::Fixed(0)
        },
    };
    Some((start_us, at))
}

impl Plan {
    pub fn build(s: &Loaded, progress: &Progress) -> Result<Self> {
        let header = s.files.header()?;
        let size0 = clamp_size((header.term.cols, header.term.rows));
        let legacy = s.legacy();

        // Indexed segments whose dump cannot contain masked output.
        let first_masked = s.mask.first_start();
        let mut usable: Vec<(u64, StartPoint, String)> = Vec::new();
        for seg in &s.segments {
            let Some((start_us, at)) = db_start(&s.files, legacy, seg) else {
                break;
            };
            if first_masked.is_some_and(|m| start_us >= m) {
                break;
            }
            if usable
                .last()
                .is_some_and(|(prev, p, _)| start_us <= *prev || at.pos() <= p.pos())
            {
                continue;
            }
            usable.push((
                start_us,
                at,
                if start_us == 0 {
                    String::new()
                } else {
                    seg.vt_dump.clone()
                },
            ));
        }
        if usable.first().is_none_or(|(t, _, _)| *t != 0) {
            usable.insert(0, (0, StartPoint::ORIGIN, String::new()));
        }
        let bounds: Vec<(usize, u64)> = usable.iter().skip(1).map(|(_, at, _)| at.pos()).collect();
        let mut sizes = vec![size0];
        sizes.extend(sizes_at(&s.files, (0, 0, size0), &bounds)?);
        let mut segments: Vec<PlanSeg> = usable
            .into_iter()
            .zip(sizes)
            .map(|((start_us, at, init), size)| PlanSeg {
                start_us,
                at,
                size,
                init,
                shown: vec![false; s.mask.spans.len()],
            })
            .collect();

        // Read on from the last usable segment, cutting new ones.
        let tail = segments
            .pop()
            .unwrap_or_else(|| unreachable!("segment 0 always exists"));
        let total = s.files.bytes_after(tail.at.part_idx, tail.at.offset).max(1);
        let mut size = tail.size;
        let mut vt = terminal(size, &tail.init);
        let mut events = Events::open(&s.files, tail.at)?;
        let mut rw = Rewriter::new(&s.mask, tail.shown.clone(), false);
        segments.push(tail);
        let mut labels = s.labels.clone();
        let mut extra_labels = false;
        let mut seen: HashSet<u64> = s.label_seqs.clone();
        let (mut done, mut since_cut, mut cut_us) = (0u64, 0u64, segments.last().map_or(0, |g| g.start_us));
        let (mut prev_raw, mut prev_us): (Option<u64>, u64) = (None, 0);
        let mut duration_us = cut_us;
        loop {
            let before = events.pos();
            let Some(next) = events.next_event() else {
                break;
            };
            let (raw, ev) = next?;
            let line = ev.data.len() as u64 + EVENT_OVERHEAD;
            done += line;
            since_cut += line;
            let due = since_cut >= PLAYBACK_SEGMENT_BYTES
                || us_to_secs(ev.t_us.saturating_sub(cut_us)) >= PLAYBACK_SEGMENT_SECS;
            // Cut before this event: only where time moves on (events of the previous
            // segment must lie before the next start) and not at a part switch.
            if due
                && ev.t_us > prev_us
                && let Some(prev_raw) = prev_raw
                && before.0 == events.pos().0
            {
                segments.push(PlanSeg {
                    start_us: ev.t_us,
                    at: events.start_point(before, prev_raw),
                    size,
                    init: vt.dump(),
                    shown: rw.shown().to_vec(),
                });
                cut_us = ev.t_us;
                since_cut = line;
                progress.set(done as f64 / total as f64);
            }
            prev_raw = Some(raw);
            prev_us = ev.t_us;
            duration_us = duration_us.max(ev.t_us);
            if !legacy
                && ev.code == EventCode::Marker
                && let Ok(Some(CastMarker::Cmd(rec))) = CastMarker::from_json(&ev.data)
                && !s.mask.seqs.contains(&rec.seq)
                && s.mask.span_at(ev.t_us).is_none()
                && seen.insert(rec.seq)
            {
                labels.push((ev.t_us, label(&rec.cmd.typed)));
                extra_labels = true;
            }
            if let Some(out) = rw.push(ev) {
                apply(&mut vt, &mut size, &out);
            }
        }
        let end = events.pos();
        if extra_labels {
            labels.sort_by_key(|(t, _)| *t);
        }
        labels.retain(|(t, _)| *t <= duration_us);
        // Starts must increase strictly and lie before the duration (the first is 0).
        let mut kept: Vec<PlanSeg> = Vec::with_capacity(segments.len());
        for seg in segments {
            match kept.last() {
                None => kept.push(PlanSeg { start_us: 0, ..seg }),
                Some(last) if seg.start_us > last.start_us && seg.start_us < duration_us => kept.push(seg),
                Some(_) => {}
            }
        }
        progress.set(1.0);
        Ok(Self {
            files: s.files.clone(),
            cols: size0.0,
            rows: size0.1,
            theme: theme(&header),
            duration_us,
            end,
            labels,
            mask: s.mask.clone(),
            segments: kept,
            forgotten: s.forgotten.clone(),
        })
    }

    pub fn duration(&self) -> f64 {
        us_to_secs(self.duration_us)
    }

    pub fn index(&self) -> PlayIndex {
        PlayIndex {
            version: 1,
            duration: self.duration(),
            term: PlayTerm {
                cols: self.cols,
                rows: self.rows,
                theme: self.theme.clone(),
            },
            markers: self.labels.iter().map(|(t, l)| (us_to_secs(*t), l.clone())).collect(),
            segments: self
                .segments
                .iter()
                .enumerate()
                .map(|(i, s)| PlaySegmentRef {
                    start: us_to_secs(s.start_us),
                    url: format!("seg/{i}"),
                })
                .collect(),
        }
    }

    /// The last segment at or before `t_us`.
    pub fn segment_at(&self, t_us: u64) -> &PlanSeg {
        let i = self.segments.partition_point(|s| s.start_us <= t_us).saturating_sub(1);
        &self.segments[i]
    }

    /// Reader position where segment `n` ends.
    fn end_of(&self, n: usize) -> (usize, u64) {
        self.segments.get(n + 1).map_or(self.end, |s| s.at.pos())
    }

    /// The events of segment `n` (absolute µs), in the player's segment rules.
    pub fn segment_events(&self, n: usize) -> Result<Vec<OutEvent>> {
        let seg = self.segments.get(n).ok_or(WebError::NotFound)?;
        let last = n + 1 == self.segments.len();
        let start = seg.start_us;
        // Inclusive upper bound of event times.
        let max = match self.segments.get(n + 1) {
            Some(next) => next.start_us - 1,
            None => self.duration_us,
        };
        let end = self.end_of(n);
        let mut events = Events::open(&self.files, seg.at)?;
        let mut rw = Rewriter::new(&self.mask, seg.shown.clone(), false);
        let mut labels = LabelMerge::new(&self.labels, start);
        let mut out: Vec<OutEvent> = Vec::new();
        let clamp = |mut e: OutEvent| {
            e.t_us = e.t_us.clamp(start, max);
            e
        };
        while events.pos() < end {
            let Some(next) = events.next_event() else {
                break;
            };
            let (_, ev) = next?;
            // The segmented format has no exit event.
            if let Some(e) = rw.push(ev)
                && e.code != 'x'
            {
                let e = clamp(e);
                out.extend(labels.until(e.t_us));
                out.push(e);
            }
        }
        out.extend(labels.before(max + 1));
        if out.first().is_none_or(|e| e.t_us != start) {
            out.insert(0, OutEvent::new(start, 'o', ""));
        }
        if last && out.last().is_some_and(|e| e.t_us != self.duration_us) {
            out.push(OutEvent::new(self.duration_us, 'o', ""));
        }
        Ok(out)
    }

    pub fn segment(&self, n: usize) -> Result<PlaySegment> {
        let events = self.segment_events(n)?;
        let seg = &self.segments[n];
        Ok(PlaySegment {
            snapshot: PlaySnapshot {
                cols: seg.size.0,
                rows: seg.size.1,
                init: seg.init.clone(),
            },
            events: events
                .into_iter()
                .map(|e| (us_to_secs(e.t_us), e.code.to_string(), e.data))
                .collect(),
        })
    }
}
