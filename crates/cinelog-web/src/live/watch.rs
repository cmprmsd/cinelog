//! The live watcher behind the UI bus (docs/API.md §9.2): polls the recorder registry,
//! index.db and meta.db and publishes what changed.
//!
//! - `session-start` / `session-update` / `session-end` from the registry (every
//!   [`TICK`]) and the index's session rows; a session is live while its recorder is alive
//!   and the index does not say otherwise;
//! - `index-progress` and `import-progress` at most every [`Timing::progress`], only while
//!   an `/events` stream is open (the status query is not free on large indexes);
//! - `forgotten` for rows that `cinelog forget` adds to meta.db. `POST /api/forget`
//!   publishes its own; the watcher hears those on the bus and does not repeat them, and it
//!   waits one poll before publishing a new row so that the handler's event can arrive.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use tokio::sync::broadcast::{self, error::TryRecvError};

use super::recorders::{self, NestedChild, Recorder};
use crate::api::db::{Forgets, IndexDb};
use crate::api::types::{
    self as t, BusEvent, Forgotten, IndexProgress, LegacyProgress, LiveSession, RecStatus, SessionEnd, SessionState,
    split_state_reason,
};
use crate::state::AppState;

/// Registry poll interval (`session-start` within 1 s, status changes within 0.5 s).
pub(crate) const TICK: Duration = Duration::from_millis(250);

#[derive(Debug, Clone, Copy)]
pub(crate) struct Timing {
    /// meta.db `forgotten` poll interval.
    pub forget_poll: Duration,
    /// Minimum interval of `index-progress` and `import-progress`.
    pub progress: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Self {
            // A new row waits one poll: published within 2 s of its write.
            forget_poll: Duration::from_millis(500),
            progress: Duration::from_secs(2),
        }
    }
}

/// A live session as observed in one pass.
#[derive(Debug, Clone)]
struct Observed {
    live: LiveSession,
    cast: PathBuf,
    indexed: bool,
}

/// What makes a `session-update`: the recorder status and the session's metadata, not its
/// growing counters.
#[derive(Debug, Clone, PartialEq)]
struct Key {
    status: RecStatus,
    reason: Option<String>,
    detail: Option<String>,
    child: Option<(t::Scope, Option<String>)>,
    session: t::Session,
    indexed: bool,
}

impl Key {
    fn of(o: &Observed) -> Self {
        let mut session = o.live.session.clone();
        session.commands = 0;
        session.duration = None;
        session.cast_bytes = None;
        Self {
            status: o.live.status,
            reason: o.live.reason.clone(),
            detail: o.live.detail.clone(),
            child: o.live.child_scope.map(|scope| (scope, o.live.child_url.clone())),
            session,
            indexed: o.indexed,
        }
    }

    fn same_status(&self, other: &Self) -> bool {
        (self.status, &self.reason, &self.detail) == (other.status, &other.reason, &other.detail)
    }
}

#[derive(Debug, Clone)]
struct Tracked {
    live: LiveSession,
    key: Key,
    cast: PathBuf,
}

/// The live sessions of this pass: alive recorders whose session is not forgotten and not
/// over as far as the index knows. Sessions the index has not seen yet are described from
/// their cast (`fallback` caches that per uuid), and the child of a nested pause from its
/// `nested` marker (`children` caches that per uuid).
fn observe(
    recs: &BTreeMap<String, Recorder>,
    forgets: &Forgets,
    db: Option<&IndexDb>,
    fallback: &mut HashMap<String, t::Session>,
    children: &mut HashMap<String, NestedChild>,
) -> BTreeMap<String, Observed> {
    let mut out = BTreeMap::new();
    for rec in recs.values().filter(|r| r.alive) {
        if forgets.session(&rec.uuid) {
            continue;
        }
        let row = db.and_then(|db| match db.session(&rec.uuid) {
            Ok(row) => row,
            Err(e) => {
                tracing::debug!("live session {}: {e}", rec.uuid);
                None
            }
        });
        let (session, indexed) = match row {
            Some(row) if row.session.state != SessionState::Live => continue,
            Some(row) => {
                fallback.remove(&rec.uuid);
                (row.session, true)
            }
            None => {
                let cached = match fallback.get(&rec.uuid) {
                    Some(s) => Some(s.clone()),
                    None => {
                        let first = cinelog_cast::find_parts(&rec.cast).into_iter().next();
                        let s = first.and_then(|p| recorders::session_from_cast(&p));
                        if let Some(s) = &s {
                            fallback.insert(rec.uuid.clone(), s.clone());
                        }
                        s
                    }
                };
                // The header is not written yet: next pass.
                let Some(s) = cached else { continue };
                (s, false)
            }
        };
        let status = RecStatus::from(rec.state);
        let (reason, detail) = match status {
            RecStatus::Recording => (None, None),
            _ => split_state_reason(rec.state_reason.as_deref()),
        };
        let child = match (reason.as_deref(), detail.as_deref()) {
            (Some("nested"), Some(uuid)) => match children.get(&rec.uuid).filter(|c| c.uuid == uuid) {
                Some(c) => Some(c.clone()),
                // Not found yet (the marker may lag the registry): looked for again next pass.
                None => recorders::nested_child(&rec.cast, uuid).inspect(|c| {
                    children.insert(rec.uuid.clone(), c.clone());
                }),
            },
            _ => None,
        };
        let since = rec.changed_at.or(Some(session.started_at));
        out.insert(
            rec.uuid.clone(),
            Observed {
                live: LiveSession {
                    session,
                    status,
                    reason,
                    detail,
                    since,
                    child_scope: child.as_ref().map(|c| c.scope),
                    child_url: child.and_then(|c| c.url),
                },
                cast: rec.cast.clone(),
                indexed,
            },
        );
    }
    fallback.retain(|uuid, _| out.contains_key(uuid));
    children.retain(|uuid, _| out.contains_key(uuid));
    out
}

