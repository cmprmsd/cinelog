//! The single index.db writer (§6.4): one transaction per batch, upserts that keep the FTS
//! triggers firing (never `INSERT OR REPLACE`), forgotten rows dropped on the way in.

use std::borrow::Cow;
use std::collections::HashMap;
use std::fs::{DirBuilder, File, OpenOptions};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::Path;

use nix::errno::Errno;
use nix::fcntl::{Flock, FlockArg};
use regex::Regex;
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};

use super::forget::{BLANK_INSIDE, BLANK_STRADDLING, Echo, ForgetSet};
use super::{
    LEGACY_SELECT, TOMBSTONE, U64, ino_from_db, ino_to_db, legacy_from_row, now, opt_u64, opt_u64_at, parse_at,
    path_at, path_value, read_meta_forgotten, u64_at, utf8_at,
};
use crate::api::IndexWriter;
use crate::error::{IndexError, Result};
use crate::model::{
    Checkpoint, Chunk, CompressCandidate, CompressionUpdate, DbPaths, FileStamp, ForgetOutcome, ForgetTarget,
    Forgotten, IndexBatch, KnownCast, LegacyImportRow, Seq, SessionCounters, SessionMeta, SessionState, TmuxLocation,
    TranscriptState, info_keys,
};
use crate::schema::{self, Access, DbKind};
use cinelog_core::config::IndexConfig;

/// Optional, user-configured `index.redact` patterns (§15.7: off by default in both scopes,
/// no default set). Matches are replaced before insert; the casts are never touched.
#[derive(Debug, Default)]
pub(crate) struct Redactor {
    patterns: Vec<Regex>,
}

pub(crate) const REDACTED: &str = "[redacted]";

impl Redactor {
    pub(crate) fn new(patterns: &[String]) -> Result<Self> {
        let patterns = patterns
            .iter()
            .map(|p| Regex::new(p).map_err(|e| IndexError::Invalid(format!("index.redact pattern {p:?}: {e}"))))
            .collect::<Result<_>>()?;
        Ok(Self { patterns })
    }

    pub(crate) fn apply<'a>(&self, text: &'a str) -> Cow<'a, str> {
        let mut out = Cow::Borrowed(text);
        for re in &self.patterns {
            if let Cow::Owned(s) = re.replace_all(&out, REDACTED) {
                out = Cow::Owned(s);
            }
        }
        out
    }
}

pub(crate) struct SqliteWriter {
    // Declared before the lock so the connection closes before the flock is released.
    conn: Connection,
    forget: ForgetSet,
    redact: Redactor,
    _lock: Flock<File>,
}

/// Takes the non-blocking exclusive flock on `index.db.writer.lock`.
fn lock_writer(path: &Path) -> Result<Flock<File>> {
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
    }
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .mode(0o600)
        .open(path)?;
    match Flock::lock(file, FlockArg::LockExclusiveNonblock) {
        Ok(lock) => Ok(lock),
        Err((_, Errno::EWOULDBLOCK)) => Err(IndexError::Locked(path.to_owned())),
        Err((_, e)) => Err(std::io::Error::from(e).into()),
    }
}

impl SqliteWriter {
    pub(crate) fn open(paths: &DbPaths, cfg: &IndexConfig) -> Result<Self> {
        let redact = Redactor::new(&cfg.redact)?;
        let lock = lock_writer(&paths.writer_lock)?;
        let conn = schema::open(&paths.index, DbKind::Index, Access::ReadWrite)?;
        // Until serve hands over its list, honour what meta.db already says, so a rebuild
        // never brings forgotten content back.
        let forgotten = read_meta_forgotten(&paths.meta)?.map(|(_, l)| l).unwrap_or_default();
        let w = Self {
            conn,
            forget: ForgetSet::new(&forgotten),
            redact,
            _lock: lock,
        };
        set_info(&w.conn, info_keys::WRITER_PID, Some(&std::process::id().to_string()))?;
        w.conn.execute(
            "INSERT INTO index_info(key, value) VALUES(?1, ?2) ON CONFLICT(key) DO NOTHING",
            [info_keys::BUILT_VERSION, env!("CARGO_PKG_VERSION")],
        )?;
        Ok(w)
    }

    fn immediate(&mut self) -> Result<Transaction<'_>> {
        Ok(self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?)
    }
}

