//! [`SessionPipeline`] for cinelog casts: parts reader → clock map → markers and the
//! segmenter → [`AvtTranscript`] → batches with checkpoints (plan §6.4).
//!
//! Everything that influences later output lives in the transcript state or in
//! [`PState`], so resuming from any checkpoint yields the rows a single pass yields (I12).

use std::collections::HashMap;
use std::fs::File;
use std::path::{Path, PathBuf};

use cinelog_cast::{
    CastReader, ClockAnchor, ClockMap, Event, EventCode, Header, PartName, PartsReader, ResumePoint, existing_part,
    find_parts, parse_sane_resize, scan_frames,
};
use cinelog_proto::{CastMarker, CmdRecord, DoneRecord, SessionInfo};
use serde::{Deserialize, Serialize};

use super::exact::{F64, XCommand, XSegment};
use super::segmenter::Segmenter;
use super::transcript::AvtTranscript;
use crate::api::{PipelineFactory, SessionPipeline, Transcript};
use crate::error::{IndexError, Result};
use crate::model::{
    CastPart, Checkpoint, Chunk, Command, FileStamp, IndexBatch, MarkerRow, OpenCommand, Owner, PollBudget,
    RenderOptions, Segment, SessionCounters, SessionMeta, SessionState, Stamp, TmuxLocation, ZstFrameRow, segment_due,
};

/// Bytes charged per event besides its data (time, code, JSON framing).
const EVENT_OVERHEAD: u64 = 16;

/// The factory behind `render::cast_pipelines`.
pub(crate) struct CastPipelines {
    pub(crate) opts: RenderOptions,
}

impl PipelineFactory for CastPipelines {
    fn open(&self, cast: &Path, checkpoint: Option<Checkpoint>) -> Result<Box<dyn SessionPipeline>> {
        Ok(Box::new(CastPipeline::open(cast, checkpoint, self.opts)?))
    }
}

/// Pipeline-private checkpoint state (`Checkpoint::pipeline_state`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct PState {
    uuid: String,
    /// Absolute time of the last event read (µs).
    t_us: u64,
    clock: Vec<(u64, i64, bool)>,
    /// Time offset of each part by index into the part list (µs).
    offsets: Vec<Option<u64>>,
    part_idx: usize,
    open: Vec<XCommand>,
    segment: XSegment,
    segment_bytes: u64,
    tmux: TmuxLocation,
    state: SessionState,
    exit_status: Option<i32>,
    ended_at: Option<F64>,
    bad_lines: u64,
    bad_markers: u64,
    lossy_bytes: u64,
    x_seen: bool,
    provisional_out: bool,
}

/// What the next batch carries besides the session row and the checkpoint.
#[derive(Debug, Default)]
struct Out {
    parts: bool,
    commands: Vec<Command>,
    chunks: Vec<Chunk>,
    provisional: Option<Vec<Chunk>>,
    markers: Vec<MarkerRow>,
    segments: Vec<Segment>,
    /// Anything besides the session row changed.
    changed: bool,
}

pub(crate) struct CastPipeline {
    first: PathBuf,
    /// The session row as of the `session` marker of part 1.
    base: SessionMeta,
    reader: PartsReader,
    transcript: AvtTranscript,
    clock: ClockMap,
    seg: Segmenter,
    ps: PState,
    out: Out,
    reset: bool,
    announced: bool,
    finished: bool,
    /// Frame tables of the compressed parts (derived from the files, never checkpointed).
    frames: HashMap<PathBuf, Option<Vec<ZstFrameRow>>>,
}

/// Header and `session` marker of a session's first part.
fn read_session(first: &Path) -> Result<(Header, SessionInfo)> {
    let mut r = cinelog_cast::open(first)?;
    let header = r.header().clone();
    let ev = r.next_event().transpose()?;
    match ev {
        Some(ev) if ev.t_us == 0 && ev.code == EventCode::Marker => match CastMarker::from_json(&ev.data) {
            Ok(Some(CastMarker::Session(info))) => Ok((header, *info)),
            _ => Err(not_cinelog(first)),
        },
        _ => Err(not_cinelog(first)),
    }
}

fn not_cinelog(path: &Path) -> IndexError {
    IndexError::Invalid(format!("{}: not a cinelog cast (no session marker)", path.display()))
}

/// 1-based part number of `path` (index `idx` in the part list).
fn part_no(path: &Path, idx: usize) -> u32 {
    path.file_name()
        .and_then(|f| f.to_str())
        .and_then(PartName::parse)
        .map_or(idx as u32 + 1, |n| n.part)
}

