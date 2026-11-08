//! `GET /api/sessions/{uuid}/live` (docs/API.md §9.1): the asciinema-player `eventsource`
//! driver's stream.
//!
//! A join builds the terminal at the current end of the cast: from the newest playback
//! checkpoint (an index.db segment whose dump cannot show anything forgotten; for a masked
//! large cast the playback plan's masked segments; else the cast start) through avt to the
//! end, sent as `LiveInit`. A thread then follows the cast: it polls for appended events
//! every [`POLL`], rewritten and masked like `/cast`, follows rotation parts, re-reads the
//! forgotten list every [`MASK_EVERY`] and on every bus `forgotten` (a change re-joins, so
//! the viewer's screen loses what was forgotten), and checks the recorder every
//! [`LIVE_EVERY`]: an `x` ends the stream with `done`, a recorder that died without one
//! with `LiveOffline` and `done`.

use std::path::Path;
use std::time::{Duration, Instant};

use bytes::Bytes;
use cinelog_cast::{EventCode, PartName};
use cinelog_index::model as im;
use cinelog_proto::CastMarker;
use futures_util::Stream;
use tokio::sync::broadcast::{self, error::TryRecvError};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

use super::recorders;
use super::sse;
use crate::api::db::{Forgets, IndexDb};
use crate::api::types::{
    BusEvent, ForgetTarget, LiveEvent, LiveInit, LiveMessage, LiveOffline, LiveStreamState, PlayMode, SessionState,
};
use crate::error::{Result, WebError};
use crate::play::plan::{apply, terminal};
use crate::play::session::{Loaded, mask_for, mode_for};
use crate::play::source::{
    CastFiles, Events, PartFile, Shift, StartPoint, clamp_size, secs_to_us, sizes_at, us_to_secs,
};
use crate::play::stream::{Mask, OutEvent, Rewriter};
use crate::state::AppState;

/// How often appended events are looked for.
pub(crate) const POLL: Duration = Duration::from_millis(100);
/// How often the recorder's registry entry is checked.
pub(crate) const LIVE_EVERY: Duration = Duration::from_millis(500);
/// How often the forgotten list is re-read without a bus `forgotten`.
pub(crate) const MASK_EVERY: Duration = Duration::from_secs(2);
/// Frames buffered towards a slow client before the follower waits.
const CHANNEL: usize = 256;
/// Events read per batch before they are sent.
const BATCH: usize = 512;

/// Everything about a session the stream needs.
struct Source {
    uuid: String,
    files: CastFiles,
    mask: Mask,
    /// `Forgets::fingerprint` when loaded.
    forgotten: String,
    legacy: bool,
    /// Indexed playback segments (`seq` order).
    segments: Vec<im::Segment>,
    /// Described by the index (else by the registry and the cast).
    indexed: bool,
}

/// The parts of `files` plus the ones on disk after them (a live cast rotated since it was
/// indexed, or a session the indexer has not seen).
fn with_disk_parts(mut files: CastFiles) -> CastFiles {
    let Some(first) = files.parts.first().map(|p| p.path.clone()) else {
        return files;
    };
    let last = files.parts.last().map_or(0, |p| p.part);
    for path in cinelog_cast::find_parts(&first) {
        let Some(part) = part_number(&path) else { continue };
        if part <= last || files.parts.last().is_some_and(|p| p.part + 1 != part) {
            continue;
        }
        let Ok(meta) = std::fs::metadata(&path) else { break };
        files.parts.push(PartFile {
            part,
            path,
            bytes: meta.len(),
            // Only the v3 resume time depends on it, and that already includes it.
            t_offset_us: 0,
        });
    }
    files
}

fn part_number(path: &Path) -> Option<u32> {
    PartName::parse(path.file_name()?.to_str()?).map(|n| n.part)
}

