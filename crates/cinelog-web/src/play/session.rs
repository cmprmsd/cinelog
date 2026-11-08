//! Everything playback and export need to know about one session: its files, what is
//! forgotten in it (as a [`Mask`]), its chapter labels and its indexed playback segments.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Mutex;

use cinelog_cast::EventCode;
use cinelog_index::model as im;
use cinelog_proto::CastMarker;

use super::source::{CastFiles, Events, StartPoint, secs_to_us, us_to_secs};
use super::stream::{Mask, Span, label};
use crate::api::db::{CmdSpan, Forgets, IndexDb, SessionRow, record_span};
use crate::api::types::PlayMode;
use crate::error::Result;
use crate::state::WebContext;

pub(crate) struct Loaded {
    pub row: SessionRow,
    pub files: CastFiles,
    pub mask: Mask,
    /// Chapters from the index (commands that are not forgotten), ascending.
    pub labels: Vec<(u64, String)>,
    /// Seqs of `labels`.
    pub label_seqs: HashSet<u64>,
    /// Indexed playback segments, `seq` order.
    pub segments: Vec<im::Segment>,
    /// [`Forgets::fingerprint`] of the session when loaded.
    pub forgotten: String,
}

impl Loaded {
    /// The session `uuid` unless it is unknown or forgotten (404).
    pub fn load(ctx: &WebContext, cache: &MarkerCache, uuid: &str) -> Result<Self> {
        let db = IndexDb::open(ctx)?;
        let forgets = Forgets::load(ctx)?;
        let _snap = db.snapshot()?;
        let row = crate::api::sessions::visible_session(&db, &forgets, uuid)?;
        let files = CastFiles::load(&db, &row)?;
        let commands = db.commands(row.id)?;
        let segments = db.segments(row.id)?;
        drop(_snap);
        let mask = mask_for(
            ctx,
            cache,
            &forgets,
            &row.session.uuid,
            row.session.legacy,
            &files,
            &commands,
        )?;
        let forgotten = forgets.fingerprint(uuid);
        let mut labels: Vec<(u64, String)> = Vec::new();
        let mut label_seqs = HashSet::new();
        for c in &commands {
            let t = secs_to_us(c.t_start);
            if forgets.command(uuid, c) || mask.seqs.contains(&c.seq) || mask.span_at(t).is_some() {
                continue;
            }
            labels.push((t, label(&c.typed)));
            label_seqs.insert(c.seq);
        }
        labels.sort_by_key(|(t, _)| *t);
        Ok(Self {
            row,
            files,
            mask,
            labels,
            label_seqs,
            segments,
            forgotten,
        })
    }

    pub fn uuid(&self) -> &str {
        &self.row.session.uuid
    }

    pub fn legacy(&self) -> bool {
        self.row.session.legacy
    }
}

/// Playback mode of a cast of `bytes` uncompressed bytes.
pub(crate) fn mode_for(ctx: &WebContext, bytes: u64) -> PlayMode {
    if bytes < threshold(ctx) {
        PlayMode::Cast
    } else {
        PlayMode::Segmented
    }
}

/// `index.playback_segment_threshold_mb` in bytes.
pub(crate) fn threshold(ctx: &WebContext) -> u64 {
    ctx.config.index.playback_segment_threshold_mb.saturating_mul(1 << 20)
}

/// Marker-derived spans of forgotten commands (see [`mask_for`]) per session, with the scan
/// position, so that a cast is scanned once: later requests reuse the result, and a live
/// cast that grew is scanned on from where the last scan stopped.
#[derive(Debug, Default)]
pub(crate) struct MarkerCache {
    inner: Mutex<HashMap<String, MarkerScan>>,
}

/// Sessions whose scans are kept.
const MARKER_SCANS: usize = 16;

#[derive(Debug, Clone)]
struct MarkerScan {
    /// The seqs looked for, ascending.
    seqs: Vec<u64>,
    /// The cast's first part, so a scan is never resumed in another cast's files.
    first: PathBuf,
    /// Where the scan stopped.
    at: StartPoint,
    /// Commands seen without their `done` yet, by (shell, counter).
    open: HashMap<(u32, u64), u64>,
    last_done: u64,
    /// `(from, to)` in µs, `to = u64::MAX` without a `done`.
    spans: HashMap<u64, (u64, u64)>,
}

impl MarkerScan {
    fn new(seqs: Vec<u64>, files: &CastFiles) -> Self {
        Self {
            seqs,
            first: first_part(files),
            at: StartPoint::ORIGIN,
            open: HashMap::new(),
            last_done: 0,
            spans: HashMap::new(),
        }
    }

