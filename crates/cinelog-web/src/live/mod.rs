//! Live viewing and the UI bus stream (work package M4-A; plan §6.4, §7.4, §8.4; docs/API.md
//! §9.0–§9.2).
//!
//! Provides, with exactly these signatures:
//! - [`LiveState::new`]: per-server live state (stream accounting, the live-session snapshot
//!   the watcher keeps, a cache of terminal sizes at join checkpoints, marker scans);
//! - [`start`]: called once by `server::spawn` inside the server's tokio runtime, after the
//!   listeners are bound. Spawns the watcher ([`watch`]) that publishes `session-start`,
//!   `session-update`, `session-end`, `index-progress`, `import-progress` and the
//!   `forgotten` rows `cinelog forget` adds on `state.bus`;
//! - [`routes`]: mounted under `/api` behind `auth::require_session`:
//!
//! | Route | Response |
//! |---|---|
//! | `GET /sessions/{uuid}/live` | `text/event-stream` of `LiveMessage`s, then event `done` ([`follow`]) |
//! | `GET /events` | `text/event-stream` of `BusEvent`s, `hello` first |
//!
//! Both streams end when `state.bus.closed()` resolves (server shutdown). The view is
//! read-only: nothing here accepts input for a session.

mod follow;
mod recorders;
mod sse;
mod watch;

#[cfg(test)]
mod tests;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use axum::Router;
use axum::extract::State;
use axum::response::Response;
use axum::routing::get;
use bytes::Bytes;
use futures_util::Stream;
use tokio::sync::broadcast::{self, error::RecvError};
use tokio::sync::watch as tokio_watch;

use crate::api::db::{blocking, parse_uuid};
use crate::api::extract::ApiPath;
use crate::api::types::{BusEvent, BusHello, LiveSession};
use crate::error::{Result, WebError};
use crate::state::{AppState, WebContext};

/// Open streams of each kind per server (docs/API.md §9.0).
pub const MAX_STREAMS: usize = 64;
/// How long a `hello` waits for the watcher's first pass.
const FIRST_PASS_WAIT: Duration = Duration::from_secs(3);
/// Sessions whose join checkpoint sizes are cached.
const SIZE_CACHE: usize = 64;

/// A terminal size at a reader position (`(part index, offset)`).
type SizeMark = ((usize, u64), (u16, u16));

/// The two stream kinds, counted separately.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Live,
    Events,
}

/// Per-server live state, held in `Shared::live`.
#[derive(Debug)]
pub struct LiveState {
    /// Registry directories of the scope (runtime dir, and for PERSONAL the state fallback
    /// of recorders started without `$XDG_RUNTIME_DIR`).
    dirs: Vec<PathBuf>,
    live_streams: AtomicUsize,
    event_streams: AtomicUsize,
    started: AtomicBool,
    /// The live sessions as last published, newest `started_at` first. The watcher
    /// publishes its changes while holding this lock, so that a subscriber that takes the
    /// lock to subscribe and copy it neither misses nor reorders an event.
    snapshot: Mutex<Vec<LiveSession>>,
    /// `true` once the watcher finished its first pass.
    ready: tokio_watch::Sender<bool>,
    /// Terminal size at a reader position, per session: the size scan of the next join of
    /// a growing cast continues from there instead of from the start.
    sizes: Mutex<HashMap<String, SizeMark>>,
}

impl LiveState {
    pub fn new(ctx: &WebContext) -> Self {
        Self {
            dirs: recorders::registry_dirs(ctx),
            live_streams: AtomicUsize::new(0),
            event_streams: AtomicUsize::new(0),
            started: AtomicBool::new(false),
            snapshot: Mutex::new(Vec::new()),
            ready: tokio_watch::channel(false).0,
            sizes: Mutex::new(HashMap::new()),
        }
    }

    fn counter(&self, kind: Kind) -> &AtomicUsize {
        match kind {
            Kind::Live => &self.live_streams,
            Kind::Events => &self.event_streams,
        }
    }

    /// Open `/events` streams (the watcher computes progress only while there are some).
    pub(crate) fn event_streams(&self) -> usize {
        self.event_streams.load(Ordering::Relaxed)
    }

    pub(crate) fn dirs(&self) -> &[PathBuf] {
        &self.dirs
    }

    fn snapshot(&self) -> MutexGuard<'_, Vec<LiveSession>> {
        self.snapshot.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn sizes(&self) -> MutexGuard<'_, HashMap<String, SizeMark>> {
        self.sizes.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// The cached size of `uuid` at or before reader position `pos`.
    pub(crate) fn cached_size(&self, uuid: &str, pos: (usize, u64)) -> Option<SizeMark> {
        self.sizes().get(uuid).copied().filter(|(at, _)| *at <= pos)
    }