fn sorted(live: impl Iterator<Item = LiveSession>) -> Vec<LiveSession> {
    let mut v: Vec<LiveSession> = live.collect();
    v.sort_by(|a, b| {
        b.session
            .started_at
            .total_cmp(&a.session.started_at)
            .then_with(|| a.session.uuid.cmp(&b.session.uuid))
    });
    v
}

fn load_forgets(st: &AppState) -> Forgets {
    Forgets::load(&st.ctx).unwrap_or_else(|e| {
        tracing::debug!("live watcher: forgotten list: {e}");
        Forgets::default()
    })
}

/// The live sessions now, for a `hello` without a running watcher.
pub(crate) fn observe_now(st: &AppState) -> Vec<LiveSession> {
    let recs = recorders::scan(st.live.dirs(), st.ctx.scope);
    let db = IndexDb::open(&st.ctx).ok();
    let observed = observe(
        &recs,
        &load_forgets(st),
        db.as_ref(),
        &mut HashMap::new(),
        &mut HashMap::new(),
    );
    sorted(observed.into_values().map(|o| o.live))
}

/// How a session that left `live` ended.
fn end_state(db: Option<&IndexDb>, forgets: &Forgets, uuid: &str, tracked: &Tracked) -> SessionEnd {
    let row = db.and_then(|db| db.session(uuid).ok().flatten());
    let (state, ended_at) = match row {
        Some(row) if row.session.state != SessionState::Live => (row.session.state, row.session.ended_at),
        // Forgotten while recording: it leaves every list.
        _ if forgets.session(uuid) => (SessionState::Ended, None),
        _ if recorders::has_exit(&tracked.cast) => (SessionState::Ended, Some(crate::now())),
        _ if tracked.live.status == RecStatus::Degraded => (SessionState::Degraded, Some(crate::now())),
        _ => (SessionState::Crashed, Some(crate::now())),
    };
    SessionEnd {
        session: uuid.to_owned(),
        state,
        ended_at,
    }
}

/// Identity of a forgotten row (forgets are idempotent per target).
fn forget_key(f: &Forgotten) -> String {
    serde_json::to_string(&f.target).unwrap_or_default()
}

pub(crate) struct Watcher {
    timing: Timing,
    tracked: BTreeMap<String, Tracked>,
    fallback: HashMap<String, t::Session>,
    children: HashMap<String, NestedChild>,
    /// The bus, for the `forgotten` events of `POST /api/forget`.
    rx: broadcast::Receiver<BusEvent>,
    /// Forgotten rows already announced (or there before the first poll); `None` until
    /// that first poll.
    known: Option<HashSet<String>>,
    /// New rows seen in the last poll, published at the next one.
    pending: HashMap<String, Forgotten>,
    last_forget_poll: Option<Instant>,
    last_progress: Option<Instant>,
    index: Option<IndexProgress>,
    import: Option<LegacyProgress>,
}

impl Watcher {
    pub fn new(st: &AppState, timing: Timing) -> Self {
        Self {
            timing,
            tracked: BTreeMap::new(),
            fallback: HashMap::new(),
            children: HashMap::new(),
            rx: st.bus.subscribe(),
            known: None,
            pending: HashMap::new(),
            last_forget_poll: None,
            last_progress: None,
            index: None,
            import: None,
        }
    }

    /// One pass.
    pub fn tick(&mut self, st: &AppState, now: Instant) {
        let recs = recorders::scan(st.live.dirs(), st.ctx.scope);
        let forgets = load_forgets(st);
        let db = IndexDb::open(&st.ctx).ok();
        let observed = observe(&recs, &forgets, db.as_ref(), &mut self.fallback, &mut self.children);
        self.sessions(st, observed, &forgets, db.as_ref());
        drop(db);
        self.forgotten(st, now);
        self.progress(st, now);
    }

