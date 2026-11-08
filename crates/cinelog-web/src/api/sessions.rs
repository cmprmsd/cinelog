//! Status, session list and detail, commands, the import report and forget
//! (docs/API.md §5.1, §5.2, §5.6).

use std::path::Path;

use cinelog_index::model as im;
use serde_json::Value;

use super::db::{CmdSpan, Forgets, IndexDb, SessionCursor, SessionFilter, SessionRow, record_span};
use super::search::limit;
use super::types::{
    self as t, CastPart, Command, CommandList, ForgetRequest, ImportFile, ImportReport, Marker, MaskedSpan,
    SessionDetail, SessionPage, SessionParams, SessionRef, Status,
};
use crate::error::{Result, WebError};
use crate::state::{AppState, WebContext};

const DEFAULT_SESSIONS: u32 = 100;
const MAX_SESSIONS: u32 = 500;

pub(crate) fn status(st: &AppState) -> Result<Status> {
    let ctx = &st.ctx;
    let s = if ctx.db.index.exists() {
        super::db::searcher(ctx)?.status()?
    } else {
        im::IndexStatus {
            last_error: Some("the index has not been built yet".into()),
            ..im::IndexStatus::default()
        }
    };
    Ok(Status {
        version: crate::VERSION.to_owned(),
        protocol: cinelog_proto::PROTOCOL_VERSION,
        scope: ctx.scope.into(),
        host: ctx.host.clone(),
        server_started_at: st.started_at,
        port: st.bound.port,
        fingerprint: st.bound.fingerprint.clone(),
        schema_version: s.schema_version,
        index_bytes: s.index_bytes,
        sessions: s.sessions.into(),
        commands: s.commands,
        outputs: s.outputs,
        live: s
            .live
            .into_iter()
            .map(|l| t::LiveLag {
                session: l.uuid,
                indexed_bytes: l.indexed_bytes,
                file_bytes: l.file_bytes,
                updated_at: l.updated_at,
            })
            .collect(),
        backlog_bytes: s.backlog_bytes,
        legacy: s.legacy.into(),
        last_error: s.last_error.map(|e| e.lines().next().unwrap_or_default().to_owned()),
    })
}

fn finite(v: Option<f64>, name: &str) -> Result<Option<f64>> {
    match v {
        Some(x) if !x.is_finite() => Err(WebError::BadRequest(format!("{name} must be a finite number"))),
        v => Ok(v),
    }
}

pub(crate) fn list(ctx: &WebContext, p: &SessionParams) -> Result<SessionPage> {
    let limit = limit(p.limit, DEFAULT_SESSIONS, MAX_SESSIONS)?;
    let cursor = p.cursor.as_deref().map(SessionCursor::decode).transpose()?;
    let filter = SessionFilter {
        live: p.live,
        host: p.host.clone(),
        user: p.user.clone(),
        started_from: finite(p.from, "from")?,
        started_to: finite(p.to, "to")?,
        pane: p.pane.clone(),
        legacy: p.legacy,
        include_hidden: p.hidden.unwrap_or(false),
        exclude: Forgets::load(ctx)?.sessions(),
        ..SessionFilter::default()
    };
    let mut rows = IndexDb::open(ctx)?.sessions(&filter, cursor, Some(limit + 1))?;
    let next = (rows.len() > limit).then(|| {
        rows.truncate(limit);
        rows.last().map(|r| {
            SessionCursor {
                started_at: r.session.started_at,
                id: r.id,
            }
            .encode()
        })
    });
    Ok(SessionPage {
        sessions: rows.into_iter().map(|r| r.session).collect(),
        next: next.flatten(),
    })
}

pub(crate) fn session_ref(s: &t::Session) -> SessionRef {
    SessionRef {
        uuid: s.uuid.clone(),
        user: s.user.clone(),
        scope: s.scope,
        state: s.state,
        started_at: s.started_at,
        ended_at: s.ended_at,
        holder_comm: s.holder_comm.clone(),
    }
}

/// The session `uuid` unless it is unknown or forgotten (404 either way).
pub(crate) fn visible_session(db: &IndexDb, forgets: &Forgets, uuid: &str) -> Result<SessionRow> {
    if forgets.session(uuid) {
        return Err(WebError::NotFound);
    }
    db.session(uuid)?.ok_or(WebError::NotFound)
}

/// The marker JSON for clients: an object, without the nonce.
fn marker_data(json: &str) -> Value {
    match serde_json::from_str::<Value>(json) {
        Ok(Value::Object(mut m)) => {
            m.remove("nonce");
            Value::Object(m)
        }
        _ => Value::Object(serde_json::Map::new()),
    }
}

