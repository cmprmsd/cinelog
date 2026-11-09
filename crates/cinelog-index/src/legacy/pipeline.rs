//! The [`SessionPipeline`] of one legacy cast (§11 steps 2–7).
//!
//! Commands are emitted as soon as their `Begin` banner is seen (typed text reconstructed
//! from the screen, so output chunks always refer to an existing command); once the cast is
//! read to its end, the banners are aligned with commands.log and every command is emitted
//! again with its final text and confidence, together with the `legacy_import` row. A cast
//! whose read stops early (`finish` before the end) gets a `failed` row.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use cinelog_cast::{CastReader, EventCode, ResumePoint};
use cinelog_proto::{Attribution, Scope};
use serde::{Deserialize, Serialize};

use super::TranscriptSource;
use super::align::{self, BannerView, Match, Prompts, SCREEN_LINES};
use super::banner::{self, BannerKind, Piece, Scanner, TitleScanner};
use super::log::{self, LogEntry};
use super::source::{self, Source};
use crate::api::{SessionPipeline, Transcript};
use crate::classify::{CastKind, classify};
use crate::error::{IndexError, Result};
use crate::model::{
    Checkpoint, Chunk, Command, FileStamp, IndexBatch, LegacyImportRow, LegacyOptions, LegacyStatus, OutputStats,
    Owner, PollBudget, Segment, SessionCounters, SessionMeta, SessionState, Stamp, TmuxLocation, legacy_uuid,
    segment_due,
};

/// Bumped whenever [`State`] changes incompatibly (older checkpoints are ignored).
const STATE_VERSION: u32 = 1;
/// Output after a `Begin` searched for the window title of its command.
const TITLE_WINDOW: u64 = 16 * 1024;
/// Caps of the screen lines kept per banner.
const MAX_LINE_CHARS: usize = 1024;
const MAX_SCREEN_BYTES: usize = 4096;
/// Plausible wall clock of a banner: the cast's span widened by the largest UTC offsets.
const WALL_SLACK_SECS: f64 = 15.0 * 3600.0 + 120.0;
/// A banner older than the previous one by more than this is replayed text (`cat` of an
/// old cast), not a live banner. One hour for the DST fall-back.
const WALL_BACKWARDS_SECS: i64 = 3600 + 5;
/// Clock cross-check tolerance (§11 step 6).
const DRIFT_SECS: f64 = 2.0;
/// `TERM=dumb` artefacts smaller than this with no commands are hidden (§11 step 7).
const HIDDEN_MAX_BYTES: u64 = 4096;

/// One `Begin` banner (= one command), as kept in the checkpoint.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct BannerRec {
    t_us: u64,
    /// The next `End` (or `Begin`, or the end of the cast).
    end_us: Option<u64>,
    /// Naive local wall clock of the banner, seconds.
    wall: i64,
    /// Logical lines above the cursor before the banner, oldest first.
    screen: Vec<String>,
    title: Option<String>,
    out: OutputStats,
}

/// Last playback segment cut.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct SegRec {
    seq: u32,
    t_us: u64,
    offset: u64,
    vt_dump: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
struct TitleWait {
    banner: usize,
    budget: u64,
}

/// File identity, with the mtime as its bit pattern so a checkpoint compares exactly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
struct StampKey(u64, u64, u64);

impl From<FileStamp> for StampKey {
    fn from(s: FileStamp) -> Self {
        Self(s.size, s.mtime.to_bits(), s.ino)
    }
}

/// Everything that influences later output (`Checkpoint::pipeline_state`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct State {
    version: u32,
    log: Option<StampKey>,
    t_us: u64,
    scanner: Scanner,
    titles: TitleScanner,
    title_for: Option<TitleWait>,
    banners: Vec<BannerRec>,
    open: Option<usize>,
    last_wall: Option<i64>,
    /// Banners dropped as implausible (counted in `bad_markers`).
    rejected: u64,
    /// Wall clocks of the accepted banners (their lines are removed from chunks).
    walls: BTreeSet<i64>,
    seg: Option<SegRec>,
    exit: Option<i32>,
    /// `bad_lines` of the readers of earlier runs.
    bad_lines: u64,
}

