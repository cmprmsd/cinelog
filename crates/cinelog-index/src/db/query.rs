//! Read-only queries over index.db under the §6.5 rules.
//!
//! A search runs one or two *streams* (commands, outputs), each yielding hits in
//! `(ts, kind, id)` descending order, and merges them:
//! - indexable terms: the trigram index (`LIKE`/`GLOB` without `ESCAPE`, per column on
//!   `cmd_fts`) yields the matching rowids; joined with the filters and the cursor, the newest
//!   [`CANDIDATE_CAP`] by `ts` are the candidates (one more sets `capped`), walked newest
//!   first through the Rust post-filter. Rowid order is not `ts` order (backfill, parallel
//!   rebuilds), so the cap is applied by `ts` and after the filters;
//! - terms the index cannot serve (fewer than three literal characters): a newest-first walk
//!   of the table along its `ts` index, interrupted after [`SHORT_TERM_SCAN`] (`partial`);
//! - the empty term: the newest commands.
//!
//! `meta.forgotten` is read for every query and excluded here, before the writer has purged
//! the rows (including the echo of a forgotten command, see `forget`).

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, HashMap};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use rusqlite::types::Value;
use rusqlite::{Connection, InterruptHandle, OptionalExtension, Row, Rows, params, params_from_iter};

use super::forget::{Echo, ForgetSet, chunk_runs_past};
use super::text::Term;
use super::{
    LEGACY_SELECT, TOMBSTONE, U64, legacy_from_row, meta, opt_u64_at, parse_at, path_at, read_meta_forgotten, u64_at,
};
use crate::api::Searcher;
use crate::error::{IndexError, Result};
use crate::model::{
    CANDIDATE_CAP, ChunkKind, Command, Cursor, DbPaths, EntryDetail, EntryRef, ExitFilter, HitKind, IndexStatus,
    LegacyImportRow, LegacyProgress, LegacyStatus, LiveLag, SHORT_TERM_SCAN, SearchFilters, SearchHit, SearchPage,
    SearchQuery, SearchTarget, SessionCounts, SessionState, SessionSummary, TmuxLocation, info_keys,
};
use crate::schema::{self, Access, DbKind};

/// How often the watchdog re-interrupts once the scan deadline has passed: an interrupt that
/// lands between two statements is cleared by SQLite when the next one starts.
const REINTERRUPT: Duration = Duration::from_millis(5);

pub(crate) struct SqliteSearcher {
    conn: Connection,
    paths: DbPaths,
    /// Read-only meta.db, opened once it exists.
    meta: RefCell<Option<Connection>>,
    /// Set while a time-limited scan runs; rows are not read past it.
    deadline: Cell<Option<Instant>>,
    short_scan: Duration,
}

/// Columns of a command hit, in the order [`cmd_hit`] reads them.
const CMD_COLS: &str = "c.id, c.ts_start, c.t_start, c.t_end, c.seq, c.exit, c.cwd, c.typed, c.expanded,
    ifnull(c.user, s.user), c.approx, c.confidence, s.uuid, s.host";
/// Columns of an output hit, in the order [`out_hit`] reads them.
const OUT_COLS: &str = "o.id, o.ts, o.t, o.kind, s.uuid, s.host, ifnull(c.user, s.user), c.seq, c.exit, c.cwd,
    c.typed, c.approx, c.confidence";

const fn kind_rank(k: HitKind) -> u8 {
    match k {
        HitKind::Command => 0,
        HitKind::Output => 1,
    }
}

/// The candidate query of `kind` for the operator `op` (`LIKE` or `GLOB`; `?1` is the
/// pattern): every FTS match, joined with `where_` (filters and cursor, see [`Cond`]), the
/// newest [`CANDIDATE_CAP`] + 1 ids by `ts`. The match set is materialized so the trigram
/// lookups stay exactly the tested ones and drive the join.
pub(crate) fn candidate_sql(kind: HitKind, op: &str, where_: &str) -> String {
    let limit = CANDIDATE_CAP + 1;
    match kind {
        HitKind::Command => format!(
            "WITH m(id) AS MATERIALIZED (SELECT rowid FROM cmd_fts WHERE typed {op} ?1 OR expanded {op} ?1 OR cwd {op} ?1)
             SELECT c.id FROM m CROSS JOIN commands c ON c.id = m.id CROSS JOIN sessions s ON s.id = c.session_id{where_}
             ORDER BY c.ts_start DESC, c.id DESC LIMIT {limit}"
        ),
        HitKind::Output => format!(
            "WITH m(id) AS MATERIALIZED (SELECT rowid FROM out_fts WHERE text {op} ?1)
             SELECT o.id FROM m CROSS JOIN outputs o ON o.id = m.id CROSS JOIN sessions s ON s.id = o.session_id
             LEFT JOIN commands c ON c.id = o.command_id{where_}
             ORDER BY o.ts DESC, o.id DESC LIMIT {limit}"
        ),
    }
}

/// The newest-first table walk for terms the trigram index cannot serve. `?1` is the pattern.
pub(crate) fn scan_sql(kind: HitKind, op: &str, filters: &str) -> String {
    match kind {
        HitKind::Command => format!(
            "SELECT {CMD_COLS} FROM commands c INDEXED BY commands_ts CROSS JOIN sessions s ON s.id = c.session_id
             WHERE (c.typed {op} ?1 OR c.expanded {op} ?1 OR c.cwd {op} ?1){filters}
             ORDER BY c.ts_start DESC, c.id DESC"
        ),
        HitKind::Output => format!(
            "SELECT {OUT_COLS}, o.text FROM outputs o INDEXED BY outputs_ts CROSS JOIN sessions s ON s.id = o.session_id
             LEFT JOIN commands c ON c.id = o.command_id
             WHERE o.text {op} ?1{filters}
             ORDER BY o.ts DESC, o.id DESC"
        ),
    }
}