impl Source {
    /// The session `uuid` from the index, or from the registry while the indexer has not
    /// seen it yet. Unknown and forgotten sessions are 404.
    fn load(st: &AppState, uuid: &str) -> Result<Self> {
        let forgets = Forgets::load(&st.ctx)?;
        if forgets.session(uuid) {
            return Err(WebError::NotFound);
        }
        let indexed = match Loaded::load(&st.ctx, &st.play.markers, uuid) {
            Ok(l) => Ok(l),
            Err(e @ (WebError::NotFound | WebError::Unavailable(_))) => Err(e),
            Err(WebError::Index(cinelog_index::IndexError::NotFound(_))) => Err(WebError::NotFound),
            Err(e) => return Err(e),
        };
        match indexed {
            Ok(l) => Ok(Self {
                uuid: uuid.to_owned(),
                files: with_disk_parts(l.files),
                legacy: l.row.session.legacy,
                mask: l.mask,
                forgotten: l.forgotten,
                segments: l.segments,
                indexed: true,
            }),
            Err(e) => {
                let rec = recorders::find(st.live.dirs(), st.ctx.scope, uuid)
                    .filter(|r| r.alive)
                    .ok_or(e)?;
                let first = cinelog_cast::find_parts(&rec.cast)
                    .into_iter()
                    .next()
                    .ok_or(WebError::NotFound)?;
                let bytes = std::fs::metadata(&first).map_err(|_| WebError::NotFound)?.len();
                let files = with_disk_parts(CastFiles {
                    parts: vec![PartFile {
                        part: part_number(&first).unwrap_or(1),
                        path: first,
                        bytes,
                        t_offset_us: 0,
                    }],
                });
                // No index rows: the mask comes from meta.db and the cast's markers alone.
                let mask = mask_for(&st.ctx, &st.play.markers, &forgets, uuid, false, &files, &[])?;
                Ok(Self {
                    uuid: uuid.to_owned(),
                    files,
                    mask,
                    forgotten: forgets.fingerprint(uuid),
                    legacy: false,
                    segments: Vec::new(),
                    indexed: false,
                })
            }
        }
    }
}

/// Whether a forgotten command of `mask` has no end yet (it hides everything after its
/// start until a reload learns the end).
fn has_open_span(mask: &Mask) -> bool {
    mask.spans.iter().any(|s| s.to_us == u64::MAX)
}

/// Where a join starts reading.
struct Start {
    at: StartPoint,
    start_us: u64,
    size: (u16, u16),
    init: String,
    shown: Vec<bool>,
}

/// An indexed segment as a reader start (as `play::plan` resumes them).
fn db_start(files: &CastFiles, legacy: bool, seg: &im::Segment) -> Option<(u64, StartPoint)> {
    let start_us = secs_to_us(seg.t_start);
    if seg.seq == 0 || start_us == 0 {
        return None;
    }
    let at = StartPoint {
        part_idx: files.index_of(seg.part)?,
        offset: seg.byte_offset,
        t_us: start_us,
        shift: if legacy {
            Shift::FirstAt(start_us)
        } else {
            Shift::Fixed(0)
        },
    };
    Some((start_us, at))
}

/// The terminal size before reader position `pos`, continuing the cached scan of an
/// earlier join of the same session when there is one.
fn size_at(st: &AppState, src: &Source, pos: (usize, u64)) -> Result<(u16, u16)> {
    let from = match st.live.cached_size(&src.uuid, pos) {
        Some((at, size)) => (at.0, at.1, size),
        None => {
            let h = src.files.header()?;
            (0, 0, clamp_size((h.term.cols, h.term.rows)))
        }
    };
    let size = sizes_at(&src.files, from, &[pos])?
        .pop()
        .unwrap_or((from.2.0, from.2.1));
    st.live.cache_size(&src.uuid, pos, size);
    Ok(size)
}

/// The newest checkpoint of `src` whose terminal state shows nothing forgotten.
fn checkpoint(st: &AppState, src: &Source) -> Result<Start> {
    let spans = src.mask.spans.len();
    let first_masked = src.mask.first_start();
    let newest = src
        .segments
        .iter()
        .rev()
        .filter_map(|seg| db_start(&src.files, src.legacy, seg).map(|s| (s, seg)))
        .find(|((start_us, _), _)| first_masked.is_none_or(|m| *start_us < m));
    if let Some(((start_us, at), seg)) = newest {
        return Ok(Start {
            at,
            start_us,
            size: size_at(st, src, at.pos())?,
            init: seg.vt_dump.clone(),
            shown: vec![false; spans],
        });
    }
    if spans > 0 && !src.segments.is_empty() && mode_for(&st.ctx, src.files.total_bytes()) == PlayMode::Segmented {
        // The index's dumps would show forgotten output: the plan's are masked.
        let plan = st.play.plan(&st.ctx, &src.uuid)?;
        if plan.mask == src.mask
            && let Some(seg) = plan.segments.last()
        {
            return Ok(Start {
                at: seg.at,
                start_us: seg.start_us,
                size: seg.size,
                init: seg.init.clone(),
                shown: seg.shown.clone(),
            });
        }
    }
    let h = src.files.header()?;
    Ok(Start {
        at: StartPoint::ORIGIN,
        start_us: 0,
        size: clamp_size((h.term.cols, h.term.rows)),
        init: String::new(),
        shown: vec![false; spans],
    })
}