impl State {
    fn new(log: Option<StampKey>) -> Self {
        Self {
            version: STATE_VERSION,
            log,
            t_us: 0,
            scanner: Scanner::default(),
            titles: TitleScanner::default(),
            title_for: None,
            banners: Vec::new(),
            open: None,
            last_wall: None,
            rejected: 0,
            walls: BTreeSet::new(),
            seg: None,
            exit: None,
            bad_lines: 0,
        }
    }
}

pub(crate) struct LegacyPipeline {
    src: Source,
    uuid: String,
    log_path: Option<PathBuf>,
    log: Vec<LogEntry>,
    reader: Box<dyn CastReader + Send>,
    tr: Box<dyn Transcript>,
    session: SessionMeta,
    st: State,
    reset: bool,
    finished: bool,
    /// Banner indices whose command changed since the last batch.
    dirty: BTreeSet<usize>,
    chunks: Vec<Chunk>,
    segments: Vec<Segment>,
}

impl LegacyPipeline {
    pub(crate) fn open(
        opts: &LegacyOptions,
        tsrc: TranscriptSource,
        cast: &Path,
        checkpoint: Option<Checkpoint>,
    ) -> Result<Self> {
        if let CastKind::Cinelog { .. } = classify(cast)? {
            return Err(IndexError::Invalid(format!(
                "{}: a cinelog cast, not a legacy one",
                cast.display()
            )));
        }
        let src = Source::open(cast)?;
        let name = cast
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or_else(|| IndexError::Invalid(format!("{}: file name is not UTF-8", cast.display())))?;
        let uuid = legacy_uuid(name, src.stamp.size, &src.sha256);
        let log_file = source::log_path(cast);
        let (log, log_stamp) = match source::read_log(&log_file)? {
            Some((text, stamp)) => (log::parse(&text), Some(StampKey::from(stamp))),
            None => (Vec::new(), None),
        };
        let had_checkpoint = checkpoint.is_some();
        let resumed = checkpoint
            .filter(|c| c.part == 1 && c.file == src.stamp)
            .and_then(|c| {
                let st: State = serde_json::from_str(&c.pipeline_state).ok()?;
                (st.version == STATE_VERSION && st.log == log_stamp).then_some((c, st))
            })
            .and_then(|(c, st)| {
                let at = ResumePoint {
                    offset: c.byte_offset,
                    t_us: st.t_us,
                };
                let (reader, _) = src.reader(Some(at)).ok()?;
                let tr = (tsrc.restore)(&c.transcript, opts.render).ok()?;
                Some((reader, tr, st))
            });
        let reset = had_checkpoint && resumed.is_none();
        let (reader, tr, st) = match resumed {
            Some(r) => r,
            None => {
                let (reader, _) = src.reader(None)?;
                let (cols, rows) = src.header.term.size();
                let tr = (tsrc.new)(cols, rows, opts.render)?;
                (reader, tr, State::new(log_stamp))
            }
        };
        let session = session_meta(opts, &src, &uuid);
        Ok(Self {
            uuid,
            log_path: log_stamp.map(|_| log_file),
            log,
            reader,
            tr,
            session,
            st,
            reset,
            finished: false,
            dirty: BTreeSet::new(),
            chunks: Vec::new(),
            segments: Vec::new(),
            src,
        })
    }

    fn started_at(&self) -> f64 {
        self.session.started_at
    }

    fn stamp(&self, t_us: u64) -> Stamp {
        let t = secs(t_us);
        Stamp {
            t,
            ts: self.started_at() + t,
        }
    }

    fn handle(&mut self, pos: u64, t_us: u64, code: EventCode, data: &str) {
        self.st.t_us = t_us;
        self.maybe_cut_segment(pos, t_us);
        let at = self.stamp(t_us);
        match code {
            EventCode::Output => {
                let mut scanner = std::mem::take(&mut self.st.scanner);
                scanner.feed(at, data, &mut |p| self.piece(p));
                self.st.scanner = scanner;
            }
            EventCode::Resize => {
                // Implausible sizes (the old plugin's tmux bugs wrote resizes to 3350
                // columns) are dropped here as in the transcript and the player.
                if let Some((cols, rows)) = cinelog_cast::parse_sane_resize(data) {
                    // A banner may straddle the resize: keep its prefix, now timed here.
                    self.st.scanner.retime(at);
                    self.tr.resize(at, cols, rows);
                }
            }
            EventCode::Exit => self.st.exit = data.trim().parse().ok(),
            EventCode::Input | EventCode::Marker | EventCode::Other(_) => {}
        }
    }