fn set_info(conn: &Connection, key: &str, value: Option<&str>) -> Result<()> {
    match value {
        Some(v) => conn
            .prepare_cached(
                "INSERT INTO index_info(key, value) VALUES(?1, ?2) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            )?
            .execute([key, v])?,
        None => conn
            .prepare_cached("DELETE FROM index_info WHERE key = ?1")?
            .execute([key])?,
    };
    Ok(())
}

fn session_id(conn: &Connection, uuid: &str) -> Result<Option<i64>> {
    Ok(conn
        .prepare_cached("SELECT id FROM sessions WHERE uuid = ?1")?
        .query_row([uuid], |r| r.get(0))
        .optional()?)
}

fn upsert_session(tx: &Transaction<'_>, s: &SessionMeta) -> Result<i64> {
    // The file at this path now belongs to another session (cast replaced): its rows are stale.
    tx.prepare_cached("DELETE FROM sessions WHERE cast_path = ?1 AND uuid <> ?2")?
        .execute(params![path_value(&s.cast_path), s.uuid])?;
    let id = tx
        .prepare_cached(
            "INSERT INTO sessions(uuid, host, scope, user, euid, sudo_user, claimed_user, attribution, login_via, tty,
               holder_comm, tmux_server, tmux_session_id, tmux_session, tmux_window, tmux_window_name, tmux_pane,
               parent_uuid, cast_path, parts, compressed, cast_bytes, started_at, ended_at, exit_status, tz, cols, rows,
               term, legacy, hidden, state, bad_lines, bad_markers, lossy_bytes)
             VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22,
               ?23, ?24, ?25, ?26, ?27, ?28, ?29, ?30, ?31, ?32, ?33, ?34, ?35)
             ON CONFLICT(uuid) DO UPDATE SET host = excluded.host, scope = excluded.scope, user = excluded.user,
               euid = excluded.euid, sudo_user = excluded.sudo_user, claimed_user = excluded.claimed_user,
               attribution = excluded.attribution, login_via = excluded.login_via, tty = excluded.tty,
               holder_comm = excluded.holder_comm, tmux_server = excluded.tmux_server,
               tmux_session_id = excluded.tmux_session_id, tmux_session = excluded.tmux_session,
               tmux_window = excluded.tmux_window, tmux_window_name = excluded.tmux_window_name,
               tmux_pane = excluded.tmux_pane, parent_uuid = excluded.parent_uuid, cast_path = excluded.cast_path,
               parts = excluded.parts, compressed = excluded.compressed, cast_bytes = excluded.cast_bytes,
               started_at = excluded.started_at, ended_at = excluded.ended_at, exit_status = excluded.exit_status,
               tz = excluded.tz, cols = excluded.cols, rows = excluded.rows, term = excluded.term,
               legacy = excluded.legacy, hidden = excluded.hidden, state = excluded.state,
               bad_lines = excluded.bad_lines, bad_markers = excluded.bad_markers, lossy_bytes = excluded.lossy_bytes
             RETURNING id",
        )?
        .query_row(
            params![
                s.uuid,
                s.host,
                s.scope.as_str(),
                s.user,
                s.euid,
                s.sudo_user,
                s.claimed_user,
                s.attribution,
                s.login_via,
                s.tty,
                s.holder_comm,
                s.tmux.server,
                s.tmux.session_id,
                s.tmux.session,
                s.tmux.window,
                s.tmux.window_name,
                s.tmux.pane,
                s.parent_uuid,
                path_value(&s.cast_path),
                s.parts,
                s.compressed,
                opt_u64(s.cast_bytes),
                s.started_at,
                s.ended_at,
                s.exit_status,
                s.tz,
                s.cols,
                s.rows,
                s.term,
                s.legacy,
                s.hidden,
                s.state.as_str(),
                U64(s.counters.bad_lines),
                U64(s.counters.bad_markers),
                U64(s.counters.lossy_bytes),
            ],
            |r| r.get(0),
        )?;
    Ok(id)
}

/// Every derived row of a session except the session row itself.
fn reset_session(tx: &Transaction<'_>, sid: i64) -> Result<()> {
    for table in [
        "outputs",
        "commands",
        "markers",
        "segments",
        "zst_frames",
        "cast_parts",
        "index_state",
    ] {
        tx.prepare_cached(&format!("DELETE FROM {table} WHERE session_id = ?1"))?
            .execute([sid])?;
    }
    Ok(())
}