pub(crate) fn detail(st: &AppState, uuid: &str) -> Result<SessionDetail> {
    let ctx = &st.ctx;
    let db = IndexDb::open(ctx)?;
    let forgets = Forgets::load(ctx)?;
    let _snap = db.snapshot()?;
    let row = visible_session(&db, &forgets, uuid)?;
    let parent = match &row.session.parent_uuid {
        Some(p) if !forgets.session(p) => db.session(p)?.map(|r| session_ref(&r.session)),
        _ => None,
    };
    let children = db
        .children(uuid)?
        .iter()
        .filter(|c| !forgets.session(&c.session.uuid))
        .map(|c| session_ref(&c.session))
        .collect();
    let markers = db.markers(row.id)?;
    // A tmux context names windows after their commands: what playback masks keeps none.
    let mask = crate::play::session::mask_of(ctx, &st.play.markers, &db, &forgets, &row)?;
    let masked = mask
        .spans
        .iter()
        .map(|s| MaskedSpan {
            t_from: crate::play::source::us_to_secs(s.from_us),
            t_to: (s.to_us != u64::MAX).then(|| crate::play::source::us_to_secs(s.to_us)),
            by: s.by.clone(),
        })
        .collect();
    let markers = markers
        .into_iter()
        .filter(|m| !(m.kind == "ctx" && mask.span_at(crate::play::source::secs_to_us(m.t)).is_some()))
        .map(|m| Marker {
            t: m.t,
            ts: m.ts,
            data: marker_data(&m.data_json),
            kind: m.kind,
        })
        .collect();
    let parts = db
        .parts(row.id)?
        .into_iter()
        .map(|p| CastPart {
            part: p.part,
            compressed: p.compressed,
            bytes: p.bytes,
            t_offset: p.t_offset,
        })
        .collect();
    let play_mode = crate::play::mode_of(ctx, &db, &row);
    Ok(SessionDetail {
        forgotten: forgets.rows(uuid).iter().map(Into::into).collect(),
        masked,
        session: row.session,
        parent,
        children,
        markers,
        parts,
        play_mode,
    })
}

/// The session's commands without forgotten ones, `t_start` order.
pub(crate) fn visible_commands(db: &IndexDb, forgets: &Forgets, row: &SessionRow) -> Result<Vec<im::Command>> {
    let uuid = &row.session.uuid;
    Ok(db
        .commands(row.id)?
        .into_iter()
        .filter(|c| !forgets.command(uuid, c))
        .collect())
}

pub(crate) fn commands(ctx: &WebContext, uuid: &str) -> Result<CommandList> {
    let db = IndexDb::open(ctx)?;
    let forgets = Forgets::load(ctx)?;
    let _snap = db.snapshot()?;
    let row = visible_session(&db, &forgets, uuid)?;
    Ok(CommandList {
        session: row.session.uuid.clone(),
        commands: visible_commands(&db, &forgets, &row)?
            .iter()
            .map(Command::from)
            .collect(),
    })
}

/// `path` for display: relative to the logs root, else its file name.
fn display_path(path: &Path, logs_root: &Path) -> String {
    match path.strip_prefix(logs_root) {
        Ok(rel) if !rel.as_os_str().is_empty() => rel.to_string_lossy().into_owned(),
        _ => path
            .file_name()
            .map_or_else(String::new, |f| f.to_string_lossy().into_owned()),
    }
}

pub(crate) fn import_report(ctx: &WebContext) -> Result<ImportReport> {
    let searcher = super::db::searcher(ctx)?;
    let progress = searcher.status()?.legacy.into();
    let files = searcher
        .legacy_report()?
        .into_iter()
        .map(|r| ImportFile {
            file: display_path(&r.path, &ctx.paths.logs_root),
            status: r.status.into(),
            session: r.session_uuid,
            banners: r.banners,
            log_lines: r.log_lines,
            aligned: r.aligned,
            mean_conf: r.mean_conf,
            error: r.error.map(|e| e.lines().next().unwrap_or_default().to_owned()),
        })
        .collect();
    Ok(ImportReport { progress, files })
}

/// Records a forget in meta.db (docs/API.md §5.6). For a command it also records what
/// playback hides of it ([`CmdSpan`], `play::session::command_span`), which keeps it masked
/// in playback and export after the serve daemon has deleted it from index.db.
pub(crate) fn forget(ctx: &WebContext, req: &ForgetRequest, by: &str) -> Result<im::Forgotten> {
    let target = req.target().map_err(|e| WebError::BadRequest(e.to_string()))?;
    let forgets = Forgets::load(ctx)?;
    if let Some(existing) = forgets.find(&target) {
        return Ok(existing.clone());
    }
    let db = IndexDb::open(ctx)?;
    let row = visible_session(&db, &forgets, target.uuid())?;
    let span = match &target {
        im::ForgetTarget::Command { uuid, seq } => {
            let all = db.commands(row.id)?;
            let c = all
                .iter()
                .find(|c| c.seq == *seq)
                .ok_or_else(|| WebError::Conflict(format!("session has no command {seq}")))?;
            let (from, to) = crate::play::session::command_span(&all, c);
            Some((uuid, *seq, CmdSpan { from, to }))
        }
        im::ForgetTarget::Session { .. } | im::ForgetTarget::Range { .. } => None,
    };
    drop(db);
    let mut meta = cinelog_index::db::open_meta(&ctx.db)?;
    // The span first: playback never sees the command forgotten without it.
    if let Some((uuid, seq, span)) = span {
        record_span(meta.as_mut(), uuid, seq, span)?;
    }
    let f = meta.forget(&target, by)?;
    tracing::info!("{by} forgot {} of session {}", target.kind(), target.uuid());
    Ok(f)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_paths_never_leave_the_logs_root() {
        let root = Path::new("/home/u/logs");
        assert_eq!(
            display_path(Path::new("/home/u/logs/2026-11/a.cast"), root),
            "2026-11/a.cast"
        );
        assert_eq!(display_path(Path::new("/elsewhere/b.cast"), root), "b.cast");
    }

    #[test]
    fn marker_data_drops_the_nonce() {
        assert_eq!(
            marker_data(r#"{"cl":1,"k":"pause","nonce":"x","reason":"tmux"}"#),
            serde_json::json!({"cl": 1, "k": "pause", "reason": "tmux"})
        );
        assert_eq!(marker_data("[1]"), serde_json::json!({}));
    }
}