fn is_compressed(path: &Path) -> bool {
    path.file_name()
        .and_then(|f| f.to_str())
        .and_then(PartName::parse)
        .is_some_and(|n| n.compressed)
}

fn secs(t_us: u64) -> f64 {
    t_us as f64 / 1e6
}

impl CastPipeline {
    fn open(cast: &Path, checkpoint: Option<Checkpoint>, opts: RenderOptions) -> Result<Self> {
        let mut paths = find_parts(cast);
        if paths.is_empty() {
            paths.push(cast.to_owned());
        }
        let first = paths[0].clone();
        let (header, info) = read_session(&first)?;
        let base = SessionMeta::from_session_marker(&info, &header, &first);
        let had_checkpoint = checkpoint.is_some();
        if let Some(cp) = checkpoint
            && let Some(p) = Self::resume(&paths, &base, &cp, opts)
        {
            return Ok(p);
        }
        let reader = PartsReader::from_paths(paths)?;
        let transcript = AvtTranscript::new(header.term.cols, header.term.rows, opts)?;
        let segment = Segment {
            seq: 0,
            part: part_no(&first, 0),
            t_start: 0.0,
            t_end: None,
            byte_offset: reader.position(),
            vt_dump: transcript.vt_dump(),
        };
        let ps = PState {
            uuid: base.uuid.clone(),
            t_us: 0,
            clock: Vec::new(),
            offsets: vec![Some(0)],
            part_idx: 0,
            open: Vec::new(),
            segment: segment.clone().into(),
            segment_bytes: 0,
            tmux: base.tmux.clone(),
            state: SessionState::Live,
            exit_status: None,
            ended_at: None,
            bad_lines: 0,
            bad_markers: 0,
            lossy_bytes: 0,
            x_seen: false,
            provisional_out: false,
        };
        let out = Out {
            parts: true,
            segments: vec![segment],
            changed: true,
            ..Out::default()
        };
        Ok(Self {
            first,
            base,
            reader,
            transcript,
            clock: ClockMap::new(),
            seg: Segmenter::default(),
            ps,
            out,
            reset: had_checkpoint,
            announced: false,
            finished: false,
            frames: HashMap::new(),
        })
    }

    /// Continues from `cp` when it still describes these files; `None` starts over.
    fn resume(paths: &[PathBuf], base: &SessionMeta, cp: &Checkpoint, opts: RenderOptions) -> Option<Self> {
        let ps: PState = serde_json::from_str(&cp.pipeline_state).ok()?;
        if ps.uuid != base.uuid {
            return None;
        }
        let idx = paths.iter().enumerate().position(|(i, p)| part_no(p, i) == cp.part)?;
        if idx != ps.part_idx {
            return None;
        }
        let path = &paths[idx];
        let now = FileStamp::of(path).ok()?;
        let same_file = if now.ino == cp.file.ino {
            now.size >= cp.file.size
        } else {
            // Compressed since the checkpoint; `open_at` verifies the offset.
            is_compressed(path)
        };
        if !same_file {
            return None;
        }
        let offset = (*ps.offsets.get(idx)?)?;
        let at = ResumePoint {
            offset: cp.byte_offset,
            t_us: ps.t_us,
        };
        let reader = PartsReader::resume(paths.to_vec(), idx, offset, at).ok()?;
        let transcript = AvtTranscript::restore(&cp.transcript, opts).ok()?;
        let mut clock = ClockMap::new();
        for &(t_us, epoch_us, step) in &ps.clock {
            clock.insert(ClockAnchor { t_us, epoch_us, step });
        }
        Some(Self {
            first: paths[0].clone(),
            base: base.clone(),
            reader,
            transcript,
            clock,
            seg: Segmenter::from_stack(cp.open_stack.clone()),
            out: Out {
                parts: true,
                ..Out::default()
            },
            ps,
            reset: false,
            announced: false,
            finished: false,
            frames: HashMap::new(),
        })
    }

    fn stamp(&self, t_us: u64) -> Stamp {
        let t = secs(t_us);
        Stamp {
            t,
            ts: self.clock.epoch_secs(t_us).unwrap_or(self.base.started_at + t),
        }
    }