fn is_rotate(ev: &cinelog_cast::Event) -> bool {
    ev.code == EventCode::Marker && matches!(CastMarker::from_json(&ev.data), Ok(Some(CastMarker::Rotate(_))))
}

/// The reader at the end of the data read so far.
struct Tail {
    files: CastFiles,
    events: Events,
    /// Raw reader time of the last event (resume time when reopening).
    last_raw: u64,
    /// Session time of the last event.
    last_us: u64,
    shown: Vec<bool>,
    part_idx: usize,
    /// The last event of the current part was its `rotate` marker.
    rotated: bool,
    /// The exit event was read.
    exited: bool,
}

impl Tail {
    /// Reads appended events into `out` (at most [`BATCH`]); `true` at the end of the data.
    fn read(&mut self, src: &Source, out: &mut Vec<OutEvent>) -> Result<bool> {
        let mut rw = Rewriter::new(&src.mask, std::mem::take(&mut self.shown), !src.legacy);
        let mut at_end = true;
        while out.len() < BATCH {
            let Some(next) = self.events.next_event() else { break };
            let (raw, ev) = next?;
            self.last_raw = raw;
            self.last_us = self.last_us.max(ev.t_us);
            let part = self.events.pos().0;
            if part != self.part_idx {
                self.part_idx = part;
                self.rotated = false;
            }
            if is_rotate(&ev) {
                self.rotated = true;
            }
            if let Some(e) = rw.push(ev) {
                if e.code == 'x' {
                    self.exited = true;
                    break;
                }
                out.push(e);
            }
            if out.len() >= BATCH {
                at_end = false;
            }
        }
        self.shown = rw.shown().to_vec();
        Ok(at_end)
    }

    /// Continues into parts that appeared after the current one; `true` if there are any.
    fn reopen(&mut self) -> Result<bool> {
        let files = with_disk_parts(self.files.clone());
        if files.parts.len() <= self.files.parts.len() {
            return Ok(false);
        }
        let at = self.events.start_point(self.events.pos(), self.last_raw);
        self.events = Events::open(&files, at)?;
        self.files = files;
        Ok(true)
    }

    fn current_part(&self) -> Option<&Path> {
        self.files.parts.get(self.part_idx).map(|p| p.path.as_path())
    }
}

/// The terminal at the current end of `src`, and the reader positioned there.
fn join(st: &AppState, src: &Source) -> Result<(LiveInit, Tail)> {
    let start = checkpoint(st, src)?;
    let mut size = start.size;
    let mut vt = terminal(size, &start.init);
    let mut tail = Tail {
        files: src.files.clone(),
        events: Events::open(&src.files, start.at)?,
        last_raw: start.at.t_us,
        last_us: start.start_us,
        shown: start.shown,
        part_idx: start.at.part_idx,
        rotated: false,
        exited: false,
    };
    let mut batch = Vec::with_capacity(BATCH);
    loop {
        let at_end = tail.read(src, &mut batch)?;
        for e in batch.drain(..) {
            apply(&mut vt, &mut size, &e);
        }
        if tail.exited || (at_end && !(tail.rotated && tail.reopen()?)) {
            break;
        }
    }
    let init = LiveInit {
        cols: size.0,
        rows: size.1,
        time: us_to_secs(tail.last_us),
        init: vt.dump(),
    };
    Ok((init, tail))
}

/// A joined stream, ready to follow.
pub(crate) struct Follower {
    st: AppState,
    src: Source,
    init: LiveInit,
    tail: Tail,
    /// The bus, subscribed before the join: no `forgotten` after it is missed.
    rx: broadcast::Receiver<BusEvent>,
}

/// How the stream ends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum End {
    /// `done`.
    Done,
    /// `LiveOffline`, then `done`.
    Offline,
    /// The client went away or the server shuts down.
    Gone,
}

