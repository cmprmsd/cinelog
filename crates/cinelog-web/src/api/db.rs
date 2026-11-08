//! Read-only access to index.db (M3-B's own queries next to `cinelog_index::db::open_reader`)
//! and to the forgotten list of meta.db.
//!
//! Every handler runs its database work through [`blocking`]: SQLite calls never run on the
//! async runtime. A missing or not yet migrated index is `503 unavailable`.

use std::collections::HashMap;
use std::ffi::OsStr;
use std::fmt::Write as _;
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;

use cinelog_index::model::{self as im, DbPaths};
use cinelog_index::schema::{self, Access, DbKind};
use cinelog_index::{IndexError, MetaStore, Searcher};
use rusqlite::types::{Value, ValueRef};
use rusqlite::{Connection, OptionalExtension, Row, params, params_from_iter};

use crate::api::types::{self as t, Scope};
use crate::error::{Result, WebError};
use crate::state::WebContext;

/// Index queries fail with the index's error type (SQL and paths stay out of responses).
pub(crate) type DbResult<T> = std::result::Result<T, IndexError>;

/// `sessions.attribution` of a forgotten session's tombstone (the writer keeps the row so
/// discovery sees the cast as indexed; it has no content and is never listed).
pub(crate) const TOMBSTONE: &str = "forgotten";

/// Runs blocking database or file work off the async runtime.
pub(crate) async fn blocking<T, F>(f: F) -> Result<T>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T> + Send + 'static,
{
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| WebError::Internal(format!("worker task failed: {e}")))?
}

/// Normalises a session uuid from a path or query (any case; lowercase hyphenated out).
pub(crate) fn parse_uuid(s: &str) -> Result<String> {
    uuid::Uuid::try_parse(s.trim())
        .map(|u| u.hyphenated().to_string())
        .map_err(|_| WebError::BadRequest("not a session uuid".into()))
}

fn require_index(paths: &DbPaths) -> Result<()> {
    if paths.index.exists() {
        Ok(())
    } else {
        Err(WebError::Unavailable("the index"))
    }
}

/// The M2 query engine over this scope's index (search, show, status, import report).
pub(crate) fn searcher(ctx: &WebContext) -> Result<Box<dyn Searcher>> {
    require_index(&ctx.db)?;
    Ok(cinelog_index::db::open_reader(&ctx.db)?)
}

/// One session row with the internal id and cast path the API never exposes.
#[derive(Debug, Clone)]
pub(crate) struct SessionRow {
    pub id: i64,
    pub cast_path: PathBuf,
    pub session: t::Session,
}

/// Columns of [`session_from_row`].
const SESSION_COLS: &str = "s.id, s.uuid, s.host, s.scope, s.user, s.euid, s.sudo_user, s.claimed_user, s.attribution,
    s.login_via, s.tty, s.holder_comm, s.tmux_server, s.tmux_session_id, s.tmux_session, s.tmux_window,
    s.tmux_window_name, s.tmux_pane, s.parent_uuid, s.cast_path, s.parts, s.compressed, s.cast_bytes, s.started_at,
    s.ended_at, s.exit_status, s.tz, s.cols, s.rows, s.term, s.legacy, s.hidden, s.state,
    (SELECT count(*) FROM commands c WHERE c.session_id = s.id),
    coalesce((SELECT i.t FROM index_state i WHERE i.session_id = s.id),
             (SELECT max(g.t_end) FROM segments g WHERE g.session_id = s.id))";