    fn flush_carry(&mut self) {
        let mut scanner = std::mem::take(&mut self.st.scanner);
        scanner.flush(&mut |p| self.piece(p));
        self.st.scanner = scanner;
    }

    fn piece(&mut self, p: Piece<'_>) {
        match p {
            Piece::Text { at, text } => {
                self.tr.output(at, text);
                self.watch_title(text);
            }
            Piece::Banner { at, kind, wall } => self.banner(at, kind, wall),
        }
    }

    fn watch_title(&mut self, text: &str) {
        let Some(w) = self.st.title_for else { return };
        let mut titles = Vec::new();
        self.st.titles.feed(text, &mut titles);
        if let Some(title) = titles.into_iter().next() {
            self.st.banners[w.banner].title = Some(title);
            self.st.title_for = None;
            return;
        }
        let budget = w.budget.saturating_sub(text.len() as u64);
        self.st.title_for = (budget > 0).then_some(TitleWait { budget, ..w });
    }

    fn banner(&mut self, at: Stamp, kind: BannerKind, wall: i64) {
        if !self.plausible_wall(at, wall) {
            self.st.rejected += 1;
            return;
        }
        // `at` is the time of the event the banner starts in, which may precede the current
        // one when it was carried over; command and boundary share it.
        let t_us = us_of(at.t);
        self.st.walls.insert(wall);
        match kind {
            BannerKind::Begin => {
                let screen = cap_screen(self.tr.lines_above_cursor(SCREEN_LINES));
                // tmux redraws repeat banners that are still visible.
                if self.tr.is_alt_screen()
                    && self
                        .st
                        .banners
                        .last()
                        .is_some_and(|b| b.wall == wall && b.screen.last() == screen.last())
                {
                    return;
                }
                self.st.last_wall = Some(self.st.last_wall.map_or(wall, |w| w.max(wall)));
                self.close_open(t_us);
                let idx = self.st.banners.len();
                self.st.banners.push(BannerRec {
                    t_us,
                    end_us: None,
                    wall,
                    screen,
                    title: None,
                    out: OutputStats::default(),
                });
                self.st.open = Some(idx);
                self.tr.boundary(at, Owner::Command(seq_of(idx)));
                self.st.titles = TitleScanner::default();
                self.st.title_for = Some(TitleWait {
                    banner: idx,
                    budget: TITLE_WINDOW,
                });
                self.dirty.insert(idx);
            }
            BannerKind::End => {
                self.st.last_wall = Some(self.st.last_wall.map_or(wall, |w| w.max(wall)));
                // Titles after `End` come from precmd, not from the command.
                self.st.title_for = None;
                if self.st.open.is_some() {
                    self.close_open(t_us);
                    self.tr.boundary(at, Owner::Gap);
                }
            }
        }
    }

    fn close_open(&mut self, t_us: u64) {
        if let Some(i) = self.st.open.take() {
            self.st.banners[i].end_us = Some(t_us);
            self.dirty.insert(i);
        }
    }

    /// A live banner's naive wall clock lies within the cast's span (in some UTC offset)
    /// and does not run backwards by more than a DST step.
    fn plausible_wall(&self, at: Stamp, wall: i64) -> bool {
        let w = wall as f64;
        let upper = self.src.stamp.mtime.max(at.ts) + WALL_SLACK_SECS;
        if w < self.started_at() - WALL_SLACK_SECS || w > upper {
            return false;
        }
        self.st.last_wall.is_none_or(|last| wall >= last - WALL_BACKWARDS_SECS)
    }

    fn maybe_cut_segment(&mut self, pos: u64, t_us: u64) {
        let due = match &self.st.seg {
            None => true,
            Some(s) => segment_due(pos.saturating_sub(s.offset), secs(t_us.saturating_sub(s.t_us))),
        };
        if !due {
            return;
        }
        let next = SegRec {
            seq: self.st.seg.as_ref().map_or(0, |s| s.seq + 1),
            t_us,
            offset: pos,
            vt_dump: self.tr.snapshot().vt_dump,
        };
        if let Some(prev) = self.st.seg.take() {
            self.segments.push(segment(&prev, Some(secs(t_us))));
        }
        self.segments.push(segment(&next, None));
        self.st.seg = Some(next);
    }