/// `s` as a literal GLOB pattern: the metacharacters `*`, `?`, `[` and `]` in brackets.
fn glob_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '*' | '?' | '[' | ']' => {
                out.push('[');
                out.push(ch);
                out.push(']');
            }
            _ => out.push(ch),
        }
    }
    out
}

/// SQL conditions with their numbered parameters.
#[derive(Debug, Default)]
struct Cond {
    clauses: Vec<String>,
    params: Vec<Value>,
}

impl Cond {
    fn with(params: Vec<Value>) -> Self {
        Self {
            clauses: Vec::new(),
            params,
        }
    }

    fn bind(&mut self, v: impl Into<Value>) -> String {
        self.params.push(v.into());
        format!("?{}", self.params.len())
    }

    fn push(&mut self, clause: String) {
        self.clauses.push(clause);
    }

    /// ` AND …` for appending to an existing `WHERE`.
    fn and(&self) -> String {
        self.clauses.iter().map(|c| format!(" AND {c}")).collect()
    }

    /// ` WHERE …`, or nothing.
    fn where_(&self) -> String {
        if self.clauses.is_empty() {
            String::new()
        } else {
            format!(" WHERE {}", self.clauses.join(" AND "))
        }
    }

    /// `cwd` as a prefix (plan §8.2): the directory itself and everything below it. A
    /// trailing `/` is ignored and `/` matches every absolute cwd.
    fn cwd(&mut self, dir: &str) {
        let base = dir.trim_end_matches('/');
        if dir.is_empty() {
            // Stored cwds are never empty (NULL instead): matches nothing, as before.
            let p = self.bind(String::new());
            self.push(format!("c.cwd = {p}"));
        } else if base.is_empty() {
            self.push("c.cwd GLOB '/*'".into());
        } else {
            let p = self.bind(base.to_owned());
            let q = self.bind(format!("{}/*", glob_escape(base)));
            self.push(format!("(c.cwd = {p} OR c.cwd GLOB {q})"));
        }
    }

    /// The §6.5 filters and the page cursor for a stream of `kind`.
    fn filters(&mut self, f: &SearchFilters, kind: HitKind, cursor: Option<Cursor>) {
        let (ts, id) = match kind {
            HitKind::Command => ("c.ts_start", "c.id"),
            HitKind::Output => ("o.ts", "o.id"),
        };
        if !f.include_hidden {
            self.push("s.hidden = 0".into());
        }
        if let Some(v) = &f.user {
            let p = self.bind(v.clone());
            self.push(format!("ifnull(c.user, s.user) = {p}"));
        }
        for (col, v) in [("s.host", &f.host), ("s.uuid", &f.session), ("s.tmux_pane", &f.pane)] {
            if let Some(v) = v {
                let p = self.bind(v.clone());
                self.push(format!("{col} = {p}"));
            }
        }
        if let Some(v) = &f.cwd {
            self.cwd(v);
        }
        if let Some(v) = f.since {
            let p = self.bind(v);
            self.push(format!("{ts} >= {p}"));
        }
        if let Some(v) = f.until {
            let p = self.bind(v);
            self.push(format!("{ts} <= {p}"));
        }
        match f.exit {
            Some(ExitFilter::Code(code)) => {
                let p = self.bind(i64::from(code));
                self.push(format!("c.exit = {p}"));
            }
            Some(ExitFilter::Success) => self.push("c.exit = 0".into()),
            Some(ExitFilter::Failure) => self.push("c.exit <> 0".into()),
            None => {}
        }
        if let Some(c) = cursor {
            // Hits strictly after the cursor in (ts, kind, id) descending order.
            let p = self.bind(c.ts);
            let clause = match kind_rank(kind).cmp(&kind_rank(c.kind)) {
                std::cmp::Ordering::Less => format!("{ts} <= {p}"),
                std::cmp::Ordering::Greater => format!("{ts} < {p}"),
                std::cmp::Ordering::Equal => {
                    let q = self.bind(c.id);
                    format!("({ts} < {p} OR ({ts} = {p} AND {id} < {q}))")
                }
            };
            self.push(clause);
        }
    }
}

fn is_interrupt(e: &rusqlite::Error) -> bool {
    matches!(e, rusqlite::Error::SqliteFailure(f, _) if f.code == rusqlite::ErrorCode::OperationInterrupted)
}

/// Interrupts the connection's running statement once `deadline` has passed, until dropped.
struct Watchdog {
    stop: Option<mpsc::Sender<()>>,
    thread: Option<JoinHandle<()>>,
}

impl Watchdog {
    fn start(handle: InterruptHandle, deadline: Instant) -> Self {
        let (stop, rx) = mpsc::channel::<()>();
        let thread = std::thread::Builder::new()
            .name("cinelog-scan-deadline".into())
            .spawn(move || {
                let mut wait = deadline.saturating_duration_since(Instant::now());
                while let Err(RecvTimeoutError::Timeout) = rx.recv_timeout(wait) {
                    handle.interrupt();
                    wait = REINTERRUPT;
                }
            })
            // Without the thread the per-row deadline check still bounds the scan.
            .map_err(|e| tracing::warn!("scan deadline thread: {e}"))
            .ok();
        Self {
            stop: Some(stop),
            thread,
        }
    }
}