    fn session(&self) -> SessionMeta {
        let mut s = self.base.clone();
        let paths = self.reader.paths();
        s.cast_path = paths.first().cloned().unwrap_or_else(|| self.first.clone());
        s.parts = paths.len().max(1) as u32;
        s.compressed = paths.iter().all(|p| is_compressed(p));
        s.cast_bytes = Some(paths.iter().filter_map(|p| self.uncompressed_len(p)).sum());
        s.tmux = self.ps.tmux.clone();
        s.state = self.ps.state;
        s.exit_status = self.ps.exit_status;
        s.ended_at = self.ps.ended_at.map(F64::get);
        s.counters = SessionCounters {
            bad_lines: self.ps.bad_lines + self.reader.stats().bad_lines,
            bad_markers: self.ps.bad_markers,
            lossy_bytes: self.ps.lossy_bytes,
        };
        s
    }

    fn parts(&self) -> Vec<CastPart> {
        self.reader
            .paths()
            .iter()
            .enumerate()
            .filter_map(|(i, path)| {
                let offset = (*self.ps.offsets.get(i)?)?;
                Some(CastPart {
                    part: part_no(path, i),
                    path: path.clone(),
                    compressed: is_compressed(path),
                    bytes: self.uncompressed_len(path),
                    t_offset: secs(offset),
                    frames: self.frames.get(path).cloned().flatten(),
                })
            })
            .collect()
    }

    /// Scans the frame table of every compressed part not seen yet. A rebuild thereby
    /// restores `zst_frames` and the uncompressed sizes the compressor recorded.
    fn scan_compressed(&mut self) {
        for path in self.reader.paths() {
            if is_compressed(path) && !self.frames.contains_key(path) {
                let frames = File::open(path)
                    .and_then(|mut f| scan_frames(&mut f))
                    .ok()
                    .map(|f| ZstFrameRow::from_frames(&f));
                self.frames.insert(path.clone(), frames);
            }
        }
    }

    /// Uncompressed size of a part: the file size, or the frame total of a compressed part.
    fn uncompressed_len(&self, path: &Path) -> Option<u64> {
        if is_compressed(path) {
            let frames = self.frames.get(path)?.as_ref()?;
            Some(frames.last().map_or(0, |f| f.u_offset + f.u_len))
        } else {
            std::fs::metadata(path).ok().map(|m| m.len())
        }
    }

    fn event(&mut self, ev: Event) {
        self.ps.t_us = ev.t_us;
        self.clock.observe(&ev);
        let new_part = self.reader.part_index() != self.ps.part_idx;
        if new_part {
            self.enter_part();
        }
        let at = self.stamp(ev.t_us);
        match ev.code {
            EventCode::Output => self.transcript.output(at, &ev.data),
            EventCode::Resize => {
                if let Some((cols, rows)) = parse_sane_resize(&ev.data) {
                    self.transcript.resize(at, cols, rows);
                }
            }
            EventCode::Marker => self.marker(&ev.data, at),
            EventCode::Exit => {
                self.ps.exit_status = ev.data.trim().parse().ok();
                self.ps.x_seen = true;
            }
            EventCode::Input | EventCode::Other(_) => {}
        }
        self.ps.segment_bytes += ev.data.len() as u64 + EVENT_OVERHEAD;
        let since = at.t - self.ps.segment.t_start();
        if !self.ps.x_seen && (new_part || segment_due(self.ps.segment_bytes, since)) {
            self.cut_segment(at);
        }
        self.out.changed = true;
    }

    /// The reader moved on to another part (rotation).
    fn enter_part(&mut self) {
        self.ps.part_idx = self.reader.part_index();
        for (i, o) in self.reader.part_offsets().iter().enumerate() {
            if let Some(o) = o {
                if self.ps.offsets.len() <= i {
                    self.ps.offsets.resize(i + 1, None);
                }
                self.ps.offsets[i] = Some(*o);
            }
        }
        if self.ps.state == SessionState::Degraded {
            self.ps.state = SessionState::Live;
        }
        // A resumed reader cannot know the terminal size at the end of the previous part,
        // so the part's own header decides (the reader's synthetic resize then is a no-op).
        if let Some(path) = self.reader.paths().get(self.ps.part_idx)
            && let Ok(r) = cinelog_cast::open(path)
        {
            let t = &r.header().term;
            let at = self.stamp(self.ps.t_us);
            self.transcript.resize(at, t.cols, t.rows);
        }
        self.out.parts = true;
    }