    fn take_chunks(&mut self, extra: Vec<Chunk>) {
        let chunks = self.tr.take_chunks().into_iter().chain(extra);
        let walls = &self.st.walls;
        self.chunks.extend(chunks.filter_map(|mut c| {
            c.text = banner::strip_banner_lines(&c.text, |w| walls.contains(&w))?;
            Some(c)
        }));
    }

    fn command(&self, idx: usize, typed: String, trunc: bool, confidence: Option<f64>) -> Command {
        let b = &self.st.banners[idx];
        let t_start = secs(b.t_us);
        let t_end = b.end_us.map(secs);
        let out = if self.st.open == Some(idx) {
            self.tr.output_stats(Owner::Command(seq_of(idx)))
        } else {
            b.out
        };
        Command {
            seq: seq_of(idx),
            parent_seq: None,
            depth: 0,
            shell_pid: None,
            t_start,
            t_end,
            ts_start: self.started_at() + t_start,
            ts_end: t_end.map(|t| self.started_at() + t),
            exit: None,
            pipestatus: Vec::new(),
            cwd: None,
            user: None,
            euid: None,
            typed,
            expanded: None,
            typed_trunc: trunc,
            approx: true,
            confidence,
            out_bytes: out.bytes,
            out_truncated: out.truncated,
        }
    }

    fn update_counters(&mut self, drifted: u64) {
        self.session.counters = SessionCounters {
            bad_lines: self.st.bad_lines + self.reader.stats().bad_lines,
            bad_markers: self.st.rejected + drifted,
            lossy_bytes: 0,
        };
    }

    fn checkpoint(&self) -> Result<Checkpoint> {
        let mut st = self.st.clone();
        st.bad_lines += self.reader.stats().bad_lines;
        Ok(Checkpoint {
            part: 1,
            byte_offset: self.reader.position(),
            t: secs(self.st.t_us),
            transcript: self.tr.snapshot(),
            open_stack: Vec::new(),
            pipeline_state: serde_json::to_string(&st)?,
            file: self.src.stamp,
        })
    }

    /// The batch of everything collected since the last one.
    fn batch(&mut self) -> Result<IndexBatch> {
        self.take_chunks(Vec::new());
        // Running commands get their current output stats.
        for i in std::mem::take(&mut self.dirty) {
            if self.st.open != Some(i) {
                self.st.banners[i].out = self.tr.output_stats(Owner::Command(seq_of(i)));
            }
        }
        self.update_counters(0);
        let mut b = IndexBatch::new(self.session.clone());
        b.reset = std::mem::take(&mut self.reset);
        b.chunks = std::mem::take(&mut self.chunks);
        b.segments = std::mem::take(&mut self.segments);
        b.checkpoint = Some(self.checkpoint()?);
        b.parts = vec![self.part()];
        Ok(b)
    }

    fn part(&self) -> crate::model::CastPart {
        crate::model::CastPart {
            part: 1,
            path: self.src.path.clone(),
            compressed: self.src.zstd,
            bytes: self.session.cast_bytes,
            t_offset: 0.0,
            frames: None,
        }
    }

    /// Commands touched since the last batch, with the reconstructed text.
    fn provisional_commands(&self, dirty: &BTreeSet<usize>) -> Vec<Command> {
        let prompts = Prompts::default();
        dirty
            .iter()
            .map(|&i| self.command(i, prompts.reconstruct(&self.st.banners[i].screen), false, None))
            .collect()
    }