impl Drop for Watchdog {
    fn drop(&mut self) {
        drop(self.stop.take());
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// A command hit plus what the post-filter and forget check need.
struct CmdRow {
    hit: SearchHit,
    t_end: Option<f64>,
    expanded: Option<String>,
}

fn cmd_hit(r: &Row<'_>) -> rusqlite::Result<CmdRow> {
    let typed: String = r.get(7)?;
    Ok(CmdRow {
        t_end: r.get(3)?,
        expanded: r.get(8)?,
        hit: SearchHit {
            kind: HitKind::Command,
            id: r.get(0)?,
            ts: r.get(1)?,
            t: r.get(2)?,
            cmd_seq: Some(u64_at(r, 4)?),
            exit: r.get(5)?,
            cwd: r.get(6)?,
            typed: Some(typed),
            user: r.get(9)?,
            approx: r.get(10)?,
            confidence: r.get(11)?,
            session_uuid: r.get(12)?,
            host: r.get(13)?,
            snippet: None,
            chunk_kind: None,
        },
    })
}

fn out_hit(r: &Row<'_>) -> rusqlite::Result<SearchHit> {
    Ok(SearchHit {
        kind: HitKind::Output,
        id: r.get(0)?,
        ts: r.get(1)?,
        t: r.get(2)?,
        chunk_kind: Some(parse_at(r, 3)?),
        session_uuid: r.get(4)?,
        host: r.get(5)?,
        user: r.get(6)?,
        cmd_seq: opt_u64_at(r, 7)?,
        exit: r.get(8)?,
        cwd: r.get(9)?,
        typed: r.get(10)?,
        approx: r.get::<_, Option<bool>>(11)?.unwrap_or(false),
        confidence: r.get(12)?,
        snippet: None,
    })
}

/// Columns of a full command row, in the order [`command_from_row`] reads them.
const COMMAND_COLS: &str = "id, seq, parent_seq, depth, shell_pid, t_start, t_end, ts_start, ts_end, exit, pipestatus,
    cwd, user, euid, typed, expanded, typed_trunc, approx, confidence, out_bytes, out_truncated";

fn command_from_row(r: &Row<'_>) -> rusqlite::Result<(i64, Command)> {
    let pipestatus: Option<String> = r.get(10)?;
    let pipestatus = pipestatus
        .map(|p| serde_json::from_str(&p))
        .transpose()
        .map_err(|e| rusqlite::Error::FromSqlConversionFailure(10, rusqlite::types::Type::Text, Box::new(e)))?
        .unwrap_or_default();
    Ok((
        r.get(0)?,
        Command {
            seq: u64_at(r, 1)?,
            parent_seq: opt_u64_at(r, 2)?,
            depth: r.get(3)?,
            shell_pid: r.get(4)?,
            t_start: r.get(5)?,
            t_end: r.get(6)?,
            ts_start: r.get(7)?,
            ts_end: r.get(8)?,
            exit: r.get(9)?,
            pipestatus,
            cwd: r.get(11)?,
            user: r.get(12)?,
            euid: r.get(13)?,
            typed: r.get(14)?,
            expanded: r.get(15)?,
            typed_trunc: r.get(16)?,
            approx: r.get(17)?,
            confidence: r.get(18)?,
            out_bytes: u64_at(r, 19)?,
            out_truncated: r.get(20)?,
        },
    ))
}

const SUMMARY_SQL: &str =
    "SELECT s.id, s.uuid, s.host, s.user, s.scope, s.state, s.started_at, s.ended_at, s.cast_path,
    s.tmux_server, s.tmux_session_id, s.tmux_session, s.tmux_window, s.tmux_window_name, s.tmux_pane, s.legacy,
    s.hidden, (SELECT count(*) FROM commands WHERE session_id = s.id)
    FROM sessions s WHERE s.uuid = ?1 AND s.attribution IS NOT ?2";

fn summary_from_row(r: &Row<'_>) -> rusqlite::Result<(i64, SessionSummary)> {
    Ok((
        r.get(0)?,
        SessionSummary {
            uuid: r.get(1)?,
            host: r.get(2)?,
            user: r.get(3)?,
            scope: parse_at(r, 4)?,
            state: parse_at(r, 5)?,
            started_at: r.get(6)?,
            ended_at: r.get(7)?,
            cast_path: path_at(r, 8)?,
            tmux: TmuxLocation {
                server: r.get(9)?,
                session_id: r.get(10)?,
                session: r.get(11)?,
                window: r.get(12)?,
                window_name: r.get(13)?,
                pane: r.get(14)?,
            },
            legacy: r.get(15)?,
            hidden: r.get(16)?,
            commands: u64_at(r, 17)?,
        },
    ))
}

/// Concatenated chunk texts (newline-separated) up to `max` bytes.
struct OutputText {
    text: String,
    truncated: bool,
    max: usize,
}

impl OutputText {
    fn new(max: usize) -> Self {
        Self {
            text: String::new(),
            truncated: false,
            max,
        }
    }

    /// Returns false once full.
    fn push(&mut self, chunk: &str) -> bool {
        if !self.text.is_empty() {
            self.text.push('\n');
        }
        self.text.push_str(chunk);
        if self.text.len() > self.max {
            let mut cut = self.max;
            while !self.text.is_char_boundary(cut) {
                cut -= 1;
            }
            self.text.truncate(cut);
            self.truncated = true;
        }
        !self.truncated
    }
}

/// The echoes of forgotten commands, loaded per session uuid once per stream.
struct Echoes<'f> {
    forget: &'f ForgetSet,
    by_uuid: HashMap<String, Vec<Echo>>,
}

impl<'f> Echoes<'f> {
    fn new(forget: &'f ForgetSet) -> Self {
        Self {
            forget,
            by_uuid: HashMap::new(),
        }
    }
}

/// One stream's result.
#[derive(Default)]
struct Stream {
    hits: Vec<SearchHit>,
    partial: bool,
    capped: bool,
}

impl SqliteSearcher {
    pub(crate) fn open(paths: &DbPaths) -> Result<Self> {
        let conn = schema::open(&paths.index, DbKind::Index, Access::ReadOnly)?;
        Ok(Self {
            conn,
            paths: paths.clone(),
            meta: RefCell::new(None),
            deadline: Cell::new(None),
            short_scan: SHORT_TERM_SCAN,
        })
    }