/// The session row of a forgotten session's tombstone: what discovery needs to see the cast
/// as handled (uuid, files, state), nothing of who ran it or where.
fn tombstone_meta(s: &SessionMeta) -> SessionMeta {
    SessionMeta {
        user: String::new(),
        euid: None,
        sudo_user: None,
        claimed_user: None,
        attribution: Some(TOMBSTONE.into()),
        login_via: None,
        tty: None,
        holder_comm: None,
        tmux: TmuxLocation::default(),
        parent_uuid: None,
        exit_status: None,
        tz: None,
        term: None,
        hidden: true,
        counters: SessionCounters::default(),
        ..s.clone()
    }
}

/// A checkpoint without terminal or pipeline state (both hold session content): it records
/// how far the cast was read, and a pipeline cannot resume from it.
fn tombstone_checkpoint(cp: &Checkpoint) -> Checkpoint {
    Checkpoint {
        transcript: TranscriptState::default(),
        open_stack: Vec::new(),
        pipeline_state: String::new(),
        ..cp.clone()
    }
}

/// Turns session `sid` into its tombstone (see [`TOMBSTONE`]): deletes its content rows and
/// strips the session row and checkpoint, keeping the parts, frame table and read position.
/// Returns whether it was not a tombstone before.
fn entomb(tx: &Transaction<'_>, sid: i64) -> Result<bool> {
    let fresh: bool = tx
        .prepare_cached("SELECT attribution IS NOT ?2 FROM sessions WHERE id = ?1")?
        .query_row(params![sid, TOMBSTONE], |r| r.get(0))?;
    for table in ["outputs", "commands", "markers", "segments"] {
        tx.prepare_cached(&format!("DELETE FROM {table} WHERE session_id = ?1"))?
            .execute([sid])?;
    }
    tx.prepare_cached(
        "UPDATE sessions SET user = '', euid = NULL, sudo_user = NULL, claimed_user = NULL, attribution = ?2,
           login_via = NULL, tty = NULL, holder_comm = NULL, tmux_server = NULL, tmux_session_id = NULL,
           tmux_session = NULL, tmux_window = NULL, tmux_window_name = NULL, tmux_pane = NULL, parent_uuid = NULL,
           exit_status = NULL, tz = NULL, term = NULL, hidden = 1, bad_lines = 0, bad_markers = 0, lossy_bytes = 0
         WHERE id = ?1",
    )?
    .execute(params![sid, TOMBSTONE])?;
    tx.prepare_cached(
        "UPDATE index_state SET vt_dump = NULL, evicted = 0, open_stack = NULL, state_json = NULL
         WHERE session_id = ?1",
    )?
    .execute([sid])?;
    Ok(fresh)
}

fn upsert_command(tx: &Transaction<'_>, sid: i64, c: &crate::model::Command, redact: &Redactor) -> Result<i64> {
    let pipestatus = (!c.pipestatus.is_empty())
        .then(|| serde_json::to_string(&c.pipestatus))
        .transpose()?;
    let typed = redact.apply(&c.typed);
    let expanded = c.expanded.as_deref().map(|e| redact.apply(e));
    Ok(tx
        .prepare_cached(
            "INSERT INTO commands(session_id, seq, parent_seq, depth, shell_pid, t_start, t_end, ts_start, ts_end, exit,
               pipestatus, cwd, user, euid, typed, expanded, typed_trunc, approx, confidence, out_bytes, out_truncated)
             VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21)
             ON CONFLICT(session_id, seq) DO UPDATE SET parent_seq = excluded.parent_seq, depth = excluded.depth,
               shell_pid = excluded.shell_pid, t_start = excluded.t_start, t_end = excluded.t_end,
               ts_start = excluded.ts_start, ts_end = excluded.ts_end, exit = excluded.exit,
               pipestatus = excluded.pipestatus, cwd = excluded.cwd, user = excluded.user, euid = excluded.euid,
               typed = excluded.typed, expanded = excluded.expanded, typed_trunc = excluded.typed_trunc,
               approx = excluded.approx, confidence = excluded.confidence, out_bytes = excluded.out_bytes,
               out_truncated = excluded.out_truncated
             RETURNING id",
        )?
        .query_row(
            params![
                sid,
                U64(c.seq),
                opt_u64(c.parent_seq),
                c.depth,
                c.shell_pid,
                c.t_start,
                c.t_end,
                c.ts_start,
                c.ts_end,
                c.exit,
                pipestatus,
                c.cwd,
                c.user,
                c.euid,
                typed,
                expanded,
                c.typed_trunc,
                c.approx,
                c.confidence,
                U64(c.out_bytes),
                c.out_truncated,
            ],
            |r| r.get(0),
        )?)
}