impl Follower {
    /// Joins session `uuid` (404 for unknown or forgotten sessions).
    pub fn open(st: &AppState, uuid: &str) -> Result<Self> {
        let rx = st.bus.subscribe();
        let src = Source::load(st, uuid)?;
        let (init, tail) = join(st, &src)?;
        Ok(Self {
            st: st.clone(),
            src,
            init,
            tail,
            rx,
        })
    }

    fn send(tx: &mpsc::Sender<Bytes>, frame: Bytes) -> bool {
        tx.blocking_send(frame).is_ok()
    }

    /// Whether something about this session was forgotten since the last check (or bus
    /// events were missed).
    fn forgets_changed(&mut self) -> bool {
        let mut changed = false;
        loop {
            match self.rx.try_recv() {
                Ok(BusEvent::Forgotten(f)) => {
                    let (ForgetTarget::Session { uuid }
                    | ForgetTarget::Command { uuid, .. }
                    | ForgetTarget::Range { uuid, .. }) = &f.target;
                    changed |= *uuid == self.src.uuid;
                }
                Ok(_) => {}
                Err(TryRecvError::Lagged(_)) => changed = true,
                Err(TryRecvError::Empty | TryRecvError::Closed) => return changed,
            }
        }
    }

    /// Re-reads the forgotten list; a change of the mask re-joins (a fresh `LiveInit`).
    /// `Some(end)` when the session itself is forgotten now. The session is also reloaded
    /// while it is not indexed (so it is masked like `/cast` once it is) and while a
    /// forgotten command has no end (to show what follows it once it ends). A list that
    /// cannot be read is retried at the next check; a re-join after a change of the list
    /// that fails ends the stream, so nothing forgotten is shown.
    fn recheck_mask(&mut self, tx: &mpsc::Sender<Bytes>) -> Result<Option<End>> {
        let forgets = match Forgets::load(&self.st.ctx) {
            Ok(f) => f,
            Err(e) => {
                tracing::debug!("live stream of {}: forgotten list: {e}", self.src.uuid);
                return Ok(None);
            }
        };
        if forgets.session(&self.src.uuid) {
            return Ok(Some(End::Done));
        }
        let listed = forgets.fingerprint(&self.src.uuid) != self.src.forgotten;
        if !listed && self.src.indexed && !has_open_span(&self.src.mask) {
            return Ok(None);
        }
        let src = match Source::load(&self.st, &self.src.uuid) {
            Ok(src) => src,
            Err(e) if listed => return Err(e),
            Err(e) => {
                // The old mask still covers everything forgotten in it.
                tracing::debug!("live stream of {}: reload: {e}", self.src.uuid);
                return Ok(None);
            }
        };
        if src.mask == self.src.mask {
            // Same masking: the reader and the viewer's screen stay as they are.
            self.src = src;
            return Ok(None);
        }
        let (init, tail) = join(&self.st, &src)?;
        self.src = src;
        self.tail = tail;
        if !Self::send(tx, sse::live_message(&LiveMessage::Init(init))) {
            return Ok(Some(End::Gone));
        }
        Ok(None)
    }

    /// Sends what was appended; `Ok(true)` at the end of the data.
    fn pump(&mut self, tx: &mpsc::Sender<Bytes>, batch: &mut Vec<OutEvent>) -> Result<Option<bool>> {
        let at_end = self.tail.read(&self.src, batch)?;
        for e in batch.drain(..) {
            let msg = LiveMessage::Event(LiveEvent(us_to_secs(e.t_us), e.code.to_string(), e.data));
            if !Self::send(tx, sse::live_message(&msg)) {
                return Ok(None);
            }
        }
        if at_end && self.tail.rotated && self.tail.reopen()? {
            return Ok(Some(false));
        }
        Ok(Some(at_end))
    }

    /// How a session whose recorder is gone ended: offline unless the cast or the index
    /// says it ended normally (or degraded, which is no crash).
    fn gone_end(&self, last_degraded: bool) -> End {
        let state = IndexDb::open(&self.st.ctx)
            .ok()
            .and_then(|db| db.session(&self.src.uuid).ok().flatten())
            .map(|r| r.session.state);
        match state {
            Some(SessionState::Ended | SessionState::Degraded | SessionState::Error) => End::Done,
            _ if last_degraded || self.src.legacy => End::Done,
            _ => End::Offline,
        }
    }