    fn cut_segment(&mut self, at: Stamp) {
        let mut cur = Segment::from(self.ps.segment.clone());
        cur.t_end = Some(at.t);
        let next = Segment {
            seq: cur.seq + 1,
            part: self
                .reader
                .paths()
                .get(self.ps.part_idx)
                .map_or(cur.part, |p| part_no(p, self.ps.part_idx)),
            t_start: at.t,
            t_end: None,
            byte_offset: self.reader.position(),
            vt_dump: self.transcript.vt_dump(),
        };
        self.out.segments.push(cur);
        self.out.segments.push(next.clone());
        self.ps.segment = next.into();
        self.ps.segment_bytes = 0;
    }

    fn marker(&mut self, data: &str, at: Stamp) {
        let Ok(Some(m)) = CastMarker::from_json(data) else {
            return;
        };
        if let Some(row) = MarkerRow::from_marker(&m, at) {
            self.out.markers.push(row);
        }
        match m {
            CastMarker::Cmd(rec) => self.on_cmd(&rec, at),
            CastMarker::Done(rec) => self.on_done(&rec, at),
            CastMarker::Ctx(ctx) => self.ps.tmux.apply_ctx(&ctx),
            CastMarker::Error(e) if e.degraded => self.ps.state = SessionState::Degraded,
            CastMarker::Unknown { k, fields } if k == "stats" => {
                let get = |key: &str| fields.get(key).and_then(serde_json::Value::as_u64);
                self.ps.bad_markers = get("bad_markers").unwrap_or(self.ps.bad_markers);
                self.ps.lossy_bytes = get("lossy_bytes").unwrap_or(self.ps.lossy_bytes);
            }
            _ => {}
        }
    }

    fn on_cmd(&mut self, rec: &CmdRecord, at: Stamp) {
        if self.seg.is_open(rec.seq) {
            return;
        }
        self.transcript.boundary(at, Owner::Command(rec.seq));
        let pushed = self.seg.push(rec.seq, rec.cmd.sh, rec.cmd.n);
        for e in &pushed.closed {
            self.close_command(e, None, at);
        }
        let cmd = Command::from_cmd(rec, at.t, at.ts, pushed.parent_seq, pushed.depth);
        self.out.commands.push(cmd.clone());
        self.ps.open.push(cmd.into());
    }

    fn on_done(&mut self, rec: &DoneRecord, at: Stamp) {
        let d = &rec.done;
        let Some(i) = self.seg.find(d.sh, d.n, rec.seq) else {
            return;
        };
        self.transcript.boundary(at, self.seg.owner_below(i));
        let popped = self.seg.pop(i);
        for e in &popped.unmatched {
            self.close_command(e, None, at);
        }
        self.close_command(&popped.matched, Some(d), at);
    }

    fn close_command(&mut self, e: &OpenCommand, done: Option<&cinelog_proto::Done>, at: Stamp) {
        let Some(i) = self.ps.open.iter().position(|c| c.seq() == e.seq) else {
            return;
        };
        let mut cmd = Command::from(self.ps.open.remove(i));
        cmd.close(done, at.t, at.ts);
        let stats = self.transcript.output_stats(Owner::Command(e.seq));
        cmd.out_bytes = stats.bytes;
        cmd.out_truncated = stats.truncated;
        self.transcript.forget_owner(Owner::Command(e.seq));
        self.out.commands.push(cmd);
    }

    /// Closes everything at the last event.
    fn finalize(&mut self, state: SessionState) {
        let at = self.stamp(self.ps.t_us);
        self.out.chunks.extend(self.transcript.take_chunks());
        self.out.chunks.extend(self.transcript.finish(at));
        for e in self.seg.drain() {
            self.close_command(&e, None, at);
        }
        self.out.provisional = Some(Vec::new());
        self.ps.provisional_out = false;
        self.ps.state = state;
        self.ps.ended_at.get_or_insert(F64::new(at.ts));
        let mut last = Segment::from(self.ps.segment.clone());
        last.t_end = Some(at.t);
        self.out.segments.push(last.clone());
        self.ps.segment = last.into();
        self.out.changed = true;
        self.finished = true;
    }