fn session_from_row(r: &Row<'_>) -> rusqlite::Result<SessionRow> {
    let scope: String = r.get(3)?;
    let scope = match scope.as_str() {
        "team" => Scope::Team,
        _ => Scope::Personal,
    };
    let state: String = r.get(32)?;
    let state: im::SessionState = state.parse().map_err(|e: IndexError| {
        rusqlite::Error::FromSqlConversionFailure(32, rusqlite::types::Type::Text, e.into())
    })?;
    let session = t::Session {
        uuid: r.get(1)?,
        host: r.get(2)?,
        scope,
        user: r.get(4)?,
        euid: r.get(5)?,
        sudo_user: r.get(6)?,
        claimed_user: r.get(7)?,
        attribution: r.get(8)?,
        login_via: r.get(9)?,
        tty: r.get(10)?,
        holder_comm: r.get(11)?,
        tmux: t::TmuxLocation {
            server: r.get(12)?,
            session_id: r.get(13)?,
            session: r.get(14)?,
            window: r.get(15)?,
            window_name: r.get(16)?,
            pane: r.get(17)?,
        },
        parent_uuid: r.get(18)?,
        parts: r.get::<_, Option<u32>>(20)?.unwrap_or(1),
        compressed: r.get(21)?,
        cast_bytes: opt_u64(r, 22)?,
        started_at: r.get(23)?,
        ended_at: r.get(24)?,
        duration: r.get(34)?,
        exit_status: r.get(25)?,
        tz: r.get(26)?,
        cols: r.get::<_, Option<u16>>(27)?.unwrap_or(80),
        rows: r.get::<_, Option<u16>>(28)?.unwrap_or(24),
        term: r.get(29)?,
        legacy: r.get(30)?,
        hidden: r.get(31)?,
        state: state.into(),
        commands: u64_at(r, 33)?,
    };
    Ok(SessionRow {
        id: r.get(0)?,
        cast_path: path_at(r, 19)?,
        session,
    })
}

fn path_at(r: &Row<'_>, idx: usize) -> rusqlite::Result<PathBuf> {
    Ok(match r.get_ref(idx)? {
        ValueRef::Text(b) | ValueRef::Blob(b) => PathBuf::from(OsStr::from_bytes(b)),
        _ => PathBuf::new(),
    })
}

fn u64_at(r: &Row<'_>, idx: usize) -> rusqlite::Result<u64> {
    Ok(opt_u64(r, idx)?.unwrap_or(0))
}

fn opt_u64(r: &Row<'_>, idx: usize) -> rusqlite::Result<Option<u64>> {
    match r.get::<_, Option<i64>>(idx)? {
        None => Ok(None),
        Some(v) => u64::try_from(v)
            .map(Some)
            .map_err(|_| rusqlite::Error::IntegralValueOutOfRange(idx, v)),
    }
}

fn i64_of(v: u64) -> i64 {
    i64::try_from(v).unwrap_or(i64::MAX)
}

/// Filters of the session list and the tree (all optional).
#[derive(Debug, Clone, Default)]
pub(crate) struct SessionFilter {
    pub live: Option<bool>,
    pub host: Option<String>,
    pub user: Option<String>,
    /// `started_at` bounds, inclusive.
    pub started_from: Option<f64>,
    pub started_to: Option<f64>,
    /// Active in `[from, to]`: started by `to`, not ended before `from`.
    pub active_from: Option<f64>,
    pub active_to: Option<f64>,
    pub pane: Option<String>,
    pub legacy: Option<bool>,
    pub include_hidden: bool,
    /// Sessions to leave out (forgotten ones the daemon has not purged yet).
    pub exclude: Vec<String>,
}

/// `(started_at, id)` of the last row of a session page.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct SessionCursor {
    pub started_at: f64,
    pub id: i64,
}

impl SessionCursor {
    pub fn encode(self) -> String {
        format!("{}:{}", self.started_at, self.id)
    }

    pub fn decode(s: &str) -> Result<Self> {
        let bad = || WebError::BadRequest("invalid cursor".into());
        let (ts, id) = s.split_once(':').ok_or_else(bad)?;
        Ok(Self {
            started_at: ts.parse::<f64>().ok().filter(|v| v.is_finite()).ok_or_else(bad)?,
            id: id.parse().map_err(|_| bad())?,
        })
    }
}

/// `(tmux pane, tmux session name, legacy)` of a session.
pub(crate) type HitContext = (Option<String>, Option<String>, bool);

/// A read-only index.db connection with the queries of the API.
pub(crate) struct IndexDb {
    conn: Connection,
}

impl IndexDb {
    pub fn open(ctx: &WebContext) -> Result<Self> {
        require_index(&ctx.db)?;
        Ok(Self::at(&ctx.db)?)
    }