    fn complete(&self) -> bool {
        self.seqs
            .iter()
            .all(|s| self.spans.get(s).is_some_and(|sp| sp.1 != u64::MAX))
    }

    /// Whether the files still hold the data scanned so far.
    fn resumable(&self, files: &CastFiles) -> bool {
        self.first == first_part(files)
            && files
                .parts
                .get(self.at.part_idx)
                .is_some_and(|p| p.bytes >= self.at.offset)
    }

    /// Reads the markers from where the scan stopped to the end of the files: a forgotten
    /// command spans from the `done` before its `cmd` (any shell's) to its own `done`
    /// (matched by shell and counter, else by seq).
    fn scan(&mut self, files: &CastFiles) -> Result<()> {
        let mut events = Events::open(files, self.at)?;
        let mut last_raw = self.at.t_us;
        while let Some(next) = events.next_event() {
            let (raw, ev) = next?;
            last_raw = raw;
            if ev.code != EventCode::Marker {
                continue;
            }
            match CastMarker::from_json(&ev.data) {
                Ok(Some(CastMarker::Cmd(rec))) if self.seqs.binary_search(&rec.seq).is_ok() => {
                    self.spans.insert(rec.seq, (self.last_done.min(ev.t_us), u64::MAX));
                    self.open.insert((rec.cmd.sh, rec.cmd.n), rec.seq);
                }
                Ok(Some(CastMarker::Done(rec))) => {
                    self.last_done = ev.t_us;
                    let seq = self.open.remove(&(rec.done.sh, rec.done.n)).or(rec.seq);
                    if let Some(seq) = seq
                        && let Some(span) = self.spans.get_mut(&seq)
                    {
                        span.1 = ev.t_us;
                    }
                }
                _ => {}
            }
        }
        self.at = events.start_point(events.pos(), last_raw);
        Ok(())
    }
}

fn first_part(files: &CastFiles) -> PathBuf {
    files.parts.first().map(|p| p.path.clone()).unwrap_or_default()
}

impl MarkerCache {
    /// The marker spans of `seqs` in the session's cast.
    fn spans(&self, uuid: &str, files: &CastFiles, seqs: &HashSet<u64>) -> Result<HashMap<u64, (u64, u64)>> {
        let mut sorted: Vec<u64> = seqs.iter().copied().collect();
        sorted.sort_unstable();
        let lock = || self.inner.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut scan = match lock().get(uuid) {
            Some(prev) if prev.seqs == sorted && prev.first == first_part(files) && prev.complete() => {
                return Ok(prev.spans.clone());
            }
            Some(prev) if prev.seqs == sorted && prev.resumable(files) => prev.clone(),
            _ => MarkerScan::new(sorted, files),
        };
        scan.scan(files)?;
        let spans = scan.spans.clone();
        let mut inner = lock();
        if inner.len() >= MARKER_SCANS && !inner.contains_key(uuid) {
            inner.clear();
        }
        inner.insert(uuid.to_owned(), scan);
        Ok(spans)
    }

    /// Scan position of a session (tests).
    #[cfg(test)]
    pub fn position(&self, uuid: &str) -> Option<(usize, u64)> {
        let inner = self.inner.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        inner.get(uuid).map(|s| s.at.pos())
    }
}

/// The mask of session `row` as playback, export and the session detail apply it.
pub(crate) fn mask_of(
    ctx: &WebContext,
    cache: &MarkerCache,
    db: &IndexDb,
    forgets: &Forgets,
    row: &SessionRow,
) -> Result<Mask> {
    if forgets.rows(&row.session.uuid).is_empty() {
        return Ok(Mask::default());
    }
    let files = CastFiles::load(db, row)?;
    let commands = db.commands(row.id)?;
    mask_for(
        ctx,
        cache,
        forgets,
        &row.session.uuid,
        row.session.legacy,
        &files,
        &commands,
    )
}