    /// Looks for part files created after the reader was opened: live rotation, and the
    /// continuation part after a disk error, whose part 1 ends without a `rotate` marker.
    /// Costs one or two stats per poll that reaches the end of the last part.
    fn follow_rotation(&mut self) -> Result<bool> {
        let idx = self.reader.part_index();
        let Some(cur) = self.reader.paths().get(idx) else {
            return Ok(false);
        };
        if idx + 1 < self.reader.paths().len() || existing_part(&self.first, part_no(cur, idx) + 1).is_none() {
            return Ok(false);
        }
        let paths = find_parts(&self.first);
        if paths.len() <= self.reader.paths().len() {
            return Ok(false);
        }
        let offset = self.ps.offsets.get(idx).copied().flatten().unwrap_or(0);
        let at = ResumePoint {
            offset: self.reader.position(),
            t_us: self.ps.t_us,
        };
        self.ps.bad_lines += self.reader.stats().bad_lines;
        self.reader = PartsReader::resume(paths, idx, offset, at)?;
        Ok(true)
    }

    fn checkpoint(&self) -> Checkpoint {
        let idx = self.reader.part_index();
        let path = self.reader.paths().get(idx);
        let mut ps = self.ps.clone();
        ps.part_idx = idx;
        ps.bad_lines += self.reader.stats().bad_lines;
        ps.clock = self
            .clock
            .anchors()
            .iter()
            .map(|a| (a.t_us, a.epoch_us, a.step))
            .collect();
        Checkpoint {
            part: path.map_or(1, |p| part_no(p, idx)),
            byte_offset: self.reader.position(),
            t: secs(self.ps.t_us),
            transcript: self.transcript.snapshot(),
            open_stack: self.seg.stack().to_vec(),
            pipeline_state: serde_json::to_string(&ps).expect("pipeline state serializes"),
            file: path.and_then(|p| FileStamp::of(p).ok()).unwrap_or_default(),
        }
    }

    fn batch(&mut self) -> IndexBatch {
        self.scan_compressed();
        self.out.chunks.extend(self.transcript.take_chunks());
        let out = std::mem::take(&mut self.out);
        let mut b = IndexBatch::new(self.session());
        b.reset = std::mem::take(&mut self.reset);
        if out.parts {
            b.parts = self.parts();
        }
        b.commands = last_by(out.commands, |c| c.seq);
        b.chunks = out.chunks;
        b.provisional = out.provisional;
        b.markers = out.markers;
        b.segments = last_by(out.segments, |s| u64::from(s.seq));
        b.checkpoint = Some(self.checkpoint());
        b.finished = self.finished;
        self.announced = true;
        b
    }
}

/// Keeps the last item per key, in the order of those last occurrences.
fn last_by<T>(items: Vec<T>, key: impl Fn(&T) -> u64) -> Vec<T> {
    let keys: Vec<u64> = items.iter().map(&key).collect();
    items
        .into_iter()
        .enumerate()
        .filter(|(i, _)| !keys[i + 1..].contains(&keys[*i]))
        .map(|(_, t)| t)
        .collect()
}

impl SessionPipeline for CastPipeline {
    fn uuid(&self) -> &str {
        &self.ps.uuid
    }

    fn poll(&mut self, budget: PollBudget) -> Result<Option<IndexBatch>> {
        if self.finished {
            return Ok(None);
        }
        let mut read = 0u64;
        let mut events = 0u64;
        while read < budget.max_bytes.max(1) && !self.ps.x_seen {
            match self.reader.next_event() {
                Some(Ok(ev)) => {
                    read += ev.data.len() as u64 + EVENT_OVERHEAD;
                    events += 1;
                    self.event(ev);
                }
                Some(Err(e)) => return Err(e.into()),
                None => {
                    if self.follow_rotation()? {
                        continue;
                    }
                    break;
                }
            }
        }
        if self.ps.x_seen {
            self.finalize(SessionState::Ended);
        } else if events > 0 {
            if budget.live {
                let p = match self.seg.owner() {
                    Owner::Command(_) => self.transcript.provisional(),
                    Owner::Gap => Vec::new(),
                };
                if !p.is_empty() || self.ps.provisional_out {
                    self.ps.provisional_out = !p.is_empty();
                    self.out.provisional = Some(p);
                }
            } else if self.ps.provisional_out {
                self.ps.provisional_out = false;
                self.out.provisional = Some(Vec::new());
            }
        }
        if self.announced && !self.out.changed {
            return Ok(None);
        }
        Ok(Some(self.batch()))
    }

    fn finish(&mut self, state: SessionState) -> Result<IndexBatch> {
        if self.finished {
            self.ps.state = state;
        } else {
            self.finalize(state);
        }
        Ok(self.batch())
    }

    fn is_finished(&self) -> bool {
        self.ps.x_seen
    }
}

#[cfg(test)]
#[path = "pipeline_tests.rs"]
mod tests;