    #[cfg(test)]
    pub(crate) fn set_short_scan(&mut self, limit: Duration) {
        self.short_scan = limit;
    }

    fn past_deadline(&self) -> bool {
        self.deadline.get().is_some_and(|d| Instant::now() >= d)
    }

    /// Next row of a stream; past the scan deadline (checked here and enforced inside SQLite
    /// by the [`Watchdog`]) the stream ends with `partial` set.
    fn next_row<'s>(&self, rows: &'s mut Rows<'_>, partial: &mut bool) -> Result<Option<&'s Row<'s>>> {
        if self.past_deadline() {
            *partial = true;
            return Ok(None);
        }
        Ok(rows.next()?)
    }

    /// Runs one stream. During a time-limited scan the [`Watchdog`] may interrupt any of its
    /// statements, also while one is being prepared: such an interrupt, like a deadline that
    /// passed before the stream began, ends the stream with `partial` set and keeps its hits.
    fn stream(&self, walk: impl FnOnce(&mut Stream) -> Result<()>) -> Result<Stream> {
        let mut out = Stream::default();
        if self.past_deadline() {
            out.partial = true;
            return Ok(out);
        }
        match walk(&mut out) {
            Err(IndexError::Sqlite(e)) if self.deadline.get().is_some() && is_interrupt(&e) => out.partial = true,
            r => r?,
        }
        Ok(out)
    }

    fn forget_set(&self) -> Result<ForgetSet> {
        let mut meta = self.meta.borrow_mut();
        if let Some(conn) = meta.as_ref() {
            return Ok(ForgetSet::new(&meta::read_forgotten(conn)?));
        }
        Ok(match read_meta_forgotten(&self.paths.meta)? {
            Some((conn, list)) => {
                *meta = Some(conn);
                ForgetSet::new(&list)
            }
            None => ForgetSet::default(),
        })
    }

