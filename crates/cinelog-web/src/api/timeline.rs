//! `GET /api/timeline` (work package M4-B; plan §8.3, §15.2; docs/API.md §9.3): the command
//! blocks, the "not recorded here" marks and the annotations of every session active in a
//! window, grouped into lanes (people or panes) and rows.
//!
//! Provides, with exactly this signature, [`routes`] (merged by `api::routes`):
//! `GET /timeline` (`TimelineParams`) → `Timeline`. Reads index.db read-only (one snapshot)
//! and the annotations of the window's sessions from meta.db
//! (`MetaStore::find_annotations`).

use std::collections::{BTreeMap, HashMap, HashSet};

use axum::Json;
use axum::Router;
use axum::extract::State;
use axum::routing::get;
use cinelog_cast::{ClockAnchor, ClockMap};
use cinelog_index::model::{self as im, AnnotationQuery};
use rusqlite::{Connection, params};
use serde_json::Value;

use super::db::{DbResult, Forgets, IndexDb, SessionFilter, SessionRow, blocking};
use super::extract::ApiQuery;
use super::types::{
    DensityBin, Scope, Session, SessionState, TimeSpan, Timeline, TimelineAnnotation, TimelineBlock, TimelineGroup,
    TimelineLane, TimelineMark, TimelineMarkKind, TimelineParams, TimelineRow, TimelineSession,
};
use crate::error::{Result, WebError};
use crate::play::source::{secs_to_us, us_to_secs};
use crate::state::AppState;

const DAY: f64 = 86_400.0;
/// Longest window a request may ask for.
const MAX_WINDOW: f64 = 31.0 * DAY;
/// More commands than this in the window: density bins instead of blocks.
pub(crate) const AGGREGATE_ABOVE: u64 = 20_000;
/// Candidate density bin widths, smallest first.
const BIN_SIZES: [f64; 8] = [60.0, 300.0, 900.0, 1800.0, 3600.0, 10_800.0, 21_600.0, 86_400.0];
const MAX_BINS: f64 = 2000.0;
/// Shortest stretch without any block or mark that is drawn as a break.
const IDLE_MIN: f64 = 600.0;
/// Room kept around a point mark (skip, nested) so it never sits inside an idle break.
const POINT_PAD: f64 = 60.0;
/// Characters of `typed` sent as hover text.
pub(crate) const TYPED_MAX: usize = 200;
/// Marker kinds the timeline reads (the rest is playback's business).
const MARKER_KINDS: &str = "'pause', 'resume', 'skip', 'nested', 'gap', 'clock'";

pub(crate) fn routes() -> Router<AppState> {
    Router::new().route("/timeline", get(timeline))
}

async fn timeline(State(st): State<AppState>, ApiQuery(p): ApiQuery<TimelineParams>) -> Result<Json<Timeline>> {
    blocking(move || build(&st, &p, crate::now())).await.map(Json)
}

// ------------------------------------------------------------------ window and grouping

/// The requested ts window, `from < to`.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Window {
    from: f64,
    to: f64,
}

impl Window {
    /// Defaults to the 24 h ending at `to` (or `now`), or starting at a lone `from`.
    fn of(p: &TimelineParams, now: f64) -> Result<Self> {
        for (name, v) in [("from", p.from), ("to", p.to)] {
            if v.is_some_and(|x| !x.is_finite()) {
                return Err(WebError::BadRequest(format!("{name} must be a finite number")));
            }
        }
        let (from, to) = match (p.from, p.to) {
            (Some(f), Some(t)) => (f, t),
            (Some(f), None) => (f, f + DAY),
            (None, Some(t)) => (t - DAY, t),
            (None, None) => (now - DAY, now),
        };
        if from >= to {
            return Err(WebError::BadRequest("from must be before to".into()));
        }
        if to - from > MAX_WINDOW {
            return Err(WebError::BadRequest("the timeline window is at most 31 days".into()));
        }
        Ok(Self { from, to })
    }

    /// Whether `[start, end]` touches the window.
    fn overlaps(self, start: f64, end: f64) -> bool {
        start <= self.to && end >= self.from
    }

    /// Smallest bin width that gives at most [`MAX_BINS`] bins.
    fn bin_secs(self) -> f64 {
        let len = self.to - self.from;
        BIN_SIZES
            .into_iter()
            .find(|b| (len / b).ceil() <= MAX_BINS)
            .unwrap_or(BIN_SIZES[BIN_SIZES.len() - 1])
    }
}

/// The person a session belongs to: `sudo_user`, else `claimed_user`, else `user` (TEAM root
/// shells belong to the member who opened them).
fn person(s: &Session) -> &str {
    fn set(v: &Option<String>) -> Option<&str> {
        v.as_deref().filter(|v| !v.is_empty())
    }
    set(&s.sudo_user).or_else(|| set(&s.claimed_user)).unwrap_or(&s.user)
}

/// Where a session is drawn: its tmux pane, else its terminal, else itself (key, label).
fn slot(s: &Session) -> (String, String) {
    let t = &s.tmux;
    if let Some(pane) = t.pane.as_deref().filter(|p| !p.is_empty()) {
        let server = t.server.as_deref().unwrap_or("-");
        let label = match (t.session.as_deref(), t.window.as_deref()) {
            (None, None) => pane.to_owned(),
            (session, window) => format!("{}:{}.{pane}", session.unwrap_or("?"), window.unwrap_or("?")),
        };
        return (format!("pane:{}/{server}/{pane}", s.host), label);
    }
    let holder = s.holder_comm.as_deref().filter(|h| !h.is_empty()).unwrap_or("shell");
    match s.tty.as_deref().filter(|t| !t.is_empty()) {
        Some(tty) => (
            format!("tty:{}/{tty}", s.host),
            format!("{holder} {}", tty.strip_prefix("/dev/").unwrap_or(tty)),
        ),
        None => (format!("session:{}", s.uuid), format!("{holder} {}", short(&s.uuid))),
    }
}

fn short(uuid: &str) -> &str {
    uuid.get(..8).unwrap_or(uuid)
}

/// First line of `typed`, at most [`TYPED_MAX`] characters.
pub(crate) fn first_line(typed: &str) -> String {
    let line = typed.split('\n').next().unwrap_or_default().trim_end_matches('\r');
    line.chars().take(TYPED_MAX).collect()
}

/// Gaps of at least [`IDLE_MIN`] in the window that no busy span covers.
fn idle_spans(mut busy: Vec<(f64, f64)>, w: Window) -> Vec<TimeSpan> {
    busy.sort_by(|a, b| a.0.total_cmp(&b.0));
    let mut out = Vec::new();
    let mut cursor = w.from;
    for (start, end) in busy {
        let (start, end) = (start.max(w.from), end.min(w.to));
        if start - cursor >= IDLE_MIN {
            out.push(TimeSpan {
                from: cursor,
                to: start,
            });
        }
        cursor = cursor.max(end);
    }
    if w.to - cursor >= IDLE_MIN {
        out.push(TimeSpan { from: cursor, to: w.to });
    }
    out
}

// ------------------------------------------------------------------ sessions

/// A session of the window with what the timeline derives from it.
struct Sess {
    row: SessionRow,
    /// End of activity in ts: now while live, else the end (or the indexed length).
    end: f64,
    /// Cast seconds → ts, from the session start and its `clock` markers.
    clock: ClockMap,
    /// The session whose row this one is drawn in (itself, or its outermost parent).
    root: usize,
}