    fn sessions(
        &mut self,
        st: &AppState,
        observed: BTreeMap<String, Observed>,
        forgets: &Forgets,
        db: Option<&IndexDb>,
    ) {
        let mut snapshot = st.live.snapshot();
        let gone: Vec<String> = self
            .tracked
            .keys()
            .filter(|u| !observed.contains_key(*u))
            .cloned()
            .collect();
        for uuid in gone {
            if let Some(tracked) = self.tracked.remove(&uuid) {
                st.bus
                    .publish(BusEvent::SessionEnd(end_state(db, forgets, &uuid, &tracked)));
            }
        }
        for (uuid, o) in observed {
            let key = Key::of(&o);
            match self.tracked.get_mut(&uuid) {
                None => {
                    st.bus.publish(BusEvent::SessionStart(o.live.clone()));
                    self.tracked.insert(
                        uuid,
                        Tracked {
                            live: o.live,
                            key,
                            cast: o.cast,
                        },
                    );
                }
                Some(prev) => {
                    let mut live = o.live;
                    live.since = if key.same_status(&prev.key) {
                        prev.live.since
                    } else {
                        Some(crate::now())
                    };
                    let changed = key != prev.key;
                    prev.live = live;
                    prev.key = key;
                    prev.cast = o.cast;
                    if changed {
                        st.bus.publish(BusEvent::SessionUpdate(prev.live.clone()));
                    }
                }
            }
        }
        *snapshot = sorted(self.tracked.values().map(|t| t.live.clone()));
    }

    fn forgotten(&mut self, st: &AppState, now: Instant) {
        let known = self.known.get_or_insert_with(HashSet::new);
        loop {
            match self.rx.try_recv() {
                Ok(BusEvent::Forgotten(f)) => {
                    let k = forget_key(&f);
                    self.pending.remove(&k);
                    known.insert(k);
                }
                Ok(_) | Err(TryRecvError::Lagged(_)) => {}
                Err(TryRecvError::Empty | TryRecvError::Closed) => break,
            }
        }
        if self
            .last_forget_poll
            .is_some_and(|at| now.saturating_duration_since(at) < self.timing.forget_poll)
        {
            return;
        }
        let first = self.last_forget_poll.is_none();
        self.last_forget_poll = Some(now);
        let rows = match read_forgotten(st) {
            Ok(rows) => rows,
            Err(e) => {
                tracing::debug!("live watcher: meta.db: {e}");
                return;
            }
        };
        let mut pending = HashMap::new();
        for f in rows {
            let k = forget_key(&f);
            if known.contains(&k) {
                continue;
            }
            if first {
                known.insert(k);
            } else if self.pending.contains_key(&k) {
                st.bus.publish(BusEvent::Forgotten(f));
                known.insert(k);
            } else {
                pending.insert(k, f);
            }
        }
        self.pending = pending;
    }

    fn progress(&mut self, st: &AppState, now: Instant) {
        if st.live.event_streams() == 0
            || self
                .last_progress
                .is_some_and(|at| now.saturating_duration_since(at) < self.timing.progress)
        {
            return;
        }
        self.last_progress = Some(now);
        let status = match crate::api::sessions::status(st) {
            Ok(s) => s,
            Err(e) => {
                tracing::debug!("live watcher: index status: {e}");
                return;
            }
        };
        let index = IndexProgress {
            backlog_bytes: status.backlog_bytes,
            commands: status.commands,
            outputs: status.outputs,
            live: status.live,
            last_error: status.last_error,
        };
        if self.index.as_ref() != Some(&index) {
            st.bus.publish(BusEvent::IndexProgress(index.clone()));
            self.index = Some(index);
        }
        match self.import {
            Some(prev) if prev != status.legacy => st.bus.publish(BusEvent::ImportProgress(status.legacy)),
            _ => {}
        }
        self.import = Some(status.legacy);
    }
}

/// Every `meta.forgotten` row; none without a meta.db.
fn read_forgotten(st: &AppState) -> cinelog_index::Result<Vec<Forgotten>> {
    if !st.ctx.db.meta.exists() {
        return Ok(Vec::new());
    }
    let meta = cinelog_index::db::open_meta(&st.ctx.db)?;
    Ok(meta.forgotten()?.iter().map(Forgotten::from).collect())
}

/// The watcher task: a pass every [`TICK`] until the server shuts down.
pub(crate) async fn run(st: AppState) {
    let mut w = Watcher::new(&st, Timing::default());
    loop {
        let st2 = st.clone();
        w = match tokio::task::spawn_blocking(move || {
            w.tick(&st2, Instant::now());
            w
        })
        .await
        {
            Ok(w) => w,
            Err(e) => {
                tracing::error!("live watcher pass failed: {e}");
                Watcher::new(&st, Timing::default())
            }
        };
        st.live.ready.send_replace(true);
        tokio::select! {
            () = st.bus.closed() => break,
            () = tokio::time::sleep(TICK) => {}
        }
    }
}