    /// The candidate ids of `kind`, newest first, and whether there were more.
    fn candidates(&self, kind: HitKind, q: &SearchQuery, term: &Term) -> Result<(Vec<i64>, bool)> {
        let mut cond = Cond::with(vec![Value::Text(term.pattern().to_owned())]);
        cond.filters(&q.filters, kind, q.cursor);
        let mut st = self
            .conn
            .prepare_cached(&candidate_sql(kind, term.op(), &cond.where_()))?;
        let mut ids: Vec<i64> = st
            .query_map(params_from_iter(&cond.params), |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        let capped = ids.len() > CANDIDATE_CAP;
        ids.truncate(CANDIDATE_CAP);
        Ok((ids, capped))
    }

    /// Walks command rows, keeping those that pass the post-filter and are not forgotten.
    fn take_commands(
        &self,
        out: &mut Stream,
        sql: &str,
        params: &[Value],
        term: &Term,
        forget: &ForgetSet,
        want: usize,
    ) -> Result<()> {
        let mut st = self.conn.prepare_cached(sql)?;
        let mut rows = st.query(params_from_iter(params))?;
        while let Some(r) = self.next_row(&mut rows, &mut out.partial)? {
            let c = cmd_hit(r)?;
            let h = &c.hit;
            let seq = h.cmd_seq.unwrap_or_default();
            if forget.command(&h.session_uuid, seq, h.t, c.t_end) {
                continue;
            }
            if !term.is_empty() && term.needs_post_filter() {
                let hay = [h.typed.as_deref(), c.expanded.as_deref(), h.cwd.as_deref()];
                if !hay.into_iter().flatten().any(|s| term.matches(s)) {
                    continue;
                }
            }
            out.hits.push(c.hit);
            if out.hits.len() >= want {
                break;
            }
        }
        Ok(())
    }

    /// Walks output rows; `text_col` is the index of `o.text` in the row, or `None` to load
    /// the text per hit (indexed path: the candidate sort must not carry chunk texts).
    #[allow(clippy::too_many_arguments)]
    fn take_outputs(
        &self,
        out: &mut Stream,
        sql: &str,
        params: &[Value],
        text_col: Option<usize>,
        term: &Term,
        forget: &ForgetSet,
        want: usize,
    ) -> Result<()> {
        let mut st = self.conn.prepare_cached(sql)?;
        let mut text_of = self.conn.prepare_cached("SELECT text FROM outputs WHERE id = ?1")?;
        let mut echoes = Echoes::new(forget);
        let mut rows = st.query(params_from_iter(params))?;
        while let Some(r) = self.next_row(&mut rows, &mut out.partial)? {
            let mut h = out_hit(r)?;
            if forget.chunk(&h.session_uuid, h.cmd_seq, h.t, |a| {
                chunk_runs_past(&self.conn, h.id, a)
            })? {
                continue;
            }
            let text: String = match text_col {
                Some(i) => r.get(i)?,
                None => match text_of.query_row([h.id], |r| r.get(0)).optional()? {
                    Some(t) => t,
                    None => continue,
                },
            };
            let (text, stripped) = match h.chunk_kind {
                Some(kind) => self.without_echoes(&mut echoes, &h.session_uuid, h.id, kind, h.t, text)?,
                None => (text, false),
            };
            if (stripped || term.needs_post_filter()) && !term.matches(&text) {
                continue;
            }
            h.snippet = term.snippet(&text);
            out.hits.push(h);
            if out.hits.len() >= want {
                break;
            }
        }
        Ok(())
    }

    fn indexed_stream(&self, kind: HitKind, q: &SearchQuery, term: &Term, forget: &ForgetSet) -> Result<Stream> {
        let (ids, capped) = self.candidates(kind, q, term)?;
        if ids.is_empty() {
            return Ok(Stream::default());
        }
        // The candidates already passed the filters and are in page order.
        let params = [Value::Text(serde_json::to_string(&ids)?)];
        let want = q.limit + 1;
        let mut s = Stream::default();
        match kind {
            HitKind::Command => {
                let sql = format!(
                    "SELECT {CMD_COLS} FROM json_each(?1) j CROSS JOIN commands c ON c.id = j.value
                     CROSS JOIN sessions s ON s.id = c.session_id ORDER BY j.key"
                );
                self.take_commands(&mut s, &sql, &params, term, forget, want)?;
            }
            HitKind::Output => {
                let sql = format!(
                    "SELECT {OUT_COLS} FROM json_each(?1) j CROSS JOIN outputs o ON o.id = j.value
                     CROSS JOIN sessions s ON s.id = o.session_id LEFT JOIN commands c ON c.id = o.command_id
                     ORDER BY j.key"
                );
                self.take_outputs(&mut s, &sql, &params, None, term, forget, want)?;
            }
        }
        s.capped = capped;
        Ok(s)
    }

    fn scan_stream(&self, kind: HitKind, q: &SearchQuery, term: &Term, forget: &ForgetSet) -> Result<Stream> {
        let mut cond = Cond::with(vec![Value::Text(term.pattern().to_owned())]);
        cond.filters(&q.filters, kind, q.cursor);
        let sql = scan_sql(kind, term.op(), &cond.and());
        let want = q.limit + 1;
        self.stream(|out| match kind {
            HitKind::Command => self.take_commands(out, &sql, &cond.params, term, forget, want),
            HitKind::Output => self.take_outputs(out, &sql, &cond.params, Some(13), term, forget, want),
        })
    }

    fn newest_commands(&self, q: &SearchQuery, term: &Term, forget: &ForgetSet) -> Result<Stream> {
        let mut cond = Cond::default();
        cond.filters(&q.filters, HitKind::Command, q.cursor);
        let sql = format!(
            "SELECT {CMD_COLS} FROM commands c JOIN sessions s ON s.id = c.session_id{}
             ORDER BY c.ts_start DESC, c.id DESC",
            cond.where_()
        );
        let mut s = Stream::default();
        self.take_commands(&mut s, &sql, &cond.params, term, forget, q.limit + 1)?;
        Ok(s)
    }

    fn search_streams(&self, q: &SearchQuery, term: &Term, forget: &ForgetSet) -> Result<Vec<Stream>> {
        let kinds: &[HitKind] = match q.target {
            SearchTarget::Commands => &[HitKind::Command],
            SearchTarget::Output => &[HitKind::Output],
            SearchTarget::All => &[HitKind::Command, HitKind::Output],
        };
        if term.is_empty() {
            return Ok(if q.target == SearchTarget::Output {
                Vec::new()
            } else {
                vec![self.newest_commands(q, term, forget)?]
            });
        }
        if term.indexable() {
            return kinds.iter().map(|&k| self.indexed_stream(k, q, term, forget)).collect();
        }
        // One time budget for the whole scan, newest first.
        let deadline = Instant::now() + self.short_scan;
        self.deadline.set(Some(deadline));
        let watchdog = Watchdog::start(self.conn.get_interrupt_handle(), deadline);
        let res = kinds.iter().map(|&k| self.scan_stream(k, q, term, forget)).collect();
        drop(watchdog);
        self.deadline.set(None);
        res
    }

    fn output_text(&self, cond: &Cond, uuid: &str, forget: &ForgetSet, max: usize) -> Result<(String, bool)> {
        let sql = format!(
            "SELECT o.id, o.t, c.seq, o.text, o.kind FROM outputs o LEFT JOIN commands c ON c.id = o.command_id{}
             ORDER BY o.t, o.id",
            cond.where_()
        );
        let mut st = self.conn.prepare_cached(&sql)?;
        let mut rows = st.query(params_from_iter(&cond.params))?;
        let mut out = OutputText::new(max);
        let mut echoes = Echoes::new(forget);
        while let Some(r) = rows.next()? {
            let (id, t, seq): (i64, f64, Option<u64>) = (r.get(0)?, r.get(1)?, opt_u64_at(r, 2)?);
            if forget.chunk(uuid, seq, t, |a| chunk_runs_past(&self.conn, id, a))? {
                continue;
            }
            let (text, _) = self.without_echoes(&mut echoes, uuid, id, parse_at(r, 4)?, t, r.get(3)?)?;
            if text.is_empty() {
                // Blanked by a forget.
                continue;
            }
            if !out.push(&text) {
                break;
            }
        }
        Ok((out.text, out.truncated))
    }

    /// The echoes of a session's forgotten commands that index.db still holds.
    fn echoes<'c>(&self, cache: &'c mut Echoes<'_>, uuid: &str) -> Result<&'c [Echo]> {
        if !cache.by_uuid.contains_key(uuid) {
            let seqs = cache.forget.seqs(uuid);
            let mut list = Vec::new();
            if !seqs.is_empty()
                && let Some(sid) = self
                    .conn
                    .prepare_cached("SELECT id FROM sessions WHERE uuid = ?1")?
                    .query_row([uuid], |r| r.get::<_, i64>(0))
                    .optional()?
            {
                for seq in seqs {
                    list.extend(Echo::of_stored(&self.conn, sid, seq)?);
                }
            }
            cache.by_uuid.insert(uuid.to_owned(), list);
        }
        Ok(cache.by_uuid.get(uuid).map_or(&[], Vec::as_slice))
    }

    /// `text` of chunk `id` without the echoes of forgotten commands the writer has not
    /// purged yet, and whether any was stripped.
    fn without_echoes(
        &self,
        cache: &mut Echoes<'_>,
        uuid: &str,
        id: i64,
        kind: ChunkKind,
        t: f64,
        mut text: String,
    ) -> Result<(String, bool)> {
        let mut stripped = false;
        if !Echo::kind(kind) {
            return Ok((text, stripped));
        }
        for e in self.echoes(cache, uuid)? {
            if e.spans(t)
                && e.owner_of(&self.conn, id)?
                && let Some(s) = e.strip(&text)
            {
                text = s;
                stripped = true;
            }
        }
        Ok((text, stripped))
    }

    fn summary(&self, uuid: &str) -> Result<Option<(i64, SessionSummary)>> {
        Ok(self
            .conn
            .prepare_cached(SUMMARY_SQL)?
            .query_row([uuid, TOMBSTONE], summary_from_row)
            .optional()?)
    }

    fn command(&self, sql_where: &str, args: impl rusqlite::Params) -> Result<Option<(i64, Command)>> {
        Ok(self
            .conn
            .prepare_cached(&format!("SELECT {COMMAND_COLS} FROM commands WHERE {sql_where}"))?
            .query_row(args, command_from_row)
            .optional()?)
    }
}

impl Searcher for SqliteSearcher {
    fn search(&self, q: &SearchQuery) -> Result<SearchPage> {
        if q.limit == 0 {
            return Ok(SearchPage::default());
        }
        if !q.filters.tags.is_empty() {
            // Contract only: implemented by the backend work package.
            return Err(IndexError::Invalid(
                "the search tag filter is not implemented yet".into(),
            ));
        }
        let term = Term::new(&q.term, q.case_sensitive);
        let forget = self.forget_set()?;
        // One read snapshot for every statement of this search.
        let tx = self.conn.unchecked_transaction()?;
        let streams = self.search_streams(q, &term, &forget)?;
        drop(tx);

        let mut page = SearchPage {
            partial: streams.iter().any(|s| s.partial),
            capped: streams.iter().any(|s| s.capped),
            ..SearchPage::default()
        };
        let mut hits: Vec<SearchHit> = streams.into_iter().flat_map(|s| s.hits).collect();
        hits.sort_by(|a, b| {
            b.ts.total_cmp(&a.ts)
                .then(kind_rank(b.kind).cmp(&kind_rank(a.kind)))
                .then(b.id.cmp(&a.id))
        });
        let more = hits.len() > q.limit;
        hits.truncate(q.limit);
        page.next = more.then(|| hits.last()).flatten().map(|h| Cursor {
            ts: h.ts,
            kind: h.kind,
            id: h.id,
        });
        page.hits = hits;
        Ok(page)
    }