fn command_id(tx: &Transaction<'_>, sid: i64, seq: Seq, cache: &mut HashMap<Seq, Option<i64>>) -> Result<Option<i64>> {
    if let Some(id) = cache.get(&seq) {
        return Ok(*id);
    }
    let id = tx
        .prepare_cached("SELECT id FROM commands WHERE session_id = ?1 AND seq = ?2")?
        .query_row(params![sid, U64(seq)], |r| r.get(0))
        .optional()?;
    cache.insert(seq, id);
    Ok(id)
}

/// Inserts a chunk; `blank` stores it without text (forgotten by range, see `forget`).
fn insert_chunk(
    tx: &Transaction<'_>,
    sid: i64,
    cmd: Option<i64>,
    ch: &Chunk,
    redact: &Redactor,
    blank: bool,
) -> Result<()> {
    let text = if blank {
        Cow::Borrowed("")
    } else {
        redact.apply(&ch.text)
    };
    tx.prepare_cached(
        "INSERT INTO outputs(session_id, command_id, t, ts, line0, kind, text) VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7)",
    )?
    .execute(params![sid, cmd, ch.t, ch.ts, U64(ch.line0), ch.kind.as_str(), text])?;
    Ok(())
}

/// Private JSON of `index_state.state_json`: the renderer's and the pipeline's state.
#[derive(serde::Serialize, serde::Deserialize, Default)]
struct StateJson {
    transcript: String,
    pipeline: String,
}

fn upsert_checkpoint(tx: &Transaction<'_>, sid: i64, cp: &Checkpoint) -> Result<()> {
    let state = serde_json::to_string(&StateJson {
        transcript: cp.transcript.state.clone(),
        pipeline: cp.pipeline_state.clone(),
    })?;
    tx.prepare_cached(
        "INSERT INTO index_state(session_id, part, byte_offset, t, vt_dump, evicted, open_stack, state_json,
           file_size, file_mtime, file_ino, updated_at)
         VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
         ON CONFLICT(session_id) DO UPDATE SET part = excluded.part, byte_offset = excluded.byte_offset,
           t = excluded.t, vt_dump = excluded.vt_dump, evicted = excluded.evicted, open_stack = excluded.open_stack,
           state_json = excluded.state_json, file_size = excluded.file_size, file_mtime = excluded.file_mtime,
           file_ino = excluded.file_ino, updated_at = excluded.updated_at",
    )?
    .execute(params![
        sid,
        cp.part,
        U64(cp.byte_offset),
        cp.t,
        cp.transcript.vt_dump.as_bytes(),
        U64(cp.transcript.evicted),
        serde_json::to_string(&cp.open_stack)?,
        state,
        U64(cp.file.size),
        cp.file.mtime,
        ino_to_db(cp.file.ino),
        now(),
    ])?;
    Ok(())
}

fn upsert_legacy(conn: &Connection, l: &LegacyImportRow, sid: Option<i64>) -> Result<()> {
    conn.prepare_cached(
        "INSERT INTO legacy_import(path, size, mtime, ino, sha256, commands_log, status, session_id, banners,
           log_lines, aligned, mean_conf, error, updated_at)
         VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)
         ON CONFLICT(path) DO UPDATE SET size = excluded.size, mtime = excluded.mtime, ino = excluded.ino,
           sha256 = excluded.sha256, commands_log = excluded.commands_log, status = excluded.status,
           session_id = excluded.session_id, banners = excluded.banners, log_lines = excluded.log_lines,
           aligned = excluded.aligned, mean_conf = excluded.mean_conf, error = excluded.error,
           updated_at = excluded.updated_at",
    )?
    .execute(params![
        path_value(&l.path),
        U64(l.file.size),
        l.file.mtime,
        ino_to_db(l.file.ino),
        l.sha256,
        l.commands_log.as_deref().map(path_value),
        l.status.as_str(),
        sid,
        U64(l.banners),
        U64(l.log_lines),
        U64(l.aligned),
        l.mean_conf,
        l.error,
        now(),
    ])?;
    Ok(())
}

