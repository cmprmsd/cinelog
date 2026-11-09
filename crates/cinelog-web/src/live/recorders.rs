//! The recorder registry as the server sees it (plan §5.1, §6.4): which sessions are being
//! recorded now, by whom they are paused, and what their casts say before the indexer has
//! seen them.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use cinelog_cast::{EventCode, parse_event_line};
use cinelog_core::Scope;
use cinelog_core::registry::{ProcessProbe, RecState, Registry, SystemProbe};
use cinelog_index::model::SessionMeta;
use cinelog_proto::CastMarker;

use crate::api::types::{self as t, TmuxLocation};
use crate::state::WebContext;

/// Bytes read from the end of a cast to look for its exit event.
const TAIL_BYTES: u64 = 64 << 10;

/// One recorder of this scope, from its registry entry.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Recorder {
    pub uuid: String,
    /// pid, starttime and boot_id describe a running process.
    pub alive: bool,
    pub state: RecState,
    pub state_reason: Option<String>,
    /// The part being written.
    pub cast: PathBuf,
    /// When the entry was last rewritten (the writer replaces it on every state change).
    pub changed_at: Option<f64>,
}

/// Registry directories of the scope. A PERSONAL recorder started without
/// `$XDG_RUNTIME_DIR` (ssh, cron) registers in the state fallback, so both are read (the
/// serve daemon reads the same ones).
pub(crate) fn registry_dirs(ctx: &WebContext) -> Vec<PathBuf> {
    let mut dirs = vec![ctx.paths.runtime_dir.clone()];
    if ctx.scope == Scope::Personal {
        let fallback = ctx.paths.state_dir.join("run");
        if !dirs.contains(&fallback) {
            dirs.push(fallback);
        }
    }
    dirs
}

fn mtime(path: &Path) -> Option<f64> {
    let m = std::fs::metadata(path).ok()?.modified().ok()?;
    Some(m.duration_since(UNIX_EPOCH).ok()?.as_secs_f64())
}

/// Every readable registry entry of `scope` in `dirs`, by uuid. A uuid listed twice (a
/// stale entry and a live one) keeps the live one.
pub(crate) fn scan(dirs: &[PathBuf], scope: Scope) -> BTreeMap<String, Recorder> {
    scan_with(dirs, scope, &SystemProbe)
}

pub(crate) fn scan_with(dirs: &[PathBuf], scope: Scope, probe: &dyn ProcessProbe) -> BTreeMap<String, Recorder> {
    let mut out: BTreeMap<String, Recorder> = BTreeMap::new();
    for dir in dirs {
        let reg = Registry::new(dir);
        let Ok(entries) = reg.entries() else { continue };
        for (key, entry) in entries {
            let Ok(e) = entry else { continue };
            if e.scope != scope || e.uuid.is_empty() {
                continue;
            }
            let rec = Recorder {
                alive: e.is_alive(probe),
                state: e.state,
                state_reason: e.state_reason,
                cast: e.cast,
                changed_at: mtime(&reg.path_for(&key)),
                uuid: e.uuid,
            };
            match out.get(&rec.uuid) {
                Some(have) if have.alive && !rec.alive => {}
                _ => {
                    out.insert(rec.uuid.clone(), rec);
                }
            }
        }
    }
    out
}

/// The recorder of session `uuid`, if it has a registry entry.
pub(crate) fn find(dirs: &[PathBuf], scope: Scope, uuid: &str) -> Option<Recorder> {
    scan(dirs, scope).remove(uuid)
}

/// The last [`TAIL_BYTES`] of the file at `path`.
fn read_tail(path: &Path) -> Option<Vec<u8>> {
    let mut f = File::open(path).ok()?;
    let len = f.metadata().ok()?.len();
    f.seek(SeekFrom::Start(len.saturating_sub(TAIL_BYTES))).ok()?;
    let mut tail = Vec::new();
    f.read_to_end(&mut tail).ok()?;
    Some(tail)
}

/// The complete non-empty lines of `tail`, last first.
fn lines_rev(tail: &[u8]) -> impl Iterator<Item = &[u8]> {
    tail.split(|b| *b == b'\n')
        .rev()
        .map(<[u8]>::trim_ascii)
        .filter(|l| !l.is_empty())
}

/// Whether the cast at `path` ends with an exit (`x`) event. Only its last bytes are read;
/// a compressed or unreadable file counts as not ended.
pub(crate) fn has_exit(path: &Path) -> bool {
    let Some(tail) = read_tail(path) else { return false };
    lines_rev(&tail)
        .find_map(parse_event_line)
        .is_some_and(|ev| ev.code == EventCode::Exit)
}

/// The child session a nested pause names, as its `nested` marker describes it.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct NestedChild {
    pub uuid: String,
    pub scope: t::Scope,
    /// The child's session URL as its recorder reported it.
    pub url: Option<String>,
}

/// The last `nested` marker for `child` in the cast part at `path`. A nested pause records
/// nothing, so the marker (written just before the pause) is among the last bytes, which are
/// all that is read.
pub(crate) fn nested_child(path: &Path, child: &str) -> Option<NestedChild> {
    let tail = read_tail(path)?;
    lines_rev(&tail)
        .filter_map(parse_event_line)
        .filter(|ev| ev.code == EventCode::Marker)
        .find_map(|ev| match CastMarker::from_json(&ev.decode_data()) {
            Ok(Some(CastMarker::Nested(n))) if n.child_uuid.eq_ignore_ascii_case(child) => Some(NestedChild {
                uuid: child.to_owned(),
                scope: n.scope.into(),
                url: n.url.filter(|u| !u.is_empty()),
            }),
            _ => None,
        })
}

/// What a session's first part says about it (header and `session` marker), for a live
/// session the indexer has not seen yet: no commands, no duration.
pub(crate) fn session_from_cast(first_part: &Path) -> Option<t::Session> {
    let mut r = cinelog_cast::open(first_part).ok()?;
    let header = r.header().clone();
    let ev = r.next_event()?.ok()?;
    if ev.t_us != 0 || ev.code != EventCode::Marker {
        return None;
    }
    let Ok(Some(CastMarker::Session(info))) = CastMarker::from_json(&ev.data) else {
        return None;
    };
    let m = SessionMeta::from_session_marker(&info, &header, first_part);
    Some(t::Session {
        uuid: m.uuid,
        host: m.host,
        scope: m.scope.into(),
        user: m.user,
        euid: m.euid,
        sudo_user: m.sudo_user,
        claimed_user: m.claimed_user,
        attribution: m.attribution,
        login_via: m.login_via,
        tty: m.tty,
        holder_comm: m.holder_comm,
        tmux: TmuxLocation::from(&m.tmux),
        parent_uuid: m.parent_uuid,
        parts: m.parts,
        compressed: m.compressed,
        cast_bytes: m.cast_bytes,
        started_at: m.started_at,
        ended_at: None,
        duration: None,
        exit_status: None,
        tz: m.tz,
        cols: m.cols,
        rows: m.rows,
        term: m.term,
        legacy: false,
        hidden: false,
        state: t::SessionState::Live,
        commands: 0,
    })
}