    /// Aligns what was read: the last batch of the session. `eof`: the cast was read to its
    /// end; otherwise the import row is `failed` (the stamp still records the file).
    fn finalize(&mut self, state: SessionState, eof: bool) -> Result<IndexBatch> {
        self.flush_carry();
        if eof && self.src.zstd {
            self.session.cast_bytes = Some(self.reader.position());
        }
        let end_us = self.st.t_us;
        let end = self.stamp(end_us);
        if let Some(i) = self.st.open.take() {
            self.st.banners[i].end_us = Some(end_us);
        }
        let rest = self.tr.finish(end);
        self.take_chunks(rest);
        for i in 0..self.st.banners.len() {
            self.st.banners[i].out = self.tr.output_stats(Owner::Command(seq_of(i)));
        }
        if let Some(last) = self.st.seg.clone() {
            self.segments.push(segment(&last, Some(end.t)));
        }

        let views: Vec<BannerView<'_>> = self
            .st
            .banners
            .iter()
            .map(|b| BannerView {
                screen: &b.screen,
                title: b.title.as_deref(),
            })
            .collect();
        let matches = align::align(&views, &self.log);
        let prompts = Prompts::learn(&views, &self.log, &matches);
        let commands = self.final_commands(&matches, &prompts);
        let drifted = self.drifted();

        self.dirty.clear();
        let mut b = self.batch()?;
        self.update_counters(drifted);
        let hidden = eof && is_dumb(&self.src) && commands.is_empty() && self.src.stamp.size < HIDDEN_MAX_BYTES;
        let stopped = (!eof).then(|| {
            format!(
                "read stopped at offset {} before the end ({})",
                self.reader.position(),
                state.as_str()
            )
        });
        self.session.state = state;
        self.session.ended_at = Some(end.ts);
        self.session.exit_status = self.st.exit;
        self.session.hidden = hidden;
        b.session = self.session.clone();
        b.commands = commands;
        b.provisional = Some(Vec::new());
        b.legacy = Some(self.import_row(&matches, hidden, stopped));
        b.finished = true;
        self.finished = true;
        Ok(b)
    }

    /// One command per banner; log entries without a banner are not commands (§11 step 7).
    fn final_commands(&self, matches: &[Match], prompts: &Prompts) -> Vec<Command> {
        let mut by_banner: Vec<Option<&Match>> = vec![None; self.st.banners.len()];
        for m in matches {
            by_banner[m.banner] = Some(m);
        }
        by_banner
            .iter()
            .enumerate()
            .map(|(i, m)| match m {
                Some(m) => {
                    let e = &self.log[m.entry];
                    self.command(i, e.text.clone(), e.truncated, Some(round3(m.score)))
                }
                None => self.command(i, prompts.reconstruct(&self.st.banners[i].screen), false, None),
            })
            .collect()
    }

    /// Banners whose wall clock minus `started_at + t` leaves the first banner's offset
    /// by more than [`DRIFT_SECS`] after removing whole-hour steps (DST, tz change).
    fn drifted(&self) -> u64 {
        let started = self.started_at();
        let mut offsets = self.st.banners.iter().map(|b| b.wall as f64 - (started + secs(b.t_us)));
        let Some(first) = offsets.next() else { return 0 };
        offsets
            .filter(|d| {
                let delta = d - first;
                (delta - (delta / 3600.0).round() * 3600.0).abs() > DRIFT_SECS
            })
            .count() as u64
    }

    fn import_row(&self, matches: &[Match], hidden: bool, stopped: Option<String>) -> LegacyImportRow {
        let parsed: Vec<&Match> = matches.iter().filter(|m| self.log[m.entry].parsed).collect();
        let mean_conf =
            (!parsed.is_empty()).then(|| round3(parsed.iter().map(|m| m.score).sum::<f64>() / parsed.len() as f64));
        LegacyImportRow {
            path: self.src.path.clone(),
            file: self.src.stamp,
            sha256: self.src.sha256.clone(),
            commands_log: self.log_path.clone(),
            status: if stopped.is_some() {
                LegacyStatus::Failed
            } else if hidden {
                LegacyStatus::Hidden
            } else {
                LegacyStatus::Imported
            },
            session_uuid: Some(self.uuid.clone()),
            banners: self.st.banners.len() as u64,
            log_lines: self.log.iter().filter(|e| e.parsed).count() as u64,
            aligned: parsed.len() as u64,
            mean_conf,
            error: stopped,
        }
    }
}

impl SessionPipeline for LegacyPipeline {
    fn uuid(&self) -> &str {
        &self.uuid
    }

    fn poll(&mut self, budget: PollBudget) -> Result<Option<IndexBatch>> {
        if self.finished {
            return Ok(None);
        }
        let start = self.reader.position();
        let mut eof = false;
        while self.reader.position() - start < budget.max_bytes.max(1) {
            let pos = self.reader.position();
            match self.reader.next_event() {
                None => {
                    eof = true;
                    break;
                }
                Some(Err(e)) => return Err(e.into()),
                Some(Ok(ev)) => self.handle(pos, ev.t_us, ev.code, &ev.data),
            }
        }
        if eof {
            return self.finalize(SessionState::Ended, true).map(Some);
        }
        let dirty = self.dirty.clone();
        let mut b = self.batch()?;
        b.commands = self.provisional_commands(&dirty);
        Ok(Some(b))
    }