fn upsert_parts(tx: &Transaction<'_>, sid: i64, parts: &[crate::model::CastPart]) -> Result<()> {
    for p in parts {
        tx.prepare_cached(
            "INSERT INTO cast_parts(session_id, part, path, compressed, bytes, t_offset) VALUES(?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(session_id, part) DO UPDATE SET path = excluded.path, compressed = excluded.compressed,
               bytes = excluded.bytes, t_offset = excluded.t_offset",
        )?
        .execute(params![
            sid,
            p.part,
            path_value(&p.path),
            p.compressed,
            opt_u64(p.bytes),
            p.t_offset
        ])?;
        if let Some(frames) = &p.frames {
            replace_frames(tx, sid, p.part, frames)?;
        }
    }
    Ok(())
}

fn replace_frames(tx: &Transaction<'_>, sid: i64, part: u32, frames: &[crate::model::ZstFrameRow]) -> Result<()> {
    tx.prepare_cached("DELETE FROM zst_frames WHERE session_id = ?1 AND part = ?2")?
        .execute(params![sid, part])?;
    let mut ins = tx.prepare_cached(
        "INSERT INTO zst_frames(session_id, part, seq, c_offset, c_len, u_offset, u_len)
         VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7)",
    )?;
    for f in frames {
        ins.execute(params![
            sid,
            part,
            f.seq,
            U64(f.c_offset),
            U64(f.c_len),
            U64(f.u_offset),
            U64(f.u_len)
        ])?;
    }
    Ok(())
}

fn apply_batch(tx: &Transaction<'_>, forget: &ForgetSet, redact: &Redactor, b: &IndexBatch) -> Result<()> {
    let uuid = b.session.uuid.as_str();
    if uuid.is_empty() {
        return Err(IndexError::Invalid("batch without session uuid".into()));
    }
    if forget.session(uuid) {
        // Nothing of a forgotten session is stored, but its tombstone keeps how far the cast
        // was read, so discovery does not re-render it on every walk.
        let sid = upsert_session(tx, &tombstone_meta(&b.session))?;
        upsert_parts(tx, sid, &b.parts)?;
        if let Some(cp) = &b.checkpoint {
            upsert_checkpoint(tx, sid, &tombstone_checkpoint(cp))?;
        }
        entomb(tx, sid)?;
        if let Some(l) = &b.legacy {
            upsert_legacy(tx, l, Some(sid))?;
        }
        return Ok(());
    }
    let sid = upsert_session(tx, &b.session)?;
    if b.reset {
        reset_session(tx, sid)?;
    }
    upsert_parts(tx, sid, &b.parts)?;

    let mut ids: HashMap<Seq, Option<i64>> = HashMap::new();
    for c in &b.commands {
        if forget.command(uuid, c.seq, c.t_start, c.t_end) {
            // A command can become forgotten once it closes inside a forgotten range.
            tx.prepare_cached("DELETE FROM commands WHERE session_id = ?1 AND seq = ?2")?
                .execute(params![sid, U64(c.seq)])?;
            ids.insert(c.seq, None);
            continue;
        }
        let id = upsert_command(tx, sid, c, redact)?;
        ids.insert(c.seq, Some(id));
    }

    if b.provisional.is_some() || b.finished {
        tx.prepare_cached("DELETE FROM outputs WHERE session_id = ?1 AND kind = 'provisional'")?
            .execute([sid])?;
    }
    let provisional = if b.finished { None } else { b.provisional.as_deref() };
    let chunks: Vec<&Chunk> = b.chunks.iter().chain(provisional.into_iter().flatten()).collect();
    for ch in &chunks {
        if forget.owner(uuid, ch.cmd_seq) {
            continue;
        }
        let cmd = match ch.cmd_seq {
            None => None,
            Some(seq) => match command_id(tx, sid, seq, &mut ids)? {
                Some(id) => Some(id),
                None => {
                    // Its command is gone (forgotten by range) or was never sent.
                    tracing::debug!(uuid, seq, "dropping output chunk without its command");
                    continue;
                }
            },
        };
        insert_chunk(tx, sid, cmd, ch, redact, forget.in_range(uuid, ch.t))?;
    }
    // The echo of a command forgotten by seq arrives with it (rebuild, or forgotten live).
    for c in &b.commands {
        if forget.by_seq(uuid, c.seq) {
            Echo::new(tx, sid, None, c.seq, c.t_start, &redact.apply(&c.typed))?.purge(tx, sid)?;
        }
    }
    // New chunks, command ends or the session's end can show that a chunk before a range
    // runs into it.
    for &(t_from, _) in forget.ranges(uuid) {
        tx.prepare_cached(BLANK_STRADDLING)?.execute(params![sid, t_from])?;
    }

    for m in &b.markers {
        tx.prepare_cached("INSERT INTO markers(session_id, t, ts, kind, data_json) VALUES(?1, ?2, ?3, ?4, ?5)")?
            .execute(params![sid, m.t, m.ts, m.kind, m.data_json])?;
    }
    for s in &b.segments {
        tx.prepare_cached(
            "INSERT INTO segments(session_id, seq, part, t_start, t_end, byte_offset, vt_dump)
             VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(session_id, seq) DO UPDATE SET part = excluded.part, t_start = excluded.t_start,
               t_end = excluded.t_end, byte_offset = excluded.byte_offset, vt_dump = excluded.vt_dump",
        )?
        .execute(params![
            sid,
            s.seq,
            s.part,
            s.t_start,
            s.t_end,
            U64(s.byte_offset),
            s.vt_dump.as_bytes()
        ])?;
    }
    if let Some(cp) = &b.checkpoint {
        upsert_checkpoint(tx, sid, cp)?;
    }
    if let Some(l) = &b.legacy {
        upsert_legacy(tx, l, Some(sid))?;
    }
    Ok(())
}