    fn session(&self, uuid: &str) -> Result<Option<SessionSummary>> {
        if self.forget_set()?.session(uuid) {
            return Ok(None);
        }
        Ok(self.summary(uuid)?.map(|(_, s)| s))
    }

    fn show(&self, entry: &EntryRef, max_output_bytes: usize) -> Result<Option<EntryDetail>> {
        let uuid = entry.uuid();
        let forget = self.forget_set()?;
        if forget.session(uuid) {
            return Ok(None);
        }
        let _tx = self.conn.unchecked_transaction()?;
        let Some((sid, session)) = self.summary(uuid)? else {
            return Ok(None);
        };
        let running = match entry {
            EntryRef::Session { .. } => None,
            EntryRef::Command { seq, .. } => {
                match self.command("session_id = ?1 AND seq = ?2", params![sid, U64(*seq)])? {
                    Some(c) => Some(c),
                    None => return Ok(None),
                }
            }
            // The innermost command running at t.
            EntryRef::At { t, .. } => self.command(
                "session_id = ?1 AND t_start <= ?2 AND (t_end IS NULL OR t_end >= ?2)
                 ORDER BY depth DESC, t_start DESC, id DESC LIMIT 1",
                params![sid, t],
            )?,
        };
        if let Some((_, c)) = &running
            && forget.command(uuid, c.seq, c.t_start, c.t_end)
        {
            return Ok(None);
        }
        let mut cond = Cond::default();
        match (&running, entry) {
            (Some((cid, _)), _) => {
                let p = cond.bind(*cid);
                cond.push(format!("o.command_id = {p}"));
            }
            (None, EntryRef::At { t, .. }) => {
                // Unattributed output between the previous command's end and the next start.
                let (prev_end, next_start): (Option<f64>, Option<f64>) = self.conn.query_row(
                    "SELECT (SELECT max(t_end) FROM commands WHERE session_id = ?1 AND t_end <= ?2),
                            (SELECT min(t_start) FROM commands WHERE session_id = ?1 AND t_start > ?2)",
                    params![sid, t],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )?;
                let p = cond.bind(sid);
                cond.push(format!("o.session_id = {p} AND o.command_id IS NULL"));
                if let Some(v) = prev_end {
                    let p = cond.bind(v);
                    cond.push(format!("o.t >= {p}"));
                }
                if let Some(v) = next_start {
                    let p = cond.bind(v);
                    cond.push(format!("o.t < {p}"));
                }
            }
            (None, _) => {
                let p = cond.bind(sid);
                cond.push(format!("o.session_id = {p}"));
            }
        }
        let (output, output_truncated) = self.output_text(&cond, uuid, &forget, max_output_bytes)?;
        Ok(Some(EntryDetail {
            session,
            command: running.map(|(_, c)| c),
            output,
            output_truncated,
        }))
    }