/// The mask of a session: its forgotten ranges and the spans of its forgotten commands
/// ([`command_span`]). A command's span is computed from index.db while the command is
/// there and recorded in meta.db ([`CmdSpan`]), because the serve daemon deletes the row
/// soon after the forget; afterwards the recorded span is used. A cinelog cast without one
/// (a command forgotten with the CLI and purged before this server saw it), or with an open
/// one, takes it from the cast's `cmd`/`done` markers, scanned once and recorded too.
/// `commands` may be empty: a live session without index rows is masked from meta.db and
/// its markers alone.
pub(crate) fn mask_for(
    ctx: &WebContext,
    cache: &MarkerCache,
    forgets: &Forgets,
    uuid: &str,
    legacy: bool,
    files: &CastFiles,
    commands: &[im::Command],
) -> Result<Mask> {
    let mut spans: Vec<Span> = forgets
        .ranges(uuid)
        .map(|(a, b, by)| Span {
            from_us: floor_us(a),
            to_us: ceil_us(b),
            by: by.to_owned(),
        })
        .collect();
    let by_seq: HashMap<u64, &im::Command> = commands.iter().map(|c| (c.seq, c)).collect();
    let mut seqs = HashSet::new();
    // Commands whose span (or its end) must come from the markers.
    let mut from_markers: HashMap<u64, (Option<f64>, &str)> = HashMap::new();
    for f in forgets.rows(uuid) {
        let im::ForgetTarget::Command { seq, .. } = f.target else {
            continue;
        };
        seqs.insert(seq);
        let indexed = by_seq.get(&seq);
        let span = match indexed {
            Some(c) => {
                let (from, to) = command_span(commands, c);
                let span = CmdSpan { from, to };
                if forgets.span(uuid, seq) != Some(span) {
                    remember(ctx, uuid, seq, span);
                }
                Some(span)
            }
            None => forgets.span(uuid, seq),
        };
        match span {
            Some(CmdSpan { from, to: Some(to) }) => spans.push(Span {
                from_us: floor_us(from),
                to_us: ceil_us(to),
                by: f.by.clone(),
            }),
            // Still running as far as the index knows (or a legacy command without an end).
            Some(CmdSpan { from, to: None }) if legacy || indexed.is_some() => spans.push(Span {
                from_us: floor_us(from),
                to_us: u64::MAX,
                by: f.by.clone(),
            }),
            Some(CmdSpan { from, to: None }) => {
                from_markers.insert(seq, (Some(from), &f.by));
            }
            None if legacy => {
                tracing::warn!("forgotten command {seq} of legacy session {uuid} has no recorded span");
            }
            None => {
                from_markers.insert(seq, (None, &f.by));
            }
        }
    }
    if !from_markers.is_empty() {
        let found = cache.spans(uuid, files, &from_markers.keys().copied().collect())?;
        for (seq, (from, by)) in from_markers {
            let marked = found.get(&seq).copied();
            let from_us = from.map(floor_us).or(marked.map(|m| m.0));
            let Some(from_us) = from_us else {
                tracing::warn!("forgotten command {seq} of session {uuid} is in neither the index nor the cast");
                continue;
            };
            let to_us = marked.map_or(u64::MAX, |m| m.1);
            if to_us != u64::MAX {
                let span = CmdSpan {
                    from: from.unwrap_or(us_to_secs(from_us)),
                    to: Some(us_to_secs(to_us)),
                };
                if forgets.span(uuid, seq) != Some(span) {
                    remember(ctx, uuid, seq, span);
                }
            }
            spans.push(Span {
                from_us,
                to_us,
                by: by.to_owned(),
            });
        }
    }
    Ok(Mask::new(spans, seqs))
}

/// Records a span, best effort: a server that cannot write meta.db finds it again.
fn remember(ctx: &WebContext, uuid: &str, seq: u64, span: CmdSpan) {
    let recorded =
        cinelog_index::db::open_meta(&ctx.db).and_then(|mut meta| record_span(meta.as_mut(), uuid, seq, span));
    if let Err(e) = recorded {
        tracing::debug!("recording the span of forgotten command {seq} of {uuid}: {e}");
    }
}

/// What forgetting command `c` hides in playback: from the end of the command before it
/// (so the prompt and the echoed command line go too; the session start for the first
/// one) to its own end (`None` while it runs).
pub(crate) fn command_span(commands: &[im::Command], c: &im::Command) -> (f64, Option<f64>) {
    let from = commands
        .iter()
        .filter(|o| o.seq != c.seq)
        .filter_map(|o| o.t_end)
        .filter(|&end| end <= c.t_start)
        .fold(0.0, f64::max);
    (from, c.t_end)
}

fn floor_us(t: f64) -> u64 {
    if t.is_finite() && t > 0.0 {
        (t * 1e6).floor() as u64
    } else {
        0
    }
}

fn ceil_us(t: f64) -> u64 {
    if t.is_finite() && t > 0.0 {
        (t * 1e6).ceil() as u64
    } else {
        0
    }
}