fn count(tx: &Transaction<'_>, sql: &str, args: impl rusqlite::Params) -> Result<u64> {
    Ok(tx.prepare_cached(sql)?.query_row(args, |r| u64_at(r, 0))?)
}

/// Removes the rows of one forget target, adding what was removed to `out`: a session
/// becomes its tombstone, commands are deleted with their output and their echo, range-
/// forgotten chunks are blanked (see `forget`).
fn purge(tx: &Transaction<'_>, target: &ForgetTarget, out: &mut ForgetOutcome) -> Result<()> {
    let Some(sid) = session_id(tx, target.uuid())? else {
        return Ok(());
    };
    match *target {
        ForgetTarget::Session { .. } => {
            out.commands += count(tx, "SELECT count(*) FROM commands WHERE session_id = ?1", [sid])?;
            out.outputs += count(tx, "SELECT count(*) FROM outputs WHERE session_id = ?1", [sid])?;
            out.sessions += u64::from(entomb(tx, sid)?);
        }
        ForgetTarget::Command { seq, .. } => {
            if let Some(echo) = Echo::of_stored(tx, sid, seq)? {
                out.outputs += echo.purge(tx, sid)?;
            }
            out.outputs += count(
                tx,
                "SELECT count(*) FROM outputs o JOIN commands c ON c.id = o.command_id
                 WHERE c.session_id = ?1 AND c.seq = ?2",
                params![sid, U64(seq)],
            )?;
            out.commands += tx.execute(
                "DELETE FROM commands WHERE session_id = ?1 AND seq = ?2",
                params![sid, U64(seq)],
            )? as u64;
        }
        ForgetTarget::Range { t_from, t_to, .. } => {
            out.outputs += tx.prepare_cached(BLANK_STRADDLING)?.execute(params![sid, t_from])? as u64;
            let inside = "session_id = ?1 AND t_start >= ?2 AND t_end IS NOT NULL AND t_end <= ?3";
            out.outputs += count(
                tx,
                &format!(
                    "SELECT count(*) FROM outputs WHERE text <> '' AND command_id IN (SELECT id FROM commands WHERE {inside})"
                ),
                params![sid, t_from, t_to],
            )?;
            out.commands += tx.execute(
                &format!("DELETE FROM commands WHERE {inside}"),
                params![sid, t_from, t_to],
            )? as u64;
            out.outputs += tx.prepare_cached(BLANK_INSIDE)?.execute(params![sid, t_from, t_to])? as u64;
        }
    }
    Ok(())
}