    fn status(&self) -> Result<IndexStatus> {
        let _tx = self.conn.unchecked_transaction()?;
        let mut st = IndexStatus {
            schema_version: schema::user_version(&self.conn)?,
            ..IndexStatus::default()
        };
        let mut wal = self.paths.index.clone().into_os_string();
        wal.push("-wal");
        st.index_bytes = [self.paths.index.as_os_str(), wal.as_os_str()]
            .iter()
            .filter_map(|p| std::fs::metadata(p).ok())
            .map(|m| m.len())
            .sum();

        let mut counts = SessionCounts::default();
        {
            let mut q = self.conn.prepare(
                "SELECT state, count(*), sum(legacy), sum(hidden) FROM sessions WHERE attribution IS NOT ?1
                 GROUP BY state",
            )?;
            let mut rows = q.query([TOMBSTONE])?;
            while let Some(r) = rows.next()? {
                let state: SessionState = parse_at(r, 0)?;
                let n = u64_at(r, 1)?;
                *match state {
                    SessionState::Live => &mut counts.live,
                    SessionState::Ended => &mut counts.ended,
                    SessionState::Crashed => &mut counts.crashed,
                    SessionState::Degraded => &mut counts.degraded,
                    SessionState::Error => &mut counts.error,
                } += n;
                counts.legacy += opt_u64_at(r, 2)?.unwrap_or(0);
                counts.hidden += opt_u64_at(r, 3)?.unwrap_or(0);
            }
        }
        st.sessions = counts;
        st.commands = self
            .conn
            .query_row("SELECT count(*) FROM commands", [], |r| u64_at(r, 0))?;
        st.outputs = self
            .conn
            .query_row("SELECT count(*) FROM outputs", [], |r| u64_at(r, 0))?;

        // Parts per session: (part, path, compressed, uncompressed bytes).
        type Part = (u32, std::path::PathBuf, bool, Option<u64>);
        let mut parts: HashMap<i64, Vec<Part>> = HashMap::new();
        {
            let mut q = self.conn.prepare(
                "SELECT session_id, part, path, compressed, bytes FROM cast_parts ORDER BY session_id, part",
            )?;
            let mut rows = q.query([])?;
            while let Some(r) = rows.next()? {
                parts
                    .entry(r.get(0)?)
                    .or_default()
                    .push((r.get(1)?, path_at(r, 2)?, r.get(3)?, opt_u64_at(r, 4)?));
            }
        }
        // A finished legacy import (it has its `legacy_import` row) is a snapshot of its
        // cast: later growth is a new import, never a resume, so it is no backlog.
        let mut q = self.conn.prepare(
            "SELECT s.id, s.uuid, s.cast_path, s.state, s.compressed, s.cast_bytes, i.part, i.byte_offset, i.updated_at
             FROM sessions s LEFT JOIN index_state i ON i.session_id = s.id WHERE s.attribution IS NOT ?1
               AND s.id NOT IN (SELECT session_id FROM legacy_import WHERE session_id IS NOT NULL)
             ORDER BY s.started_at",
        )?;
        let mut rows = q.query([TOMBSTONE])?;
        while let Some(r) = rows.next()? {
            let sid: i64 = r.get(0)?;
            let cast_path = path_at(r, 2)?;
            let state: SessionState = parse_at(r, 3)?;
            let cp: Option<(u32, u64)> = match (r.get::<_, Option<u32>>(6)?, opt_u64_at(r, 7)?) {
                (Some(p), Some(o)) => Some((p, o)),
                _ => None,
            };
            let own = parts.remove(&sid).unwrap_or_else(|| {
                vec![(
                    1,
                    cast_path.clone(),
                    r.get(4).unwrap_or(false),
                    opt_u64_at(r, 5).unwrap_or(None),
                )]
            });
            let mut indexed_part_size = None;
            for (part, path, compressed, bytes) in &own {
                let size = if *compressed {
                    bytes.unwrap_or(0)
                } else {
                    std::fs::metadata(path).map(|m| m.len()).unwrap_or(0)
                };
                let pending = match cp {
                    Some((p, _)) if *part < p => 0,
                    Some((p, off)) if *part == p => {
                        indexed_part_size = Some(size);
                        size.saturating_sub(off)
                    }
                    _ => size,
                };
                st.backlog_bytes += pending;
            }
            if state == SessionState::Live {
                let first_size = own
                    .first()
                    .and_then(|(_, p, _, _)| std::fs::metadata(p).ok())
                    .map(|m| m.len());
                st.live.push(LiveLag {
                    uuid: r.get(1)?,
                    cast_path,
                    indexed_bytes: cp.map_or(0, |(_, o)| o),
                    file_bytes: indexed_part_size.or(first_size),
                    updated_at: r.get(8)?,
                });
            }
        }

        let mut legacy = LegacyProgress::default();
        {
            let mut q = self.conn.prepare(
                "SELECT status, count(*), ifnull(sum(banners), 0), ifnull(sum(log_lines), 0), ifnull(sum(aligned), 0)
                 FROM legacy_import GROUP BY status",
            )?;
            let mut rows = q.query([])?;
            while let Some(r) = rows.next()? {
                let status: LegacyStatus = parse_at(r, 0)?;
                let n = u64_at(r, 1)?;
                legacy.files += n;
                *match status {
                    LegacyStatus::Imported => &mut legacy.imported,
                    LegacyStatus::Hidden => &mut legacy.hidden,
                    LegacyStatus::Skipped => &mut legacy.skipped,
                    LegacyStatus::Failed => &mut legacy.failed,
                } += n;
                legacy.banners += u64_at(r, 2)?;
                legacy.log_lines += u64_at(r, 3)?;
                legacy.aligned += u64_at(r, 4)?;
            }
        }
        st.legacy = legacy;

        let info: BTreeMap<String, String> = self
            .conn
            .prepare("SELECT key, value FROM index_info WHERE value IS NOT NULL ORDER BY key")?
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?;
        // A newer index.db or meta.db (a downgrade, §10.3) outranks the indexer's last error:
        // nothing is indexed until cinelog is upgraded again.
        let meta_version = if self.paths.meta.exists() {
            schema::open(&self.paths.meta, DbKind::Meta, Access::ReadOnly)
                .and_then(|c| schema::user_version(&c))
                .ok()
        } else {
            None
        };
        st.last_error = schema::newer_note(DbKind::Index, st.schema_version)
            .or_else(|| meta_version.and_then(|v| schema::newer_note(DbKind::Meta, v)))
            .or_else(|| info.get(info_keys::LAST_ERROR).cloned());
        st.info = info;
        Ok(st)
    }