    /// Follows the cast until it ends, the client goes away or the server shuts down.
    fn run(mut self, tx: &mpsc::Sender<Bytes>) -> Result<End> {
        if !Self::send(tx, sse::live_message(&LiveMessage::Init(self.init.clone()))) {
            return Ok(End::Gone);
        }
        let mut batch = Vec::with_capacity(BATCH);
        let mut last_mask = Instant::now();
        let mut last_live: Option<Instant> = None;
        let mut degraded = false;
        loop {
            if tx.is_closed() || self.st.bus.is_closed() {
                return Ok(End::Gone);
            }
            if self.tail.exited {
                return Ok(End::Done);
            }
            if self.forgets_changed() || last_mask.elapsed() >= MASK_EVERY {
                last_mask = Instant::now();
                if let Some(end) = self.recheck_mask(tx)? {
                    return Ok(end);
                }
            }
            let Some(at_end) = self.pump(tx, &mut batch)? else {
                return Ok(End::Gone);
            };
            if self.tail.exited {
                return Ok(End::Done);
            }
            if !at_end {
                continue;
            }
            if last_live.is_none_or(|t| t.elapsed() >= LIVE_EVERY) {
                last_live = Some(Instant::now());
                match recorders::find(self.st.live.dirs(), self.st.ctx.scope, &self.src.uuid) {
                    Some(rec) if rec.alive => {
                        degraded = rec.state == cinelog_core::RecState::Degraded;
                        // A new part without a `rotate` marker (resumed after degradation).
                        if self.tail.current_part() != Some(rec.cast.as_path()) && self.tail.reopen()? {
                            continue;
                        }
                    }
                    rec => {
                        degraded |= rec.is_some_and(|r| r.state == cinelog_core::RecState::Degraded);
                        // The recorder writes `x` before it removes its entry: read once more.
                        self.tail.reopen()?;
                        loop {
                            match self.pump(tx, &mut batch)? {
                                None => return Ok(End::Gone),
                                Some(true) => break,
                                Some(false) => {}
                            }
                        }
                        if self.tail.exited {
                            return Ok(End::Done);
                        }
                        return Ok(self.gone_end(degraded));
                    }
                }
            }
            std::thread::sleep(POLL);
        }
    }
}

/// Starts following `f` on its own thread; the frames of the stream, `LiveInit` first.
pub(crate) fn spawn(f: Follower) -> Result<impl Stream<Item = Bytes> + Send + 'static> {
    let (tx, rx) = mpsc::channel(CHANNEL);
    let uuid = f.src.uuid.clone();
    std::thread::Builder::new().name("cinelog-live".into()).spawn(move || {
        let end = f.run(&tx).unwrap_or_else(|e| {
            tracing::warn!("live stream of {uuid} ended early: {e}");
            End::Done
        });
        if end == End::Offline {
            let offline = LiveMessage::Offline(LiveOffline {
                state: LiveStreamState::Offline,
            });
            let _ = tx.blocking_send(sse::live_message(&offline));
        }
        if end != End::Gone {
            let _ = tx.blocking_send(sse::done());
        }
    })?;
    Ok(ReceiverStream::new(rx))
}

/// Session time (µs) a join of `uuid` starts reading at.
#[cfg(test)]
pub(crate) fn checkpoint_us(st: &AppState, uuid: &str) -> Result<u64> {
    Ok(checkpoint(st, &Source::load(st, uuid)?)?.start_us)
}

/// Whether a join of `uuid` is described by the index, and its mask.
#[cfg(test)]
pub(crate) fn source_mask(st: &AppState, uuid: &str) -> Result<(bool, Mask)> {
    let src = Source::load(st, uuid)?;
    Ok((src.indexed, src.mask))
}

#[cfg(test)]
pub(crate) fn init_of(f: &Follower) -> LiveInit {
    f.init.clone()
}

/// The `LiveInit` of a join that reads `uuid` from its start.
#[cfg(test)]
pub(crate) fn replay_from_origin(st: &AppState, uuid: &str) -> Result<LiveInit> {
    let mut src = Source::load(st, uuid)?;
    src.segments.clear();
    Ok(join(st, &src)?.0)
}