/// Row of `known_casts`.
fn known_from_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<KnownCast> {
    let part: Option<u32> = r.get(7)?;
    Ok(KnownCast {
        uuid: r.get(0)?,
        cast_path: path_at(r, 1)?,
        legacy: r.get(2)?,
        state: parse_at(r, 3)?,
        parts: r.get(4)?,
        compressed: r.get(5)?,
        ended_at: r.get(6)?,
        indexed_part: part,
        indexed_offset: opt_u64_at(r, 8)?,
        indexed_file: match part {
            Some(_) => Some(FileStamp {
                size: opt_u64_at(r, 9)?.unwrap_or(0),
                mtime: r.get::<_, Option<f64>>(10)?.unwrap_or(0.0),
                ino: r.get::<_, Option<i64>>(11)?.map_or(0, ino_from_db),
            }),
            None => None,
        },
    })
}

impl IndexWriter for SqliteWriter {
    fn apply(&mut self, batch: &IndexBatch) -> Result<()> {
        let tx = self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        apply_batch(&tx, &self.forget, &self.redact, batch)?;
        set_info(&tx, info_keys::WRITER_HEARTBEAT, Some(&now().to_string()))?;
        tx.commit()?;
        Ok(())
    }

    fn apply_forgotten(&mut self, forgotten: &[Forgotten]) -> Result<ForgetOutcome> {
        let tx = self.immediate()?;
        let mut out = ForgetOutcome::default();
        for f in forgotten {
            purge(&tx, &f.target, &mut out)?;
        }
        tx.commit()?;
        self.forget = ForgetSet::new(forgotten);
        Ok(out)
    }

    fn record_legacy(&mut self, row: &LegacyImportRow) -> Result<()> {
        let tx = self.immediate()?;
        let sid = match &row.session_uuid {
            Some(u) => session_id(&tx, u)?,
            None => None,
        };
        upsert_legacy(&tx, row, sid)?;
        tx.commit()?;
        Ok(())
    }