    fn legacy_report(&self) -> Result<Vec<LegacyImportRow>> {
        let mut st = self.conn.prepare_cached(LEGACY_SELECT)?;
        let mut rows = st
            .query_map([], legacy_from_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        // Paths are TEXT or BLOB (non-UTF-8), which SQLite would order by type first.
        rows.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(rows)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The watchdog interrupts a single long-running statement (one `step` that never
    /// returns a row) and leaves the connection usable afterwards.
    #[test]
    fn watchdog_interrupts_a_running_statement() {
        let conn = Connection::open_in_memory().unwrap();
        let endless = "WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM c) SELECT x FROM c WHERE x < 0";
        let started = Instant::now();
        let dog = Watchdog::start(conn.get_interrupt_handle(), started + Duration::from_millis(50));
        let err = conn.query_row(endless, [], |r| r.get::<_, i64>(0)).unwrap_err();
        drop(dog);
        assert!(is_interrupt(&err), "{err:?}");
        assert!(started.elapsed() < Duration::from_secs(5));
        assert_eq!(conn.query_row("SELECT 7", [], |r| r.get::<_, i64>(0)).unwrap(), 7);
    }

    /// An interrupt that lands while a statement is being prepared (another one of the
    /// stream is still running, so SQLite keeps the flag) ends a scan stream as partial and
    /// keeps what it collected; outside a scan it stays an error.
    #[test]
    fn interrupt_during_prepare_ends_a_scan_stream_as_partial() {
        let dir = tempfile::tempdir().unwrap();
        let paths = DbPaths::in_dir(dir.path());
        drop(crate::db::open_writer(&paths, &Default::default()).unwrap());
        let r = SqliteSearcher::open(&paths).unwrap();
        let walk = |out: &mut Stream| -> Result<()> {
            out.capped = true;
            let mut running = r.conn.prepare("SELECT 1 UNION ALL SELECT 2")?;
            let mut rows = running.query([])?;
            rows.next()?;
            r.conn.get_interrupt_handle().interrupt();
            r.conn.prepare("SELECT 3")?;
            Ok(())
        };
        let err = r.stream(walk).err().expect("prepare is interrupted");
        assert!(matches!(&err, IndexError::Sqlite(e) if is_interrupt(e)), "{err:?}");

        r.deadline.set(Some(Instant::now() + Duration::from_secs(60)));
        let s = r.stream(walk).unwrap();
        assert!(s.partial && s.capped);
        r.deadline.set(None);
        assert_eq!(r.conn.query_row("SELECT 7", [], |row| row.get::<_, i64>(0)).unwrap(), 7);
    }

    #[test]
    fn glob_escapes_metacharacters() {
        assert_eq!(glob_escape("/home/a"), "/home/a");
        assert_eq!(glob_escape("/x*y?[z]"), "/x[*]y[?][[]z[]]");
    }

    /// The cwd filter: the directory and its subdirectories, never a sibling sharing the
    /// prefix, GLOB metacharacters literal, a trailing `/` ignored, `/` matches everything.
    #[test]
    fn cwd_filter_matches_subdirectories() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE commands (cwd TEXT)").unwrap();
        let dirs = [
            "/",
            "/home",
            "/home/a",
            "/home/a/src",
            "/home/a/src/x",
            "/home/ab",
            "/home/a*",
            "/home/a*/b",
            "/home/[a]",
            "/home/[a]/x",
            "/home/a?",
            "/home/a]",
        ];
        for d in dirs {
            conn.execute("INSERT INTO commands VALUES (?1)", [d]).unwrap();
        }
        conn.execute("INSERT INTO commands VALUES (NULL)", []).unwrap();
        let matching = |dir: &str| -> Vec<String> {
            let mut c = Cond::default();
            c.cwd(dir);
            let sql = format!("SELECT c.cwd FROM commands c{} ORDER BY c.cwd", c.where_());
            let mut st = conn.prepare(&sql).unwrap();
            st.query_map(params_from_iter(c.params), |r| r.get(0))
                .unwrap()
                .map(|r| r.unwrap())
                .collect()
        };
        assert_eq!(matching("/home/a"), ["/home/a", "/home/a/src", "/home/a/src/x"]);
        assert_eq!(matching("/home/a/"), matching("/home/a"));
        assert_eq!(matching("/home/a/src/x"), ["/home/a/src/x"]);
        assert_eq!(matching("/home/a*"), ["/home/a*", "/home/a*/b"]);
        assert_eq!(matching("/home/[a]"), ["/home/[a]", "/home/[a]/x"]);
        assert_eq!(matching("/home/a?"), ["/home/a?"]);
        assert_eq!(matching("/home/a]"), ["/home/a]"]);
        let mut all: Vec<String> = dirs.iter().map(|d| (*d).to_owned()).collect();
        all.sort();
        assert_eq!(matching("/"), all);
        assert_eq!(matching("//"), all);
        assert!(matching("/nowhere").is_empty());
        assert!(matching("").is_empty());
    }

    /// Dropping the watchdog before its deadline never interrupts later statements.
    #[test]
    fn cancelled_watchdog_stays_quiet() {
        let conn = Connection::open_in_memory().unwrap();
        drop(Watchdog::start(
            conn.get_interrupt_handle(),
            Instant::now() + Duration::from_millis(10),
        ));
        std::thread::sleep(Duration::from_millis(30));
        let n: i64 = conn
            .query_row(
                "WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM c WHERE x < 200000) SELECT count(*) FROM c",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 200_000);
    }
}