    fn finish(&mut self, state: SessionState) -> Result<IndexBatch> {
        if self.finished {
            let mut session = self.session.clone();
            session.state = state;
            let mut b = IndexBatch::new(session);
            b.finished = true;
            return Ok(b);
        }
        self.finalize(state, false)
    }

    fn is_finished(&self) -> bool {
        self.finished
    }
}

fn session_meta(opts: &LegacyOptions, src: &Source, uuid: &str) -> SessionMeta {
    let owner = source::user_name(src.uid);
    let (user, euid) = match opts.scope {
        Scope::Team => (source::team_user(&opts.logs_root, &src.path).unwrap_or(owner), 0),
        Scope::Personal => (owner, src.uid),
    };
    let started_at = src
        .header
        .timestamp
        .map(|t| t as f64)
        .or_else(|| source::start_from_name(&src.path))
        .unwrap_or(src.stamp.mtime);
    SessionMeta {
        uuid: uuid.to_owned(),
        host: opts.host.clone(),
        scope: opts.scope,
        user,
        euid: Some(euid),
        sudo_user: None,
        claimed_user: None,
        attribution: Some(attribution_text()),
        login_via: None,
        tty: None,
        holder_comm: None,
        tmux: TmuxLocation::default(),
        parent_uuid: None,
        cast_path: src.path.clone(),
        parts: 1,
        compressed: src.zstd,
        // Uncompressed size; for zstd known once read to the end.
        cast_bytes: (!src.zstd).then_some(src.stamp.size),
        started_at,
        ended_at: None,
        exit_status: None,
        tz: jiff::tz::TimeZone::system().iana_name().map(str::to_owned),
        cols: src.header.term.size().0,
        rows: src.header.term.size().1,
        term: term_of(src),
        legacy: true,
        hidden: false,
        state: SessionState::Live,
        counters: SessionCounters::default(),
    }
}

fn attribution_text() -> String {
    match serde_json::to_value(Attribution::Legacy) {
        Ok(serde_json::Value::String(s)) => s,
        _ => "legacy".to_owned(),
    }
}

fn term_of(src: &Source) -> Option<String> {
    src.header
        .term
        .term_type
        .clone()
        .or_else(|| src.header.env.as_ref().and_then(|e| e.get("TERM").cloned()))
}

fn is_dumb(src: &Source) -> bool {
    term_of(src).as_deref() == Some("dumb")
}

fn seq_of(idx: usize) -> u64 {
    idx as u64 + 1
}

fn secs(us: u64) -> f64 {
    us as f64 / 1e6
}

/// Inverse of [`secs`] (exact for every `secs(us)`).
fn us_of(t: f64) -> u64 {
    (t * 1e6).round() as u64
}

fn round3(v: f64) -> f64 {
    (v * 1000.0).round() / 1000.0
}

fn segment(s: &SegRec, t_end: Option<f64>) -> Segment {
    Segment {
        seq: s.seq,
        part: 1,
        t_start: secs(s.t_us),
        t_end,
        byte_offset: s.offset,
        vt_dump: s.vt_dump.clone(),
    }
}

/// Keeps the newest lines within [`MAX_SCREEN_BYTES`], each cut to [`MAX_LINE_CHARS`].
fn cap_screen(lines: Vec<String>) -> Vec<String> {
    let mut out: Vec<String> = Vec::with_capacity(lines.len());
    let mut total = 0;
    for l in lines.into_iter().rev() {
        let l = match l.char_indices().nth(MAX_LINE_CHARS) {
            // Keep the end: the command is at the end of its line.
            Some(_) => {
                let skip = l.chars().count() - MAX_LINE_CHARS;
                l.chars().skip(skip).collect()
            }
            None => l,
        };
        total += l.len();
        if total > MAX_SCREEN_BYTES && !out.is_empty() {
            break;
        }
        out.push(l);
    }
    out.reverse();
    out
}