    fn record_compression(&mut self, u: &CompressionUpdate) -> Result<()> {
        let tx = self.immediate()?;
        let sid = session_id(&tx, &u.uuid)?.ok_or_else(|| IndexError::NotFound(format!("session {}", u.uuid)))?;
        let updated = tx.execute(
            "UPDATE cast_parts SET path = ?3, compressed = 1, bytes = ?4 WHERE session_id = ?1 AND part = ?2",
            params![sid, u.part, path_value(&u.to), U64(u.uncompressed_bytes)],
        )?;
        if updated == 0 {
            // Single-part sessions may never have listed their part explicitly.
            let first: Option<std::path::PathBuf> = tx
                .query_row("SELECT cast_path FROM sessions WHERE id = ?1", [sid], |r| path_at(r, 0))
                .optional()?;
            if u.part != 1 || first.as_deref() != Some(u.from.as_path()) {
                return Err(IndexError::NotFound(format!("{} part {}", u.uuid, u.part)));
            }
            tx.execute(
                "INSERT INTO cast_parts(session_id, part, path, compressed, bytes, t_offset) VALUES(?1, 1, ?2, 1, ?3, 0)",
                params![sid, path_value(&u.to), U64(u.uncompressed_bytes)],
            )?;
        }
        if u.part == 1 {
            tx.execute(
                "UPDATE sessions SET cast_path = ?2 WHERE id = ?1",
                params![sid, path_value(&u.to)],
            )?;
        }
        tx.execute(
            "UPDATE sessions SET compressed = (
               (SELECT count(*) FROM cast_parts WHERE session_id = ?1 AND compressed = 0) = 0
               AND (SELECT count(*) FROM cast_parts WHERE session_id = ?1) >= parts)
             WHERE id = ?1",
            [sid],
        )?;
        replace_frames(&tx, sid, u.part, &u.frames)?;
        tx.commit()?;
        Ok(())
    }

    fn set_info(&mut self, key: &str, value: Option<&str>) -> Result<()> {
        set_info(&self.conn, key, value)
    }

    fn clear(&mut self) -> Result<()> {
        let tx = self.immediate()?;
        // Row-by-row FTS delete triggers would make a rebuild of a large index crawl: drop
        // them, empty the tables, reset the FTS indexes in one step and restore the triggers.
        let triggers: Vec<(String, String)> = tx
            .prepare(
                "SELECT name, sql FROM sqlite_schema WHERE type = 'trigger' AND tbl_name IN ('commands', 'outputs')",
            )?
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?;
        for (name, _) in &triggers {
            tx.execute_batch(&format!("DROP TRIGGER \"{name}\""))?;
        }
        tx.execute_batch(
            "DELETE FROM outputs; DELETE FROM commands; DELETE FROM markers; DELETE FROM segments;
             DELETE FROM zst_frames; DELETE FROM cast_parts; DELETE FROM index_state; DELETE FROM legacy_import;
             DELETE FROM sessions;
             INSERT INTO out_fts(out_fts) VALUES('delete-all');
             INSERT INTO cmd_fts(cmd_fts) VALUES('delete-all');",
        )?;
        for (_, sql) in &triggers {
            tx.execute_batch(sql)?;
        }
        tx.commit()?;
        Ok(())
    }

    fn known_casts(&self) -> Result<Vec<KnownCast>> {
        let mut st = self.conn.prepare_cached(
            "SELECT s.uuid, s.cast_path, s.legacy, s.state, s.parts, s.compressed, s.ended_at,
               i.part, i.byte_offset, i.file_size, i.file_mtime, i.file_ino
             FROM sessions s LEFT JOIN index_state i ON i.session_id = s.id ORDER BY s.id",
        )?;
        let rows = st.query_map([], known_from_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    fn checkpoint(&self, uuid: &str) -> Result<Option<Checkpoint>> {
        let row = self
            .conn
            .prepare_cached(
                "SELECT i.part, i.byte_offset, i.t, i.vt_dump, i.evicted, i.open_stack, i.state_json,
                   i.file_size, i.file_mtime, i.file_ino
                 FROM index_state i JOIN sessions s ON s.id = i.session_id WHERE s.uuid = ?1",
            )?
            .query_row([uuid], |r| {
                Ok((
                    r.get::<_, u32>(0)?,
                    u64_at(r, 1)?,
                    r.get::<_, f64>(2)?,
                    utf8_at(r, 3)?,
                    u64_at(r, 4)?,
                    r.get::<_, Option<String>>(5)?,
                    r.get::<_, Option<String>>(6)?,
                    FileStamp {
                        size: opt_u64_at(r, 7)?.unwrap_or(0),
                        mtime: r.get::<_, Option<f64>>(8)?.unwrap_or(0.0),
                        ino: r.get::<_, Option<i64>>(9)?.map_or(0, ino_from_db),
                    },
                ))
            })
            .optional()?;
        let Some((part, byte_offset, t, vt_dump, evicted, stack, state, file)) = row else {
            return Ok(None);
        };
        let state: StateJson = state.map(|s| serde_json::from_str(&s)).transpose()?.unwrap_or_default();
        Ok(Some(Checkpoint {
            part,
            byte_offset,
            t,
            transcript: TranscriptState {
                vt_dump,
                evicted,
                state: state.transcript,
            },
            open_stack: stack.map(|s| serde_json::from_str(&s)).transpose()?.unwrap_or_default(),
            pipeline_state: state.pipeline,
            file,
        }))
    }

    fn legacy_entry(&self, path: &Path) -> Result<Option<LegacyImportRow>> {
        Ok(self
            .conn
            .prepare_cached(&format!("{LEGACY_SELECT} WHERE l.path = ?1"))?
            .query_row([path_value(path)], legacy_from_row)
            .optional()?)
    }

    fn compress_candidates(&self, ended_before: f64) -> Result<Vec<CompressCandidate>> {
        let finished = format!(
            "s.state IN ('{}', '{}') AND s.legacy = 0 AND s.ended_at IS NOT NULL AND s.ended_at < ?1",
            SessionState::Ended,
            SessionState::Crashed
        );
        let sql = format!(
            "SELECT s.uuid, p.part, p.path, s.ended_at FROM cast_parts p JOIN sessions s ON s.id = p.session_id
               WHERE {finished} AND p.compressed = 0
             UNION ALL
             SELECT s.uuid, 1, s.cast_path, s.ended_at FROM sessions s
               WHERE {finished} AND s.compressed = 0
                 AND NOT EXISTS (SELECT 1 FROM cast_parts p WHERE p.session_id = s.id)
             ORDER BY 4, 1, 2"
        );
        let mut st = self.conn.prepare_cached(&sql)?;
        let rows = st.query_map([ended_before], |r| {
            Ok(CompressCandidate {
                uuid: r.get(0)?,
                part: r.get(1)?,
                path: path_at(r, 2)?,
                ended_at: r.get(3)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }
}