    pub(crate) fn cache_size(&self, uuid: &str, pos: (usize, u64), size: (u16, u16)) {
        let mut sizes = self.sizes();
        if sizes.len() >= SIZE_CACHE && !sizes.contains_key(uuid) {
            sizes.clear();
        }
        sizes.insert(uuid.to_owned(), (pos, size));
    }
}

/// Counts an open stream until dropped.
struct StreamGuard {
    st: AppState,
    kind: Kind,
}

impl StreamGuard {
    fn acquire(st: &AppState, kind: Kind) -> Result<Self> {
        let counter = st.live.counter(kind);
        if counter.fetch_add(1, Ordering::AcqRel) >= MAX_STREAMS {
            counter.fetch_sub(1, Ordering::AcqRel);
            return Err(WebError::Unavailable(match kind {
                Kind::Live => "another live stream (too many are open)",
                Kind::Events => "another event stream (too many are open)",
            }));
        }
        Ok(Self { st: st.clone(), kind })
    }
}

impl Drop for StreamGuard {
    fn drop(&mut self) {
        self.st.live.counter(self.kind).fetch_sub(1, Ordering::AcqRel);
    }
}

/// Starts the background watcher of `state` (see the module docs). Later calls do nothing.
pub fn start(state: &AppState) {
    if state.live.started.swap(true, Ordering::AcqRel) {
        return;
    }
    tokio::spawn(watch::run(state.clone()));
}

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/sessions/{uuid}/live", get(live))
        .route("/events", get(events))
}

/// `GET /api/sessions/{uuid}/live` (docs/API.md §9.1).
async fn live(State(st): State<AppState>, ApiPath(uuid): ApiPath<String>) -> Result<Response> {
    let uuid = parse_uuid(&uuid)?;
    let guard = StreamGuard::acquire(&st, Kind::Live)?;
    let st2 = st.clone();
    let joined = blocking(move || follow::Follower::open(&st2, &uuid)).await?;
    let items = follow::spawn(joined)?;
    Ok(sse::response(items, st.bus.clone(), guard))
}

/// `GET /api/events` (docs/API.md §9.2).
async fn events(State(st): State<AppState>) -> Result<Response> {
    let guard = StreamGuard::acquire(&st, Kind::Events)?;
    let (rx, live) = subscribe(&st).await?;
    Ok(sse::response(bus_stream(st.clone(), rx, live), st.bus.clone(), guard))
}

/// Whether the watcher runs and has finished its first pass (waiting for that briefly).
async fn watcher_ready(st: &AppState) -> bool {
    if !st.live.started.load(Ordering::Acquire) {
        return false;
    }
    let mut ready = st.live.ready.subscribe();
    // A watcher that never finishes its first pass still lets the stream start.
    let _ = tokio::time::timeout(FIRST_PASS_WAIT, ready.wait_for(|r| *r)).await;
    *ready.borrow()
}

/// A bus subscription and the live sessions at that moment.
async fn subscribe(st: &AppState) -> Result<(broadcast::Receiver<BusEvent>, Vec<LiveSession>)> {
    if watcher_ready(st).await {
        let snapshot = st.live.snapshot();
        return Ok((st.bus.subscribe(), snapshot.clone()));
    }
    let rx = st.bus.subscribe();
    Ok((rx, current(st).await?))
}

/// The live sessions now: the watcher's snapshot, or (no watcher) the registry observed
/// directly.
async fn current(st: &AppState) -> Result<Vec<LiveSession>> {
    if watcher_ready(st).await {
        return Ok(st.live.snapshot().clone());
    }
    let st2 = st.clone();
    blocking(move || Ok(watch::observe_now(&st2))).await
}

fn hello(st: &AppState, live: Vec<LiveSession>) -> BusEvent {
    BusEvent::Hello(BusHello {
        version: crate::VERSION.to_owned(),
        server_started_at: st.started_at,
        live,
    })
}

/// The `/events` frames: `hello`, then every bus event; a fresh `hello` after falling more
/// than the bus capacity behind.
fn bus_stream(
    st: AppState,
    mut rx: broadcast::Receiver<BusEvent>,
    live: Vec<LiveSession>,
) -> impl Stream<Item = Bytes> + Send + 'static {
    async_stream::stream! {
        yield sse::bus_event(&hello(&st, live));
        loop {
            match rx.recv().await {
                Ok(ev) => yield sse::bus_event(&ev),
                Err(RecvError::Lagged(n)) => {
                    tracing::debug!("event stream fell {n} events behind: resync");
                    match current(&st).await {
                        Ok(live) => yield sse::bus_event(&hello(&st, live)),
                        Err(e) => {
                            tracing::warn!("event stream resync: {e}");
                            break;
                        }
                    }
                }
                Err(RecvError::Closed) => break,
            }
        }
    }
}