    /// index.db of `paths`, which must exist.
    pub fn at(paths: &DbPaths) -> DbResult<Self> {
        Ok(Self {
            conn: schema::open(&paths.index, DbKind::Index, Access::ReadOnly)?,
        })
    }

    /// A consistent snapshot for several queries.
    pub fn snapshot(&self) -> DbResult<rusqlite::Transaction<'_>> {
        Ok(self.conn.unchecked_transaction()?)
    }

    /// The session by uuid; `None` for unknown sessions and tombstones.
    pub fn session(&self, uuid: &str) -> DbResult<Option<SessionRow>> {
        let sql = format!("SELECT {SESSION_COLS} FROM sessions s WHERE s.uuid = ?1 AND s.attribution IS NOT ?2");
        Ok(self
            .conn
            .prepare_cached(&sql)?
            .query_row(params![uuid, TOMBSTONE], session_from_row)
            .optional()?)
    }

    /// Sessions matching `f`, newest `started_at` first, after `cursor`, at most `limit`
    /// (`None`: all).
    pub fn sessions(
        &self,
        f: &SessionFilter,
        cursor: Option<SessionCursor>,
        limit: Option<usize>,
    ) -> DbResult<Vec<SessionRow>> {
        let mut clauses = vec!["s.attribution IS NOT ?1".to_owned()];
        let mut binds: Vec<Value> = vec![Value::Text(TOMBSTONE.into())];
        let bind = |v: Value, binds: &mut Vec<Value>| {
            binds.push(v);
            format!("?{}", binds.len())
        };
        match f.live {
            Some(true) => clauses.push("s.state = 'live'".into()),
            Some(false) => clauses.push("s.state <> 'live'".into()),
            None => {}
        }
        for (col, v) in [("s.host", &f.host), ("s.user", &f.user), ("s.tmux_pane", &f.pane)] {
            if let Some(v) = v {
                let p = bind(Value::Text(v.clone()), &mut binds);
                clauses.push(format!("{col} = {p}"));
            }
        }
        if let Some(v) = f.started_from {
            let p = bind(Value::Real(v), &mut binds);
            clauses.push(format!("s.started_at >= {p}"));
        }
        if let Some(v) = f.started_to {
            let p = bind(Value::Real(v), &mut binds);
            clauses.push(format!("s.started_at <= {p}"));
        }
        if let Some(v) = f.active_to {
            let p = bind(Value::Real(v), &mut binds);
            clauses.push(format!("s.started_at <= {p}"));
        }
        if let Some(v) = f.active_from {
            let p = bind(Value::Real(v), &mut binds);
            clauses.push(format!("(s.ended_at IS NULL OR s.ended_at >= {p})"));
        }
        if let Some(v) = f.legacy {
            clauses.push(format!("s.legacy = {}", i32::from(v)));
        }
        if !f.include_hidden {
            clauses.push("s.hidden = 0".into());
        }
        if !f.exclude.is_empty() {
            let list: Vec<String> = f
                .exclude
                .iter()
                .map(|u| bind(Value::Text(u.clone()), &mut binds))
                .collect();
            clauses.push(format!("s.uuid NOT IN ({})", list.join(", ")));
        }
        if let Some(c) = cursor {
            let a = bind(Value::Real(c.started_at), &mut binds);
            let b = bind(Value::Integer(c.id), &mut binds);
            clauses.push(format!("(s.started_at < {a} OR (s.started_at = {a} AND s.id < {b}))"));
        }
        let limit = limit.map_or(String::new(), |n| format!(" LIMIT {n}"));
        let sql = format!(
            "SELECT {SESSION_COLS} FROM sessions s WHERE {} ORDER BY s.started_at DESC, s.id DESC{limit}",
            clauses.join(" AND ")
        );
        let mut st = self.conn.prepare(&sql)?;
        let rows = st.query_map(params_from_iter(binds), session_from_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Direct children (`parent_uuid = uuid`), oldest first.
    pub fn children(&self, uuid: &str) -> DbResult<Vec<SessionRow>> {
        let sql = format!(
            "SELECT {SESSION_COLS} FROM sessions s WHERE s.parent_uuid = ?1 AND s.attribution IS NOT ?2
             ORDER BY s.started_at, s.id"
        );
        let mut st = self.conn.prepare_cached(&sql)?;
        let rows = st.query_map(params![uuid, TOMBSTONE], session_from_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// `(pane, tmux session name, legacy)` of the given sessions (search hit enrichment).
    pub fn hit_context(&self, uuids: &[&str]) -> DbResult<HashMap<String, HitContext>> {
        let mut out = HashMap::new();
        let mut st = self
            .conn
            .prepare_cached("SELECT tmux_pane, tmux_session, legacy FROM sessions WHERE uuid = ?1")?;
        for u in uuids {
            if out.contains_key(*u) {
                continue;
            }
            if let Some(v) = st
                .query_row([u], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
                .optional()?
            {
                out.insert((*u).to_owned(), v);
            }
        }
        Ok(out)
    }

    /// All commands of a session in `t_start` order.
    pub fn commands(&self, session_id: i64) -> DbResult<Vec<im::Command>> {
        let mut st = self.conn.prepare_cached(
            "SELECT seq, parent_seq, depth, shell_pid, t_start, t_end, ts_start, ts_end, exit, pipestatus, cwd, user,
                    euid, typed, expanded, typed_trunc, approx, confidence, out_bytes, out_truncated
             FROM commands WHERE session_id = ?1 ORDER BY t_start, id",
        )?;
        let rows = st.query_map([session_id], command_from_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn command(&self, session_id: i64, seq: u64) -> DbResult<Option<im::Command>> {
        Ok(self
            .conn
            .prepare_cached(
                "SELECT seq, parent_seq, depth, shell_pid, t_start, t_end, ts_start, ts_end, exit, pipestatus, cwd,
                        user, euid, typed, expanded, typed_trunc, approx, confidence, out_bytes, out_truncated
                 FROM commands WHERE session_id = ?1 AND seq = ?2",
            )?
            .query_row(params![session_id, i64_of(seq)], command_from_row)
            .optional()?)
    }

    pub fn markers(&self, session_id: i64) -> DbResult<Vec<im::MarkerRow>> {
        let mut st = self
            .conn
            .prepare_cached("SELECT t, ts, kind, data_json FROM markers WHERE session_id = ?1 ORDER BY t, id")?;
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

    /// `cast_parts` rows, part order (frames left `None`; see [`Self::frames`]).
    pub fn parts(&self, session_id: i64) -> DbResult<Vec<im::CastPart>> {
        let mut st = self.conn.prepare_cached(
            "SELECT part, path, compressed, bytes, t_offset FROM cast_parts WHERE session_id = ?1 ORDER BY part",
        )?;
        let rows = st.query_map([session_id], |r| {
            Ok(im::CastPart {
                part: r.get(0)?,
                path: path_at(r, 1)?,
                compressed: r.get(2)?,
                bytes: opt_u64(r, 3)?,
                t_offset: r.get(4)?,
                frames: None,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// `zst_frames` of one part.
    pub fn frames(&self, session_id: i64, part: u32) -> DbResult<Vec<im::ZstFrameRow>> {
        let mut st = self.conn.prepare_cached(
            "SELECT seq, c_offset, c_len, u_offset, u_len FROM zst_frames WHERE session_id = ?1 AND part = ?2
             ORDER BY seq",
        )?;
        let rows = st.query_map(params![session_id, part], |r| {
            Ok(im::ZstFrameRow {
                seq: r.get(0)?,
                c_offset: u64_at(r, 1)?,
                c_len: u64_at(r, 2)?,
                u_offset: u64_at(r, 3)?,
                u_len: u64_at(r, 4)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Playback segments, `seq` order.
    pub fn segments(&self, session_id: i64) -> DbResult<Vec<im::Segment>> {
        let mut st = self.conn.prepare_cached(
            "SELECT seq, part, t_start, t_end, byte_offset, vt_dump FROM segments WHERE session_id = ?1 ORDER BY seq",
        )?;
        let rows = st.query_map([session_id], |r| {
            Ok(im::Segment {
                seq: r.get(0)?,
                part: r.get(1)?,
                t_start: r.get(2)?,
                t_end: r.get(3)?,
                byte_offset: u64_at(r, 4)?,
                vt_dump: match r.get_ref(5)? {
                    ValueRef::Text(b) | ValueRef::Blob(b) => String::from_utf8_lossy(b).into_owned(),
                    _ => String::new(),
                },
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }
}

fn command_from_row(r: &Row<'_>) -> rusqlite::Result<im::Command> {
    let pipestatus: Option<String> = r.get(9)?;
    Ok(im::Command {
        seq: u64_at(r, 0)?,
        parent_seq: opt_u64(r, 1)?,
        depth: r.get(2)?,
        shell_pid: r.get(3)?,
        t_start: r.get(4)?,
        t_end: r.get(5)?,
        ts_start: r.get(6)?,
        ts_end: r.get(7)?,
        exit: r.get(8)?,
        pipestatus: pipestatus
            .and_then(|p| serde_json::from_str(&p).ok())
            .unwrap_or_default(),
        cwd: r.get(10)?,
        user: r.get(11)?,
        euid: r.get(12)?,
        typed: r.get(13)?,
        expanded: r.get(14)?,
        typed_trunc: r.get(15)?,
        approx: r.get(16)?,
        confidence: r.get(17)?,
        out_bytes: u64_at(r, 18)?,
        out_truncated: r.get(19)?,
    })
}

// ------------------------------------------------------------------ forgotten

/// The forgotten list of meta.db, by session (§7.6). Queries exclude what it names before
/// the serve daemon has purged the rows from index.db.
#[derive(Debug, Clone, Default)]
pub(crate) struct Forgets {
    by_uuid: HashMap<String, Vec<im::Forgotten>>,
    /// Recorded playback spans of forgotten commands, by (uuid, seq).
    spans: HashMap<(String, u64), CmdSpan>,
}

/// What playback and export hide of a forgotten command, in session seconds
/// (`play::session::command_span`); `to = None` while it ran.
///
/// It is kept in meta.db `settings` under [`span_key`] as `"<from> <to>"` (`-` for an open
/// end), so it outlives the command's row in index.db, which the serve daemon deletes. It is
/// deliberately not a `range` row of `meta.forgotten`: the index's range purge would also
/// blank unattributed output and the output of leading-space commands in that span.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct CmdSpan {
    pub from: f64,
    pub to: Option<f64>,
}

/// meta.db `settings` keys of [`CmdSpan`]s: `forget_span/<uuid>/<seq>`.
pub(crate) const SPAN_PREFIX: &str = "forget_span/";

pub(crate) fn span_key(uuid: &str, seq: u64) -> String {
    format!("{SPAN_PREFIX}{uuid}/{seq}")
}

impl CmdSpan {
    fn encode(self) -> String {
        match self.to {
            Some(to) => format!("{} {to}", self.from),
            None => format!("{} -", self.from),
        }
    }

    fn decode(v: &str) -> Option<Self> {
        let (from, to) = v.split_once(' ')?;
        let from: f64 = from.parse().ok().filter(|f: &f64| f.is_finite())?;
        let to = match to {
            "-" => None,
            t => Some(t.parse().ok().filter(|t: &f64| t.is_finite() && *t >= from)?),
        };
        Some(Self { from, to })
    }
}

/// `(uuid, seq)` of a [`span_key`].
fn parse_span_key(key: &str) -> Option<(String, u64)> {
    let (uuid, seq) = key.strip_prefix(SPAN_PREFIX)?.rsplit_once('/')?;
    Some((uuid.to_owned(), seq.parse().ok()?))
}

/// Records the playback span of forgotten command `seq`.
pub(crate) fn record_span(meta: &mut dyn MetaStore, uuid: &str, seq: u64, span: CmdSpan) -> DbResult<()> {
    meta.set_setting(&span_key(uuid, seq), Some(&span.encode()))
}

impl Forgets {
    /// Reads meta.db read-only; a missing or never initialised meta.db forgets nothing.
    pub fn load(ctx: &WebContext) -> Result<Self> {
        let (list, spans) = read_forgotten(&ctx.db)?;
        let mut me = Self::new(list);
        me.spans = spans;
        Ok(me)
    }

    pub fn new(list: Vec<im::Forgotten>) -> Self {
        let mut by_uuid: HashMap<String, Vec<im::Forgotten>> = HashMap::new();
        for f in list {
            by_uuid.entry(f.target.uuid().to_owned()).or_default().push(f);
        }
        Self {
            by_uuid,
            spans: HashMap::new(),
        }
    }

    /// The recorded playback span of forgotten command `seq`.
    pub fn span(&self, uuid: &str, seq: u64) -> Option<CmdSpan> {
        self.spans.get(&(uuid.to_owned(), seq)).copied()
    }

    pub fn rows(&self, uuid: &str) -> &[im::Forgotten] {
        self.by_uuid.get(uuid).map_or(&[], Vec::as_slice)
    }

    pub fn session(&self, uuid: &str) -> bool {
        self.rows(uuid)
            .iter()
            .any(|f| matches!(f.target, im::ForgetTarget::Session { .. }))
    }

    /// Every forgotten session.
    pub fn sessions(&self) -> Vec<String> {
        let mut v: Vec<String> = self.by_uuid.keys().filter(|u| self.session(u)).cloned().collect();
        v.sort();
        v
    }

    /// Forgotten command seqs of a session.
    pub fn command_seqs(&self, uuid: &str) -> impl Iterator<Item = u64> + '_ {
        self.rows(uuid).iter().filter_map(|f| match f.target {
            im::ForgetTarget::Command { seq, .. } => Some(seq),
            _ => None,
        })
    }

    /// Inclusive ranges in session seconds, with who forgot them.
    pub fn ranges(&self, uuid: &str) -> impl Iterator<Item = (f64, f64, &str)> + '_ {
        self.rows(uuid).iter().filter_map(|f| match f.target {
            im::ForgetTarget::Range { t_from, t_to, .. } => Some((t_from, t_to, f.by.as_str())),
            _ => None,
        })
    }

    /// The index rule for commands: forgotten by seq, with its session, or when it starts
    /// and ends inside a forgotten range.
    pub fn command(&self, uuid: &str, c: &im::Command) -> bool {
        self.session(uuid)
            || self.command_seqs(uuid).any(|s| s == c.seq)
            || c.t_end
                .is_some_and(|end| self.ranges(uuid).any(|(a, b, _)| c.t_start >= a && end <= b))
    }

    /// The existing row with exactly this target.
    pub fn find(&self, target: &im::ForgetTarget) -> Option<&im::Forgotten> {
        self.rows(target.uuid()).iter().find(|f| f.target == *target)
    }

    /// Identity of everything forgotten in a session (cheap staleness check of caches).
    pub fn fingerprint(&self, uuid: &str) -> String {
        let mut out = String::new();
        for f in self.rows(uuid) {
            let _ = match &f.target {
                im::ForgetTarget::Session { .. } => write!(out, "s;"),
                im::ForgetTarget::Command { seq, .. } => write!(out, "c{seq};"),
                im::ForgetTarget::Range { t_from, t_to, .. } => write!(out, "r{t_from}-{t_to};"),
            };
        }
        out
    }
}

type SpanMap = HashMap<(String, u64), CmdSpan>;

/// The forgotten rows and the recorded command spans.
fn read_forgotten(paths: &DbPaths) -> DbResult<(Vec<im::Forgotten>, SpanMap)> {
    if !paths.meta.exists() {
        return Ok(Default::default());
    }
    let conn = match schema::open(&paths.meta, DbKind::Meta, Access::ReadOnly) {
        Ok(c) => c,
        Err(IndexError::SchemaMismatch { found: 0, .. }) => return Ok(Default::default()),
        Err(e) => return Err(e),
    };
    let rows = forgotten_rows(&conn)?;
    // `0` follows `/`: the range is exactly the keys under the prefix.
    let mut st = conn.prepare("SELECT key, value FROM settings WHERE key >= ?1 AND key < ?2")?;
    let end = format!("{}0", SPAN_PREFIX.trim_end_matches('/'));
    let mut spans = HashMap::new();
    let found = st.query_map(params![SPAN_PREFIX, end], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?))
    })?;
    for kv in found {
        let (k, v) = kv?;
        if let (Some(key), Some(span)) = (parse_span_key(&k), v.as_deref().and_then(CmdSpan::decode)) {
            spans.insert(key, span);
        }
    }
    Ok((rows, spans))
}

fn forgotten_rows(conn: &Connection) -> DbResult<Vec<im::Forgotten>> {
    let mut st = conn.prepare("SELECT session_uuid, kind, cmd_seq, t_from, t_to, by, at FROM forgotten ORDER BY id")?;
    let rows = st.query_map([], |r| {
        let uuid: String = r.get(0)?;
        let kind: String = r.get(1)?;
        let target = match kind.as_str() {
            "command" => im::ForgetTarget::Command {
                uuid,
                seq: u64_at(r, 2)?,
            },
            "range" => im::ForgetTarget::Range {
                uuid,
                t_from: r.get(3)?,
                t_to: r.get(4)?,
            },
            _ => im::ForgetTarget::Session { uuid },
        };
        Ok(im::Forgotten {
            target,
            by: r.get(5)?,
            at: r.get(6)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_cursor_roundtrip() {
        let c = SessionCursor {
            started_at: 1_731_070_000.123456,
            id: 42,
        };
        assert_eq!(SessionCursor::decode(&c.encode()).unwrap(), c);
        for bad in ["", "x", "1:", ":2", "inf:3", "1.5:x"] {
            assert!(SessionCursor::decode(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn uuids_are_normalised() {
        assert_eq!(
            parse_uuid(" 0193A1B2-0000-7000-8000-000000000001 ").unwrap(),
            "0193a1b2-0000-7000-8000-000000000001"
        );
        assert!(parse_uuid("../../etc/passwd").is_err());
    }

    #[test]
    fn command_spans_roundtrip() {
        for span in [
            CmdSpan {
                from: 0.0,
                to: Some(12.345678),
            },
            CmdSpan { from: 1.1, to: None },
        ] {
            assert_eq!(CmdSpan::decode(&span.encode()), Some(span));
        }
        for bad in ["", "1", "x 2", "2 1", "1 inf", "NaN -"] {
            assert_eq!(CmdSpan::decode(bad), None, "{bad}");
        }
        let key = span_key("0193a1b2-0000-7000-8000-000000000001", 7);
        assert_eq!(key, "forget_span/0193a1b2-0000-7000-8000-000000000001/7");
        assert_eq!(
            parse_span_key(&key),
            Some(("0193a1b2-0000-7000-8000-000000000001".into(), 7))
        );
        assert_eq!(parse_span_key("port"), None);
    }

    #[test]
    fn forget_rules() {
        let u = "u";
        let f = |target| im::Forgotten {
            target,
            by: "alice".into(),
            at: 1.0,
        };
        let set = Forgets::new(vec![
            f(im::ForgetTarget::Command { uuid: u.into(), seq: 2 }),
            f(im::ForgetTarget::Range {
                uuid: u.into(),
                t_from: 10.0,
                t_to: 20.0,
            }),
        ]);
        let cmd = |seq, t_start, t_end| im::Command {
            seq,
            parent_seq: None,
            depth: 0,
            shell_pid: None,
            t_start,
            t_end,
            ts_start: t_start,
            ts_end: t_end,
            exit: None,
            pipestatus: vec![],
            cwd: None,
            user: None,
            euid: None,
            typed: String::new(),
            expanded: None,
            typed_trunc: false,
            approx: false,
            confidence: None,
            out_bytes: 0,
            out_truncated: false,
        };
        assert!(!set.session(u));
        assert!(set.command(u, &cmd(2, 0.0, Some(1.0))));
        assert!(set.command(u, &cmd(3, 11.0, Some(19.0))));
        assert!(!set.command(u, &cmd(4, 11.0, Some(21.0))), "runs past the range");
        assert!(!set.command(u, &cmd(5, 11.0, None)), "still running");
        assert!(!set.command("other", &cmd(2, 0.0, Some(1.0))));
    }
}