impl Sess {
    fn new(row: SessionRow, now: f64) -> Self {
        let s = &row.session;
        let end = match (s.state, s.ended_at) {
            (SessionState::Live, _) => now.max(s.started_at),
            (_, Some(e)) => e,
            (_, None) => s.started_at + s.duration.unwrap_or(0.0),
        };
        let clock = ClockMap::with_start((s.started_at * 1e6).round() as i64);
        Self {
            row,
            end,
            clock,
            root: 0,
        }
    }

    fn uuid(&self) -> &str {
        &self.row.session.uuid
    }

    fn live(&self) -> bool {
        self.row.session.state == SessionState::Live
    }

    /// ts of session second `t`.
    fn ts(&self, t: f64) -> f64 {
        wall_ts(&self.clock, self.row.session.started_at, t)
    }
}

/// Index of the outermost ancestor of `i` that is in the window (cycle-safe).
fn root_of(sessions: &[Sess], by_uuid: &HashMap<String, usize>, i: usize) -> usize {
    let mut cur = i;
    let mut seen = HashSet::from([i]);
    while let Some(p) = sessions[cur].row.session.parent_uuid.as_deref()
        && let Some(&pi) = by_uuid.get(p)
        && seen.insert(pi)
    {
        cur = pi;
    }
    cur
}

/// What a session has forgotten, for the per-command check.
#[derive(Default)]
struct Forgot {
    seqs: HashSet<u64>,
    ranges: Vec<(f64, f64)>,
}

impl Forgot {
    fn of(forgets: &Forgets, uuid: &str) -> Self {
        Self {
            seqs: forgets.command_seqs(uuid).collect(),
            ranges: forgets.ranges(uuid).map(|(a, b, _)| (a, b)).collect(),
        }
    }

    /// The index rule (`Forgets::command`): by seq, or wholly inside a forgotten range.
    fn command(&self, seq: u64, t_start: f64, t_end: Option<f64>) -> bool {
        self.seqs.contains(&seq) || t_end.is_some_and(|end| self.ranges.iter().any(|&(a, b)| t_start >= a && end <= b))
    }
}

// ------------------------------------------------------------------ commands and marks

/// A block with its drawn end (ts).
struct Block {
    block: TimelineBlock,
    end: f64,
    failed: bool,
}

/// The visible commands of session `i` that overlap the window, by `ts_start`.
fn commands(conn: &Connection, sessions: &[Sess], i: usize, forgot: &Forgot, w: Window) -> DbResult<Vec<Block>> {
    let s = &sessions[i];
    // `+` keeps SQLite on the per-session index instead of scanning every command by ts.
    let mut st = conn.prepare_cached(
        "SELECT seq, t_start, t_end, ts_start, ts_end, exit, substr(typed, 1, ?4), cwd, approx, depth
         FROM commands WHERE session_id = ?1 AND +ts_start <= ?2 AND (+ts_end IS NULL OR +ts_end >= ?3)
         ORDER BY ts_start, id",
    )?;
    let rows = st.query_map(params![s.row.id, w.to, w.from, TYPED_MAX as i64], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            r.get::<_, f64>(1)?,
            r.get::<_, Option<f64>>(2)?,
            r.get::<_, f64>(3)?,
            r.get::<_, Option<f64>>(4)?,
            r.get::<_, Option<i32>>(5)?,
            r.get::<_, String>(6)?,
            r.get::<_, Option<String>>(7)?,
            r.get::<_, bool>(8)?,
            r.get::<_, u32>(9)?,
        ))
    })?;
    let mut out = Vec::new();
    for row in rows {
        let (seq, t_start, t_end, ts_start, ts_end, exit, typed, cwd, approx, depth) = row?;
        let seq = u64::try_from(seq).unwrap_or(0);
        if forgot.command(seq, t_start, t_end) {
            continue;
        }
        let end = ts_end.unwrap_or(s.end).max(ts_start);
        if !w.overlaps(ts_start, end) {
            continue;
        }
        out.push(Block {
            block: TimelineBlock {
                session: s.uuid().to_owned(),
                seq,
                t_start,
                ts_start,
                ts_end,
                exit,
                typed: first_line(&typed),
                cwd,
                approx,
                depth,
            },
            end,
            failed: exit.is_some_and(|e| e != 0),
        });
    }
    Ok(out)
}

/// The timeline's markers of a session, `t` order.
fn markers(conn: &Connection, session_id: i64) -> DbResult<Vec<im::MarkerRow>> {
    let mut st = conn.prepare_cached(&format!(
        "SELECT t, ts, kind, data_json FROM markers WHERE session_id = ?1 AND kind IN ({MARKER_KINDS}) ORDER BY t, id"
    ))?;
    let rows = st.query_map([session_id], |r| {
        Ok(im::MarkerRow {
            t: r.get(0)?,
            ts: r.get(1)?,
            kind: r.get(2)?,
            data_json: r.get(3)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

fn data_of(m: &im::MarkerRow) -> serde_json::Map<String, Value> {
    match serde_json::from_str(&m.data_json) {
        Ok(Value::Object(o)) => o,
        _ => serde_json::Map::new(),
    }
}

fn text(d: &serde_json::Map<String, Value>, k: &str) -> Option<String> {
    d.get(k)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

fn num(d: &serde_json::Map<String, Value>, k: &str) -> Option<f64> {
    d.get(k).and_then(Value::as_f64).filter(|v| v.is_finite())
}

/// ts of second `t` of a session that started at `started_at`, through its cast-time → ts
/// map.
pub(crate) fn wall_ts(clock: &ClockMap, started_at: f64, t: f64) -> f64 {
    clock.epoch_secs(secs_to_us(t)).unwrap_or(started_at + t)
}

/// Adds the `clock` markers of `list` (other kinds are skipped) to a cast-time → ts map.
pub(crate) fn add_clock_anchors(clock: &mut ClockMap, list: &[im::MarkerRow]) {
    for m in list.iter().filter(|m| m.kind == "clock") {
        let d = data_of(m);
        if let Some(epoch_us) = d.get("epoch_us").and_then(Value::as_i64) {
            clock.insert(ClockAnchor {
                t_us: secs_to_us(m.t),
                epoch_us,
                step: d.get("reason").and_then(Value::as_str) == Some("step"),
            });
        }
    }
}

/// Adds the `clock` markers to the session's cast-time → ts map; the end of a session that
/// was never closed (its indexed length) moves with them.
fn add_clock(s: &mut Sess, list: &[im::MarkerRow]) {
    add_clock_anchors(&mut s.clock, list);
    let x = &s.row.session;
    if !s.live() && x.ended_at.is_none() {
        s.end = s.ts(x.duration.unwrap_or(0.0)).max(x.started_at);
    }
}

fn mark(session: &str, kind: TimelineMarkKind, ts: f64, ts_end: Option<f64>) -> TimelineMark {
    TimelineMark {
        session: session.to_owned(),
        kind,
        ts,
        ts_end,
        reason: None,
        detail: None,
        child: None,
        child_scope: None,
        child_url: None,
    }
}

/// Pauses, skips, nested starts and gaps of a session from its markers (unclipped).
fn marker_marks(s: &Sess, list: &[im::MarkerRow], forgets: &Forgets) -> Vec<TimelineMark> {
    let uuid = s.uuid();
    let mut out = Vec::new();
    let mut paused: Option<TimelineMark> = None;
    for m in list {
        let d = data_of(m);
        match m.kind.as_str() {
            "pause" if paused.is_none() => {
                let mut p = mark(uuid, TimelineMarkKind::Pause, m.ts, None);
                p.reason = text(&d, "reason");
                p.detail = text(&d, "detail");
                paused = Some(p);
            }
            "resume" => {
                if let Some(mut p) = paused.take() {
                    p.ts_end = Some(m.ts.max(p.ts));
                    out.push(p);
                }
            }
            "skip" => out.push(mark(uuid, TimelineMarkKind::Skip, m.ts, None)),
            "nested" => {
                let mut n = mark(uuid, TimelineMarkKind::Nested, m.ts, None);
                n.reason = text(&d, "mode");
                n.detail = text(&d, "user");
                n.child = text(&d, "child_uuid").filter(|c| !forgets.session(c));
                if n.child.is_some() {
                    n.child_scope = d
                        .get("scope")
                        .and_then(|v| serde_json::from_value::<Scope>(v.clone()).ok());
                    n.child_url = text(&d, "url");
                }
                out.push(n);
            }
            "gap" => {
                // `from_t`/`to_t` are offsets in the cast part; the marker is written at `to_t`.
                let len = match (num(&d, "from_t"), num(&d, "to_t")) {
                    (Some(a), Some(b)) if b >= a => b - a,
                    _ => 0.0,
                };
                let mut g = mark(uuid, TimelineMarkKind::Gap, s.ts((m.t - len).max(0.0)), Some(m.ts));
                g.detail = num(&d, "dropped_bytes").map(|b| format!("{b} bytes dropped"));
                out.push(g);
            }
            _ => {}
        }
    }
    if let Some(mut p) = paused {
        // Still paused: open while live, else until the session ended.
        p.ts_end = (!s.live()).then_some(s.end.max(p.ts));
        out.push(p);
    }
    out
}

/// What playback masks of a session (`SessionDetail.masked`), in ts.
fn forgotten_marks(s: &Sess, mask: &crate::play::stream::Mask) -> Vec<TimelineMark> {
    mask.spans
        .iter()
        .map(|span| {
            let to = (span.to_us != u64::MAX).then(|| s.ts(us_to_secs(span.to_us)));
            let mut m = mark(
                s.uuid(),
                TimelineMarkKind::Forgotten,
                s.ts(us_to_secs(span.from_us)),
                to,
            );
            m.detail = Some(span.by.clone());
            m
        })
        .collect()
}

fn is_point(m: &TimelineMark) -> bool {
    matches!(m.kind, TimelineMarkKind::Skip | TimelineMarkKind::Nested)
}

/// The drawn extent of a mark; an open span lasts until `open_end`.
fn mark_extent(m: &TimelineMark, open_end: f64) -> (f64, f64) {
    let end = match m.ts_end {
        Some(e) => e,
        None if is_point(m) => m.ts,
        None => open_end,
    };
    (m.ts, end.max(m.ts))
}

/// What a mark keeps out of the idle breaks: its span, or a point with some room around it,
/// so every pause and skip stays visible on the uncollapsed axis (plan §15.2).
fn mark_busy(m: &TimelineMark, open_end: f64) -> (f64, f64) {
    let (a, b) = mark_extent(m, open_end);
    if is_point(m) {
        (a - POINT_PAD, b + POINT_PAD)
    } else {
        (a, b)
    }
}

// ------------------------------------------------------------------ rows

struct RowAcc {
    slot: String,
    person: String,
    /// Sessions of the row, roots and nested ones.
    sessions: Vec<usize>,
    /// Newest root session (labels and row metadata).
    newest: usize,
    first: f64,
    blocks: Vec<Block>,
    marks: Vec<TimelineMark>,
}

/// `ended_at` is where the timeline draws the session to: a non-live session the index never
/// closed (`degraded`, no `ended_at`) ends after its indexed length, as its open commands do.
fn timeline_session(s: &Sess) -> TimelineSession {
    let x = &s.row.session;
    TimelineSession {
        uuid: x.uuid.clone(),
        user: x.user.clone(),
        state: x.state,
        started_at: x.started_at,
        ended_at: x.ended_at.or((!s.live()).then_some(s.end)),
        parent_uuid: x.parent_uuid.clone(),
        legacy: x.legacy,
    }
}

/// Non-empty density bins of `blocks` (`[from + i·bin, from + (i+1)·bin)`), ascending.
fn density(blocks: &[Block], w: Window, bin: f64) -> Vec<DensityBin> {
    let mut bins: BTreeMap<u64, (u32, u32)> = BTreeMap::new();
    for b in blocks {
        let i = ((b.block.ts_start - w.from) / bin).floor().max(0.0) as u64;
        let e = bins.entry(i).or_default();
        e.0 += 1;
        e.1 += u32::from(b.failed);
    }
    bins.into_iter()
        .map(|(i, (commands, failed))| DensityBin {
            ts: w.from + i as f64 * bin,
            commands,
            failed,
        })
        .collect()
}

// ------------------------------------------------------------------ build

fn build(st: &AppState, p: &TimelineParams, now: f64) -> Result<Timeline> {
    let w = Window::of(p, now)?;
    let group = p.group.unwrap_or_default();
    let ctx = &st.ctx;
    let db = IndexDb::open(ctx)?;
    let forgets = Forgets::load(ctx)?;
    let snap = db.snapshot()?;
    let filter = SessionFilter {
        active_from: Some(w.from),
        active_to: Some(w.to),
        host: p.host.clone(),
        include_hidden: p.hidden.unwrap_or(false),
        exclude: forgets.sessions(),
        ..SessionFilter::default()
    };
    let mut found: Vec<Sess> = db
        .sessions(&filter, None, None)?
        .into_iter()
        .map(|r| Sess::new(r, now))
        .filter(|s| s.end >= w.from)
        .collect();
    found.sort_by(|a, b| {
        let (a, b) = (&a.row.session, &b.row.session);
        a.started_at.total_cmp(&b.started_at).then_with(|| a.uuid.cmp(&b.uuid))
    });
    let by_uuid: HashMap<String, usize> = found
        .iter()
        .enumerate()
        .map(|(i, s)| (s.uuid().to_owned(), i))
        .collect();
    for i in 0..found.len() {
        found[i].root = root_of(&found, &by_uuid, i);
    }
    let wanted = |s: &Sess| {
        p.user
            .as_deref()
            .is_none_or(|u| person(&found[s.root].row.session) == u)
    };
    let keep: Vec<usize> = (0..found.len()).filter(|&i| wanted(&found[i])).collect();

    // Rows by (slot, person) of the root session.
    let mut rows: Vec<RowAcc> = Vec::new();
    let mut row_of_root: HashMap<usize, usize> = HashMap::new();
    let mut row_index: HashMap<(String, String), usize> = HashMap::new();
    let mut row_of_sess: HashMap<usize, usize> = HashMap::new();
    for &i in &keep {
        let root = found[i].root;
        let r = *row_of_root.entry(root).or_insert_with(|| {
            let s = &found[root].row.session;
            let key = (slot(s).0, person(s).to_owned());
            *row_index.entry(key.clone()).or_insert_with(|| {
                rows.push(RowAcc {
                    slot: key.0,
                    person: key.1,
                    sessions: Vec::new(),
                    newest: root,
                    first: f64::INFINITY,
                    blocks: Vec::new(),
                    marks: Vec::new(),
                });
                rows.len() - 1
            })
        });
        let acc = &mut rows[r];
        acc.sessions.push(i);
        acc.first = acc.first.min(found[i].row.session.started_at.max(w.from));
        if i == root && found[root].row.session.started_at >= found[acc.newest].row.session.started_at {
            acc.newest = root;
        }
        row_of_sess.insert(i, r);
    }

    // Commands and marks of every kept session.
    let mut total: u64 = 0;
    // What keeps a stretch out of the idle breaks: blocks and marks of every row.
    let mut busy: Vec<(f64, f64)> = Vec::new();
    let mut known: HashMap<(usize, u64), (f64, Option<f64>)> = HashMap::new();
    for &i in &keep {
        let list = markers(&snap, found[i].row.id)?;
        add_clock(&mut found[i], &list);
        let s = &found[i];
        let uuid = s.uuid().to_owned();
        let forgot = Forgot::of(&forgets, &uuid);
        let blocks = commands(&snap, &found, i, &forgot, w)?;
        let mut marks = marker_marks(s, &list, &forgets);
        if !forgets.rows(&uuid).is_empty() {
            let mask = crate::play::session::mask_of(ctx, &st.play.markers, &db, &forgets, &s.row)?;
            marks.extend(forgotten_marks(s, &mask));
        }
        let open_end = s.end;
        let acc = &mut rows[row_of_sess[&i]];
        for m in marks {
            let (a, b) = mark_extent(&m, open_end);
            if w.overlaps(a, b) {
                busy.push(mark_busy(&m, open_end));
                acc.marks.push(m);
            }
        }
        total += blocks.len() as u64;
        for b in &blocks {
            known.insert((i, b.block.seq), (b.block.ts_start, b.block.ts_end));
            busy.push((b.block.ts_start, b.end));
        }
        acc.blocks.extend(blocks);
    }

    let idle = idle_spans(busy, w);
    let aggregated = total > AGGREGATE_ABOVE;
    let bin = aggregated.then(|| w.bin_secs());

    // Final row keys: in group `user` a slot shared by several people gets the person too.
    let mut people_of_slot: HashMap<String, HashSet<&str>> = HashMap::new();
    for r in &rows {
        people_of_slot
            .entry(r.slot.clone())
            .or_default()
            .insert(person(&found[r.newest].row.session));
    }
    let mut slot_label: HashMap<String, (f64, String)> = HashMap::new();
    for r in &rows {
        let s = &found[r.newest].row.session;
        let e = slot_label
            .entry(r.slot.clone())
            .or_insert((f64::NEG_INFINITY, String::new()));
        if s.started_at >= e.0 {
            *e = (s.started_at, slot(s).1);
        }
    }
    let row_keys: Vec<String> = rows
        .iter()
        .map(|r| match group {
            TimelineGroup::User if people_of_slot[r.slot.as_str()].len() == 1 => r.slot.clone(),
            _ => format!("{}|user:{}", r.slot, r.person),
        })
        .collect();

    let annotations = annotations(st, &db, &found, &keep, &row_of_sess, &row_keys, &known, &forgets)?;
    drop(snap);

    let mut lanes: BTreeMap<String, TimelineLane> = BTreeMap::new();
    let mut ordered: Vec<(usize, RowAcc)> = rows.into_iter().enumerate().collect();
    ordered.sort_by(|(ka, a), (kb, b)| {
        a.first
            .total_cmp(&b.first)
            .then_with(|| row_keys[*ka].cmp(&row_keys[*kb]))
    });
    for (k, mut r) in ordered {
        let (lane_key, lane_label, row_label) = match group {
            TimelineGroup::User => (
                format!("user:{}", r.person),
                r.person.clone(),
                slot_label[r.slot.as_str()].1.clone(),
            ),
            TimelineGroup::Pane => (r.slot.clone(), slot_label[r.slot.as_str()].1.clone(), r.person.clone()),
        };
        r.blocks.sort_by(|a, b| {
            a.block
                .ts_start
                .total_cmp(&b.block.ts_start)
                .then_with(|| a.block.seq.cmp(&b.block.seq))
        });
        r.marks.sort_by(|a, b| a.ts.total_cmp(&b.ts));
        r.sessions.sort_by(|&a, &b| {
            let (a, b) = (&found[a].row.session, &found[b].row.session);
            a.started_at.total_cmp(&b.started_at).then_with(|| a.uuid.cmp(&b.uuid))
        });
        let newest = &found[r.newest].row.session;
        let density = bin.map_or_else(Vec::new, |bin| density(&r.blocks, w, bin));
        let row = TimelineRow {
            key: row_keys[k].clone(),
            label: row_label,
            host: newest.host.clone(),
            user: r.person,
            tmux: newest.tmux.clone(),
            holder_comm: newest.holder_comm.clone(),
            tty: newest.tty.clone(),
            sessions: r.sessions.iter().map(|&i| timeline_session(&found[i])).collect(),
            blocks: if aggregated {
                Vec::new()
            } else {
                r.blocks.into_iter().map(|b| b.block).collect()
            },
            marks: r.marks,
            density,
        };
        lanes
            .entry(format!("{lane_label}\u{0}{lane_key}"))
            .or_insert_with(|| TimelineLane {
                key: lane_key,
                label: lane_label,
                rows: Vec::new(),
            })
            .rows
            .push(row);
    }

    Ok(Timeline {
        from: w.from,
        to: w.to,
        group,
        lanes: lanes.into_values().collect(),
        idle,
        annotations,
        commands: total,
        aggregated,
        bin_secs: bin,
    })
}

/// The non-deleted annotations of the drawn sessions in wall time, by `ts_from`.
#[allow(clippy::too_many_arguments)]
fn annotations(
    st: &AppState,
    db: &IndexDb,
    found: &[Sess],
    keep: &[usize],
    row_of_sess: &HashMap<usize, usize>,
    row_keys: &[String],
    known: &HashMap<(usize, u64), (f64, Option<f64>)>,
    forgets: &Forgets,
) -> Result<Vec<TimelineAnnotation>> {
    if keep.is_empty() || !st.ctx.db.meta.exists() {
        return Ok(Vec::new());
    }
    let meta = cinelog_index::db::open_meta(&st.ctx.db)?;
    let by_uuid: HashMap<&str, usize> = keep.iter().map(|&i| (found[i].uuid(), i)).collect();
    let list = meta.find_annotations(&AnnotationQuery {
        sessions: by_uuid.keys().map(|u| (*u).to_owned()).collect(),
        ..AnnotationQuery::default()
    })?;
    if list.is_empty() {
        return Ok(Vec::new());
    }
    let colors: HashMap<String, String> = meta
        .tags()?
        .into_iter()
        .filter_map(|t| t.color.map(|c| (t.name, c)))
        .collect();
    let mut out = Vec::new();
    for a in list {
        let Some(&i) = by_uuid.get(a.session_uuid.as_str()) else {
            continue;
        };
        let s = &found[i];
        let span = match (a.cmd_seq, a.t_from, a.t_to) {
            (Some(seq), _, _) => match known.get(&(i, seq)) {
                Some(&(start, end)) => Some((start, end.unwrap_or(start))),
                None => db
                    .command(s.row.id, seq)?
                    .filter(|c| !Forgot::of(forgets, s.uuid()).command(c.seq, c.t_start, c.t_end))
                    .map(|c| (c.ts_start, c.ts_end.unwrap_or(c.ts_start))),
            },
            (None, Some(from), Some(to)) => Some((s.ts(from), s.ts(to.max(from)))),
            _ => None,
        };
        let Some((ts_from, ts_to)) = span else { continue };
        out.push(TimelineAnnotation {
            row: row_keys[row_of_sess[&i]].clone(),
            session: a.session_uuid,
            cmd_seq: a.cmd_seq,
            ts_from,
            ts_to: ts_to.max(ts_from),
            color: a.tags.iter().find_map(|t| colors.get(t).cloned()),
            tags: a.tags,
            author: a.author,
            uuid: a.uuid,
        });
    }
    out.sort_by(|a, b| a.ts_from.total_cmp(&b.ts_from).then_with(|| a.uuid.cmp(&b.uuid)));
    Ok(out)
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use axum::http::StatusCode;
    use cinelog_index::model::ForgetTarget;
    use serde_json::Value;

    use super::*;
    use crate::api::testkit::{Cast, Kit, Meta, START_US, uuid};

    const START: f64 = START_US as f64 / 1e6;

    fn params(from: Option<f64>, to: Option<f64>) -> TimelineParams {
        TimelineParams {
            from,
            to,
            ..TimelineParams::default()
        }
    }

    #[test]
    fn windows_default_and_are_validated() {
        let now = 1_000_000.0;
        assert_eq!(
            Window::of(&params(None, None), now).unwrap(),
            Window {
                from: now - DAY,
                to: now
            }
        );
        assert_eq!(
            Window::of(&params(Some(10.0), None), now).unwrap(),
            Window {
                from: 10.0,
                to: 10.0 + DAY
            }
        );
        assert_eq!(
            Window::of(&params(None, Some(5e5)), now).unwrap(),
            Window {
                from: 5e5 - DAY,
                to: 5e5
            }
        );
        for (from, to) in [
            (5.0, 5.0),
            (6.0, 5.0),
            (0.0, MAX_WINDOW + 1.0),
            (f64::NAN, 1.0),
            (0.0, f64::INFINITY),
        ] {
            assert!(Window::of(&params(Some(from), Some(to)), now).is_err(), "{from} {to}");
        }
        assert!(Window::of(&params(Some(0.0), Some(MAX_WINDOW)), now).is_ok());
    }

    #[test]
    fn bins_fit_the_window() {
        let w = |len: f64| Window { from: 0.0, to: len };
        assert_eq!(w(3600.0).bin_secs(), 60.0);
        assert_eq!(w(2000.0 * 60.0).bin_secs(), 60.0);
        assert_eq!(w(2000.0 * 60.0 + 1.0).bin_secs(), 300.0);
        assert_eq!(w(7.0 * DAY).bin_secs(), 900.0);
        assert_eq!(w(MAX_WINDOW).bin_secs(), 1800.0);
        for len in [1.0, 3600.0, DAY, 7.0 * DAY, MAX_WINDOW] {
            assert!((len / w(len).bin_secs()).ceil() <= MAX_BINS);
        }
    }

    #[test]
    fn idle_breaks_need_ten_minutes_without_any_block() {
        let w = Window {
            from: 0.0,
            to: 10_000.0,
        };
        let idle = idle_spans(
            vec![(1000.0, 1200.0), (100.0, 900.0), (1500.0, 1600.0), (9000.0, 20_000.0)],
            w,
        );
        let spans: Vec<(f64, f64)> = idle.iter().map(|s| (s.from, s.to)).collect();
        assert_eq!(spans, [(1600.0, 9000.0)], "gaps of 100 s and 300 s are no breaks");
        let all = idle_spans(Vec::new(), w);
        assert_eq!((all[0].from, all[0].to), (0.0, 10_000.0));
        assert!(idle_spans(vec![(-50.0, 20_000.0)], w).is_empty());
        let edges = idle_spans(vec![(700.0, 800.0)], w);
        assert_eq!(
            edges.iter().map(|s| (s.from, s.to)).collect::<Vec<_>>(),
            [(0.0, 700.0), (800.0, 10_000.0)]
        );
    }

    #[test]
    fn marks_are_never_inside_an_idle_break() {
        let w = Window {
            from: 0.0,
            to: 10_000.0,
        };
        let mut pause = mark("s", TimelineMarkKind::Pause, 1000.0, Some(4000.0));
        let skip = mark("s", TimelineMarkKind::Skip, 6000.0, None);
        let busy = vec![(0.0, 10.0), mark_busy(&pause, 9000.0), mark_busy(&skip, 9000.0)];
        let spans: Vec<(f64, f64)> = idle_spans(busy, w).iter().map(|s| (s.from, s.to)).collect();
        assert_eq!(
            spans,
            [
                (10.0, 1000.0),
                (4000.0, 6000.0 - POINT_PAD),
                (6000.0 + POINT_PAD, 10_000.0)
            ]
        );
        // An open pause lasts until the session's end.
        pause.ts_end = None;
        assert_eq!(mark_busy(&pause, 9000.0), (1000.0, 9000.0));
        assert_eq!(mark_extent(&skip, 9000.0), (6000.0, 6000.0));
    }

    fn session(over: impl FnOnce(&mut Session)) -> Session {
        let mut s = Session {
            uuid: uuid(9),
            host: "h1".into(),
            scope: crate::api::types::Scope::Team,
            user: "root".into(),
            euid: Some(0),
            sudo_user: None,
            claimed_user: None,
            attribution: None,
            login_via: None,
            tty: Some("/dev/pts/4".into()),
            holder_comm: Some("sshd-session".into()),
            tmux: Default::default(),
            parent_uuid: None,
            parts: 1,
            compressed: false,
            cast_bytes: None,
            started_at: 0.0,
            ended_at: None,
            duration: None,
            exit_status: None,
            tz: None,
            cols: 80,
            rows: 24,
            term: None,
            legacy: false,
            hidden: false,
            state: SessionState::Ended,
            commands: 0,
        };
        over(&mut s);
        s
    }

    #[test]
    fn people_and_slots() {
        let root = session(|_| {});
        assert_eq!(person(&root), "root");
        assert_eq!(person(&session(|s| s.claimed_user = Some("carol".into()))), "carol");
        let sudo = session(|s| {
            s.claimed_user = Some("carol".into());
            s.sudo_user = Some("alice".into());
        });
        assert_eq!(person(&sudo), "alice");
        assert_eq!(person(&session(|s| s.sudo_user = Some(String::new()))), "root");

        assert_eq!(slot(&root), ("tty:h1//dev/pts/4".into(), "sshd-session pts/4".into()));
        let bare = session(|s| {
            s.tty = None;
            s.holder_comm = None;
        });
        assert_eq!(slot(&bare), (format!("session:{}", uuid(9)), "shell 0193a1b2".into()));
        let pane = session(|s| {
            s.tmux.server = Some("42:7".into());
            s.tmux.session = Some("work".into());
            s.tmux.window = Some("2".into());
            s.tmux.pane = Some("%5".into());
        });
        assert_eq!(slot(&pane), ("pane:h1/42:7/%5".into(), "work:2.%5".into()));
        let anon = session(|s| s.tmux.pane = Some("%5".into()));
        assert_eq!(slot(&anon), ("pane:h1/-/%5".into(), "%5".into()));
    }

    #[test]
    fn typed_is_one_short_line() {
        assert_eq!(first_line("ls -la"), "ls -la");
        assert_eq!(first_line("for x in a b\r\ndo echo $x\ndone"), "for x in a b");
        let long = "é".repeat(300);
        assert_eq!(first_line(&long).chars().count(), TYPED_MAX);
    }

    #[test]
    fn forgotten_commands_follow_the_index_rule() {
        let f = Forgot {
            seqs: HashSet::from([2]),
            ranges: vec![(10.0, 20.0)],
        };
        assert!(f.command(2, 0.0, Some(1.0)));
        assert!(f.command(3, 11.0, Some(19.0)));
        assert!(!f.command(4, 11.0, Some(21.0)));
        assert!(!f.command(5, 11.0, None));
    }

    fn m(kind: &str, fields: &str) -> String {
        format!(r#"{{"cl":1,"k":"{kind}"{fields}}}"#)
    }

    /// Session 1 (`u`, pane %3) with every mark kind and a clock step of +100 s at t=20,
    /// session 3 nested in it (sudo), session 2 (`bob`, terminator, no tmux).
    fn fixture(kit: &Kit) {
        let dir = kit.dir.path();
        let mut a = Cast::new(dir, 1, 80, 24, &Meta::default());
        a.command(1.0, 1, "ls -la", &["total 0"], 0);
        a.m(5.0, &m("pause", r#","reason":"user","detail":"cinelog pause""#));
        a.m(8.0, &m("resume", r#","reason":"user""#));
        a.m(9.0, &m("skip", r#","sh":100"#));
        let child = uuid(3);
        a.m(
            10.0,
            &m(
                "nested",
                &format!(
                    r#","child_uuid":"{child}","scope":"personal","user":"root","url":"https://h:10000/s/{child}","mode":"record""#
                ),
            ),
        );
        a.m(12.0, &m("gap", r#","dropped_bytes":100,"from_t":11.0,"to_t":12.0"#));
        a.command(13.0, 2, "printf 'a\nb'", &["a", "b"], 1);
        let step = START_US + 1_000_000 + 120_000_000;
        a.m(
            20.0,
            &m(
                "clock",
                &format!(r#","epoch_us":{step},"boottime_us":0,"reason":"step""#),
            ),
        );
        let t = a.command(30.0, 3, "false", &[], 1);
        a.exit(t + 0.5);

        let parent = uuid(1);
        let mut c = Cast::new(
            dir,
            3,
            80,
            24,
            &Meta {
                pane: "",
                parent: Some(&parent),
                holder: Some("sudo"),
                ..Meta::default()
            },
        );
        let t = c.command(0.5, 1, "id", &["uid=0(root)"], 0);
        c.exit(t + 0.5);

        let mut b = Cast::new(
            dir,
            2,
            100,
            30,
            &Meta {
                pane: "",
                holder: Some("terminator"),
                user: "bob",
                ..Meta::default()
            },
        );
        let t = b.command(0.5, 1, "docker ps", &["CONTAINER ID"], 0);
        b.exit(t + 0.5);
        for p in [&a.path, &b.path, &c.path] {
            kit.index(p, None);
        }
    }

    fn window_uri(extra: &str) -> String {
        format!("/timeline?from={}&to={}{extra}", START - 60.0, START + 3600.0)
    }

    fn close(v: &Value, want: f64) -> bool {
        v.as_f64().is_some_and(|x| (x - want).abs() < 1e-3)
    }

    #[tokio::test]
    async fn lanes_rows_blocks_and_marks() {
        let kit = Kit::new(8);
        fixture(&kit);
        let tl = kit.json(&window_uri("")).await;
        assert_eq!(tl["group"], "user");
        assert_eq!(tl["aggregated"], false);
        assert_eq!(tl["bin_secs"], Value::Null);
        assert_eq!(tl["commands"], 5);
        let lanes = tl["lanes"].as_array().unwrap();
        let keys: Vec<&str> = lanes.iter().map(|l| l["key"].as_str().unwrap()).collect();
        assert_eq!(keys, ["user:bob", "user:u"], "by label");

        let bob = &lanes[0]["rows"][0];
        assert_eq!(bob["key"], "tty:testhost//dev/pts/3");
        assert_eq!(bob["label"], "terminator pts/3");
        assert_eq!(bob["user"], "bob");

        let rows = lanes[1]["rows"].as_array().unwrap();
        assert_eq!(rows.len(), 1, "the nested session is drawn in its parent's row");
        let row = &rows[0];
        let key = row["key"].as_str().unwrap();
        assert!(key.starts_with("pane:testhost/") && key.ends_with("/%3"), "{key}");
        assert_eq!(row["user"], "u");
        assert_eq!(row["tmux"]["pane"], "%3");
        let sessions: Vec<&str> = row["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s["uuid"].as_str().unwrap())
            .collect();
        assert_eq!(sessions, [uuid(1), uuid(3)]);
        assert_eq!(row["sessions"][1]["parent_uuid"], uuid(1));

        let blocks = row["blocks"].as_array().unwrap();
        let ids: Vec<(String, u64)> = blocks
            .iter()
            .map(|b| (b["session"].as_str().unwrap().to_owned(), b["seq"].as_u64().unwrap()))
            .collect();
        assert_eq!(
            ids,
            [(uuid(1), 1), (uuid(3), 1), (uuid(1), 2), (uuid(1), 3)],
            "by ts_start"
        );
        assert_eq!(blocks[0]["typed"], "ls -la");
        assert_eq!(blocks[0]["exit"], 0);
        assert_eq!(blocks[0]["cwd"], "/tmp");
        assert!(close(&blocks[0]["ts_start"], START + 1.0 + 1.01));
        assert_eq!(blocks[2]["typed"], "printf 'a", "first line only");
        assert_eq!(blocks[2]["exit"], 1);
        assert!(
            close(&blocks[3]["ts_start"], START + 1.0 + 30.01 + 100.0),
            "clock step applied"
        );

        let marks = row["marks"].as_array().unwrap();
        let kinds: Vec<&str> = marks.iter().map(|m| m["kind"].as_str().unwrap()).collect();
        assert_eq!(kinds, ["pause", "skip", "nested", "gap"]);
        assert!(close(&marks[0]["ts"], START + 1.0 + 5.0));
        assert!(close(&marks[0]["ts_end"], START + 1.0 + 8.0));
        assert_eq!(marks[0]["reason"], "user");
        assert_eq!(marks[0]["detail"], "cinelog pause");
        assert!(close(&marks[1]["ts"], START + 10.0));
        assert_eq!(marks[1]["ts_end"], Value::Null);
        assert_eq!(marks[2]["child"], uuid(3));
        assert_eq!(marks[2]["detail"], "root");
        assert_eq!(marks[2]["child_scope"], "personal");
        assert_eq!(marks[2]["child_url"], format!("https://h:10000/s/{}", uuid(3)));
        assert_eq!(marks[0]["child_scope"], Value::Null, "only nested marks name a child");
        assert!(close(&marks[3]["ts"], START + 1.0 + 11.0));
        assert!(close(&marks[3]["ts_end"], START + 1.0 + 12.0));
        assert_eq!(marks[3]["detail"], "100 bytes dropped");

        // Everything ran within a few minutes: one break from the last command to `to`.
        let idle = tl["idle"].as_array().unwrap();
        assert_eq!(idle.len(), 1);
        assert!(close(&idle[0]["to"], START + 3600.0));
        assert!(idle[0]["from"].as_f64().unwrap() < START + 200.0);
    }

    #[tokio::test]
    async fn pauses_and_skips_without_commands_are_not_idle() {
        let kit = Kit::new(8);
        let mut a = Cast::new(kit.dir.path(), 1, 80, 24, &Meta::default());
        a.command(1.0, 1, "ls", &[], 0);
        a.m(100.0, &m("pause", r#","reason":"user","detail":"cinelog pause""#));
        a.m(4000.0, &m("resume", r#","reason":"user""#));
        a.m(6000.0, &m("skip", r#","sh":100"#));
        let t = a.command(9000.0, 2, "true", &[], 0);
        a.exit(t + 0.5);
        kit.index(&a.path, None);
        let tl = kit
            .json(&format!("/timeline?from={}&to={}", START, START + 12_000.0))
            .await;
        let row = &tl["lanes"][0]["rows"][0];
        let kinds: Vec<&str> = row["marks"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["kind"].as_str().unwrap())
            .collect();
        assert_eq!(kinds, ["pause", "skip"]);
        let idle: Vec<(f64, f64)> = tl["idle"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| (s["from"].as_f64().unwrap(), s["to"].as_f64().unwrap()))
            .collect();
        let (pause_from, pause_to) = (START + 1.0 + 100.0, START + 1.0 + 4000.0);
        let skip = START + 1.0 + 6000.0;
        assert_eq!(idle.len(), 3, "{idle:?}");
        for &(a, b) in &idle {
            assert!(b <= pause_from + 1e-3 || a >= pause_to - 1e-3, "pause inside {a}..{b}");
            assert!(
                b < skip - POINT_PAD / 2.0 || a > skip + POINT_PAD / 2.0,
                "skip inside {a}..{b}"
            );
        }
        assert!(close(&Value::from(idle[0].0), pause_to), "{idle:?}");
    }

    #[tokio::test]
    async fn a_never_closed_session_is_drawn_to_its_indexed_end() {
        let kit = Kit::new(8);
        fixture(&kit);
        drop(kit.writer());
        let conn = Connection::open(&kit.ctx().db.index).unwrap();
        conn.execute(
            "UPDATE sessions SET state = 'degraded', ended_at = NULL WHERE uuid = ?1",
            [uuid(1)],
        )
        .unwrap();
        conn.execute(
            "UPDATE commands SET t_end = NULL, ts_end = NULL, exit = NULL
             WHERE seq = 3 AND session_id = (SELECT id FROM sessions WHERE uuid = ?1)",
            [uuid(1)],
        )
        .unwrap();
        drop(conn);
        let tl = kit.json(&window_uri("")).await;
        let row = &tl["lanes"][1]["rows"][0];
        let s = &row["sessions"][0];
        assert_eq!(s["uuid"], uuid(1));
        assert_eq!(s["state"], "degraded");
        let end = s["ended_at"].as_f64().expect("the drawn end is sent");
        let open = &row["blocks"][3];
        assert_eq!(open["ts_end"], Value::Null);
        let start = open["ts_start"].as_f64().unwrap();
        assert!(end > start, "{end} after {start}");
        // The break starts where the client draws the open command to.
        assert!(close(&tl["idle"][0]["from"], end), "{} vs {end}", tl["idle"][0]["from"]);
        // Sessions that did end keep their own `ended_at`; nothing is invented for live ones.
        assert!(row["sessions"][1]["ended_at"].is_f64());
    }

    #[tokio::test]
    async fn pane_grouping_and_filters() {
        let kit = Kit::new(8);
        fixture(&kit);
        let tl = kit.json(&window_uri("&group=pane")).await;
        assert_eq!(tl["group"], "pane");
        let lanes = tl["lanes"].as_array().unwrap();
        assert_eq!(lanes.len(), 2);
        let pane = lanes
            .iter()
            .find(|l| l["key"].as_str().unwrap().starts_with("pane:"))
            .unwrap();
        assert_eq!(pane["label"], "%3");
        let row = &pane["rows"][0];
        assert_eq!(row["key"], format!("{}|user:u", pane["key"].as_str().unwrap()));
        assert_eq!(row["label"], "u");
        assert_eq!(row["blocks"].as_array().unwrap().len(), 4);

        let only_bob = kit.json(&window_uri("&user=bob")).await;
        let keys: Vec<&str> = only_bob["lanes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|l| l["key"].as_str().unwrap())
            .collect();
        assert_eq!(keys, ["user:bob"]);
        assert_eq!(only_bob["commands"], 1);
        let other_host = kit.json(&window_uri("&host=elsewhere")).await;
        assert_eq!(other_host["lanes"], Value::Array(vec![]));

        // A window long after the fixture: nothing but one break.
        let later = kit
            .json(&format!("/timeline?from={}&to={}", START + DAY, START + 2.0 * DAY))
            .await;
        assert_eq!(later["lanes"], Value::Array(vec![]));
        assert_eq!(later["commands"], 0);
        assert_eq!(later["idle"].as_array().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn bad_windows_and_a_missing_index() {
        let kit = Kit::new(8);
        let (status, _, _) = kit.get("/timeline").await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        fixture(&kit);
        for q in [
            "from=10&to=10",
            "from=20&to=10",
            "from=0&to=2678401",
            "from=inf",
            "from=x",
            "group=lane",
        ] {
            let (status, _, body) = kit.get(&format!("/timeline?{q}")).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{q}");
            let err: Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(err["error"], "bad_request");
        }
        kit.json("/timeline").await;
    }

    #[tokio::test]
    async fn forgotten_commands_and_sessions() {
        let kit = Kit::new(8);
        fixture(&kit);
        kit.cli_forget(ForgetTarget::Command { uuid: uuid(1), seq: 2 });
        kit.cli_forget(ForgetTarget::Session { uuid: uuid(2) });
        for purged in [false, true] {
            if purged {
                kit.purge();
            }
            let tl = kit.json(&window_uri("")).await;
            let lanes = tl["lanes"].as_array().unwrap();
            assert_eq!(lanes.len(), 1, "bob's only session is forgotten");
            let row = &lanes[0]["rows"][0];
            let seqs: Vec<(String, u64)> = row["blocks"]
                .as_array()
                .unwrap()
                .iter()
                .map(|b| (b["session"].as_str().unwrap().to_owned(), b["seq"].as_u64().unwrap()))
                .collect();
            assert_eq!(seqs, [(uuid(1), 1), (uuid(3), 1), (uuid(1), 3)], "purged={purged}");
            let forgotten: Vec<&Value> = row["marks"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|m| m["kind"] == "forgotten")
                .collect();
            assert_eq!(forgotten.len(), 1, "purged={purged}");
            assert_eq!(forgotten[0]["detail"], "cli");
            // From the end of the command before it to its own end.
            assert!(forgotten[0]["ts"].as_f64().unwrap() < START + 1.0 + 13.0);
            assert!(forgotten[0]["ts_end"].as_f64().unwrap() > START + 1.0 + 13.0);
            assert_eq!(tl["commands"], 3);
        }
    }

    fn annotation(
        n: u32,
        session: &str,
        cmd_seq: Option<u64>,
        span: Option<(f64, f64)>,
        tags: &[&str],
    ) -> im::Annotation {
        im::Annotation {
            uuid: format!("0193a1b2-0000-7000-9000-{n:012}"),
            session_uuid: session.into(),
            cmd_seq,
            t_from: span.map(|s| s.0),
            t_to: span.map(|s| s.1),
            author: "alice".into(),
            body: "note".into(),
            tags: tags.iter().map(|t| (*t).to_owned()).collect(),
            created_at: f64::from(n),
            updated_at: f64::from(n),
            deleted_at: None,
        }
    }

    #[tokio::test]
    async fn annotations_in_wall_time() {
        let kit = Kit::new(8);
        fixture(&kit);
        let mut meta = cinelog_index::db::open_meta(&kit.ctx().db).unwrap();
        meta.put_tag("incident", Some("#ff0000")).unwrap();
        meta.put_annotation(&annotation(
            1,
            &uuid(1),
            None,
            Some((25.0, 35.0)),
            &["auto", "incident"],
        ))
        .unwrap();
        meta.put_annotation(&annotation(2, &uuid(2), Some(1), None, &[]))
            .unwrap();
        meta.put_annotation(&annotation(3, &uuid(3), Some(1), None, &["auto"]))
            .unwrap();
        let mut deleted = annotation(4, &uuid(1), Some(1), None, &[]);
        deleted.deleted_at = Some(5.0);
        meta.put_annotation(&deleted).unwrap();
        meta.put_annotation(&annotation(5, &uuid(1), Some(99), None, &[]))
            .unwrap();
        drop(meta);

        let tl = kit.json(&window_uri("")).await;
        let list = tl["annotations"].as_array().unwrap();
        let row_u = tl["lanes"][1]["rows"][0]["key"].as_str().unwrap();
        let ids: Vec<&str> = list.iter().map(|a| a["uuid"].as_str().unwrap()).collect();
        assert_eq!(
            ids,
            [
                "0193a1b2-0000-7000-9000-000000000002",
                "0193a1b2-0000-7000-9000-000000000003",
                "0193a1b2-0000-7000-9000-000000000001"
            ],
            "by ts_from; deleted ones and unknown commands left out"
        );
        assert_eq!(list[0]["row"], "tty:testhost//dev/pts/3");
        let nested = &list[1];
        assert_eq!(nested["row"], row_u, "the nested session's row is its parent's");
        assert_eq!(nested["cmd_seq"], 1);
        assert_eq!(nested["color"], Value::Null);
        assert!(nested["ts_to"].as_f64().unwrap() > nested["ts_from"].as_f64().unwrap());
        let span = &list[2];
        assert_eq!(span["row"], row_u);
        assert_eq!(span["cmd_seq"], Value::Null);
        assert_eq!(span["color"], "#ff0000", "first tag with a colour");
        assert_eq!(span["tags"], serde_json::json!(["auto", "incident"]));
        assert_eq!(span["author"], "alice");
        assert!(
            close(&span["ts_from"], START + 1.0 + 25.0 + 100.0),
            "{}",
            span["ts_from"]
        );
        assert!(close(&span["ts_to"], START + 1.0 + 35.0 + 100.0));

        // Only the drawn people's annotations.
        let bob = kit.json(&window_uri("&user=bob")).await;
        assert_eq!(bob["annotations"].as_array().unwrap().len(), 1);
    }

    /// Writes `sessions` sessions with `per` commands each, spread over `days` days ending at
    /// `end`, straight into index.db (the cast pipeline would take minutes for this).
    fn synthetic(kit: &Kit, people: u32, panes: u32, days: u32, per: u32, end: f64) -> u64 {
        drop(kit.writer());
        let mut conn = Connection::open(&kit.ctx().db.index).unwrap();
        let tx = conn.transaction().unwrap();
        let mut total = 0;
        let mut n = 0;
        for day in 0..days {
            for p in 0..people {
                for pane in 0..panes {
                    n += 1;
                    let start = end - f64::from(days - day) * DAY + f64::from(pane) * 600.0;
                    let len = 8.0 * 3600.0;
                    tx.execute(
                        "INSERT INTO sessions(uuid, host, scope, user, sudo_user, tty, holder_comm, tmux_server,
                             tmux_session, tmux_window, tmux_pane, cast_path, started_at, ended_at, state)
                         VALUES(?1, 'h', 'team', 'root', ?2, '/dev/pts/1', 'tmux: server', '1:2', 'work', '0', ?3,
                             ?4, ?5, ?6, 'ended')",
                        params![
                            uuid(n),
                            format!("user{p}"),
                            format!("%{}", p * panes + pane),
                            format!("/c/{n}.cast"),
                            start,
                            start + len
                        ],
                    )
                    .unwrap();
                    let id = tx.last_insert_rowid();
                    for c in 0..per {
                        let t = f64::from(c) * len / f64::from(per);
                        tx.execute(
                            "INSERT INTO commands(session_id, seq, t_start, t_end, ts_start, ts_end, exit, cwd, typed)
                             VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, '/srv', ?8)",
                            params![
                                id,
                                c + 1,
                                t,
                                t + 20.0,
                                start + t,
                                start + t + 20.0,
                                i32::from(c % 7 == 0),
                                format!("make target-{c} && echo done")
                            ],
                        )
                        .unwrap();
                        total += 1;
                    }
                    for k in 0..(len / 300.0) as u32 {
                        let t = f64::from(k) * 300.0;
                        let epoch = ((start + t) * 1e6) as i64;
                        tx.execute(
                            "INSERT INTO markers(session_id, t, ts, kind, data_json) VALUES(?1, ?2, ?3, 'clock', ?4)",
                            params![
                                id,
                                t,
                                start + t,
                                format!(r#"{{"cl":1,"k":"clock","epoch_us":{epoch}}}"#)
                            ],
                        )
                        .unwrap();
                    }
                    tx.execute(
                        "INSERT INTO markers(session_id, t, ts, kind, data_json) VALUES(?1, 60, ?2, 'skip', '{}')",
                        params![id, start + 60.0],
                    )
                    .unwrap();
                }
            }
        }
        tx.commit().unwrap();
        total
    }

    /// Plan §13 M4 Accept 2: one week of team data in under 500 ms (release; debug builds
    /// get a generous bound). 10 people with 3 panes a day for 7 days, 19 950 commands, just
    /// below the aggregation threshold, which is the expensive case.
    #[tokio::test]
    async fn a_week_of_team_data_is_fast() {
        let kit = Kit::new(8);
        let end = START + 7.0 * DAY;
        let total = synthetic(&kit, 10, 3, 7, 95, end);
        assert_eq!(total, 19_950);
        let uri = format!("/timeline?from={}&to={end}", end - 7.0 * DAY);
        kit.json(&uri).await;
        let t0 = Instant::now();
        let tl = kit.json(&uri).await;
        let took = t0.elapsed();
        eprintln!("timeline of {total} commands: {took:?}");
        let budget = if cfg!(debug_assertions) { 5000 } else { 500 };
        assert!(took.as_millis() < budget, "{took:?}");
        assert_eq!(tl["commands"], total);
        assert_eq!(tl["aggregated"], false);
        let lanes = tl["lanes"].as_array().unwrap();
        assert_eq!(lanes.len(), 10);
        assert_eq!(lanes[0]["rows"].as_array().unwrap().len(), 3, "one row per pane");
        let blocks: usize = lanes
            .iter()
            .flat_map(|l| l["rows"].as_array().unwrap())
            .map(|r| r["blocks"].as_array().unwrap().len())
            .sum();
        assert_eq!(blocks as u64, total);
        let skips = lanes[0]["rows"][0]["marks"].as_array().unwrap().len();
        assert_eq!(skips, 7, "every skip is visible");
    }

    #[tokio::test]
    async fn many_commands_are_aggregated() {
        let kit = Kit::new(8);
        let end = START + 2.0 * DAY;
        let total = synthetic(&kit, 4, 2, 2, 1300, end);
        assert!(total > AGGREGATE_ABOVE);
        let tl = kit.json(&format!("/timeline?from={}&to={end}", end - 2.0 * DAY)).await;
        assert_eq!(tl["aggregated"], true);
        assert_eq!(tl["bin_secs"], 300.0);
        assert_eq!(tl["commands"], total);
        let mut counted = 0;
        let mut failed = 0;
        for row in tl["lanes"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|l| l["rows"].as_array().unwrap())
        {
            assert_eq!(row["blocks"], Value::Array(vec![]));
            assert!(!row["marks"].as_array().unwrap().is_empty(), "marks are always sent");
            let mut prev = f64::NEG_INFINITY;
            for bin in row["density"].as_array().unwrap() {
                let ts = bin["ts"].as_f64().unwrap();
                assert!(ts > prev);
                prev = ts;
                assert!(bin["commands"].as_u64().unwrap() > 0);
                counted += bin["commands"].as_u64().unwrap();
                failed += bin["failed"].as_u64().unwrap();
            }
        }
        assert_eq!(counted, total);
        assert_eq!(failed, 4 * 2 * 2 * 1300_u64.div_ceil(7));
    }
}
