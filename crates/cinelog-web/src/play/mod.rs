//! Playback (work package M3-B; plan §7.4, §7.5; docs/API.md "Playback").
//!
//! Provides, with exactly these signatures:
//! - [`PlayState::new`]: per-server playback state (on-demand segment builds of casts that
//!   are not indexed yet, with progress; bounded cache);
//! - [`routes`]: mounted under `/api` behind `auth::require_session`:
//!
//! | Route | Response |
//! |---|---|
//! | `GET /sessions/{uuid}/play` | `PlayInfo` (starts an on-demand build when needed) |
//! | `GET /sessions/{uuid}/cast` | asciicast v3 stream (`application/x-asciicast`), only below `index.playback_segment_threshold_mb`, else 413 |
//! | `GET /sessions/{uuid}/play/index.json` | `PlayIndex` |
//! | `GET /sessions/{uuid}/play/seg/{n}` | `PlaySegment` |
//!
//! Every served event stream: all parts in order (zstd-transparent, `zst_frames` seeks),
//! `cmd` markers rewritten to `$ <typed>` labels, every other `m` event dropped, forgotten
//! ranges masked (their `o` events replaced by one `[redacted by <who>]` event, their `i`
//! events dropped). Memory stays bounded by one avt screen plus buffers.
//!
//! Plans (`plan::Plan`) are built in the background when `/play` is polled, cached per
//! session (at most [`MAX_PLANS`]) and rebuilt when the cast grew, its indexed segments
//! changed or something in it was forgotten. While a rebuild for growth runs, the previous
//! plan keeps being served, so a player that already loaded an index stays consistent.

pub(crate) mod body;
pub(crate) mod cast;
pub(crate) mod plan;
pub(crate) mod session;
pub(crate) mod source;
pub(crate) mod stream;
#[cfg(test)]
mod tests;

use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use axum::Json;
use axum::Router;
use axum::extract::State;
use axum::http::{HeaderValue, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;

use self::plan::{Plan, Progress};
use self::session::{Loaded, mode_for};
use self::source::clamp_size;
use crate::api::db::{Forgets, IndexDb, SessionRow, blocking, parse_uuid};
use crate::api::extract::ApiPath;
use crate::api::types::{PlayIndex, PlayInfo, PlayMode, PlaySegment};
use crate::error::{Result, WebError};
use crate::state::{AppState, WebContext};

/// Plans kept in memory (each holds one avt dump per segment: a few MB for 517 MB casts).
pub const MAX_PLANS: usize = 6;
/// Concurrent background builds.
const MAX_BUILDS: usize = 2;
/// A failed build is retried after this long.
const FAILED_TTL: Duration = Duration::from_secs(30);
/// How long a request waits for a running build.
const BUILD_WAIT: Duration = Duration::from_secs(120);

pub const CAST_CONTENT_TYPE: &str = "application/x-asciicast";

#[derive(Default)]
struct Slot {
    /// Identity of what `ready` was built from.
    key: String,
    ready: Option<Arc<Plan>>,
    /// A build in progress: its generation and progress.
    build: Option<(u64, String, Arc<Progress>)>,
    failed: Option<(Instant, String, String)>,
    used: u64,
}

#[derive(Default)]
struct Inner {
    slots: HashMap<String, Slot>,
    building: usize,
    tick: u64,
    generation: u64,
}

#[derive(Default)]
pub struct PlayState {
    inner: Mutex<Inner>,
    done: Condvar,
    /// Spans of forgotten commands found in casts' markers.
    pub(crate) markers: session::MarkerCache,
}

impl std::fmt::Debug for PlayState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PlayState").finish_non_exhaustive()
    }
}

/// What `/play` finds.
enum Lookup {
    Ready(Arc<Plan>),
    Building(f64),
}

/// The identity of the data a plan is built from.
fn plan_key(s: &Loaded) -> String {
    let mut k = String::new();
    for p in &s.files.parts {
        let _ = write!(k, "{}:{}:{};", p.path.display(), p.bytes, p.t_offset_us);
    }
    let _ = write!(k, "|{}|", s.mask.key());
    if let Some(last) = s.segments.last() {
        let _ = write!(k, "{}:{}:{}", s.segments.len(), last.byte_offset, last.t_start);
    }
    let _ = write!(k, "|{}", s.labels.len());
    k
}

impl PlayState {
    pub fn new(ctx: &WebContext) -> Self {
        let _ = ctx;
        Self::default()
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Drops the cached plan of a session (something in it was forgotten); a running build
    /// of it is discarded when it finishes.
    pub fn invalidate(&self, uuid: &str) {
        let mut inner = self.lock();
        if let Some(slot) = inner.slots.get_mut(uuid) {
            slot.ready = None;
            slot.failed = None;
            slot.key.clear();
            if let Some(b) = slot.build.as_mut() {
                b.0 = u64::MAX;
            }
        }
    }

    /// Starts a build of `s` unless one of it runs or [`MAX_BUILDS`] run; the generation of
    /// the build to run, if this call must start it.
    fn claim(inner: &mut Inner, uuid: &str, key: &str) -> Option<(u64, Arc<Progress>)> {
        if inner.slots.get(uuid).is_some_and(|s| s.build.is_some()) || inner.building >= MAX_BUILDS {
            return None;
        }
        inner.generation += 1;
        let generation = inner.generation;
        let progress = Arc::new(Progress::default());
        inner.building += 1;
        let slot = inner.slots.entry(uuid.to_owned()).or_default();
        slot.build = Some((generation, key.to_owned(), progress.clone()));
        Some((generation, progress))
    }

    /// Stores a finished build and wakes waiters.
    fn finish(&self, uuid: &str, generation: u64, key: String, result: std::result::Result<Arc<Plan>, String>) {
        let mut inner = self.lock();
        inner.building = inner.building.saturating_sub(1);
        inner.tick += 1;
        let tick = inner.tick;
        if let Some(slot) = inner.slots.get_mut(uuid)
            && let Some(b) = &slot.build
            && (b.0 == generation || b.0 == u64::MAX)
        {
            let current = b.0 == generation;
            slot.build = None;
            // An invalidated build (generation `u64::MAX`) is stale: dropped.
            if current {
                match result {
                    Ok(p) => {
                        slot.ready = Some(p);
                        slot.key = key;
                        slot.failed = None;
                        slot.used = tick;
                    }
                    Err(msg) => slot.failed = Some((Instant::now(), key, msg)),
                }
            }
        }
        Self::evict(&mut inner);
        drop(inner);
        self.done.notify_all();
    }

    fn evict(inner: &mut Inner) {
        while inner.slots.values().filter(|s| s.ready.is_some()).count() > MAX_PLANS {
            let victim = inner
                .slots
                .iter()
                .filter(|(_, s)| s.ready.is_some() && s.build.is_none())
                .min_by_key(|(_, s)| s.used)
                .map(|(k, _)| k.clone());
            match victim {
                Some(k) => {
                    inner.slots.remove(&k);
                }
                None => break,
            }
        }
        inner
            .slots
            .retain(|_, s| s.ready.is_some() || s.build.is_some() || s.failed.is_some());
    }

    fn run_build(&self, s: &Loaded, generation: u64, key: String, progress: &Progress) -> Result<Arc<Plan>> {
        self.run_build_with(s.uuid(), generation, key, || Plan::build(s, progress))
    }

    /// Runs a claimed build and records its result. A panic in it (avt, the cast reader) is
    /// a failed build: the slot and the build count are released either way.
    fn run_build_with(
        &self,
        uuid: &str,
        generation: u64,
        key: String,
        build: impl FnOnce() -> Result<Plan>,
    ) -> Result<Arc<Plan>> {
        let started = Instant::now();
        let result = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(build)) {
            Ok(r) => r.map(Arc::new),
            Err(_) => Err(WebError::Internal("building playback segments panicked".into())),
        };
        match &result {
            Ok(p) => tracing::info!(
                "playback segments of {uuid}: {} in {:.2} s",
                p.segments.len(),
                started.elapsed().as_secs_f64()
            ),
            Err(e) => tracing::warn!("playback segments of {uuid}: {e}"),
        }
        self.finish(
            uuid,
            generation,
            key,
            result.as_ref().map(Arc::clone).map_err(WebError::message),
        );
        result
    }

    /// `/play`: the current plan, or the progress of its (possibly just started) build.
    fn lookup(st: &AppState, s: Loaded) -> Result<Lookup> {
        let key = plan_key(&s);
        let uuid = s.uuid().to_owned();
        let me = &st.play;
        let mut inner = me.lock();
        inner.tick += 1;
        let tick = inner.tick;
        if let Some(slot) = inner.slots.get_mut(&uuid) {
            slot.used = tick;
            if let Some(p) = slot.ready.as_ref().filter(|_| slot.key == key) {
                return Ok(Lookup::Ready(p.clone()));
            }
            if let Some((t, k, msg)) = &slot.failed
                && *k == key
                && t.elapsed() < FAILED_TTL
            {
                return Err(WebError::Internal(format!("building playback segments failed: {msg}")));
            }
            if let Some((_, k, progress)) = &slot.build {
                // A rebuild for new data: the previous plan stays valid meanwhile.
                return Ok(match &slot.ready {
                    Some(p) if *k == key && p.forgotten == s.forgotten => Lookup::Ready(p.clone()),
                    _ => Lookup::Building(progress.get()),
                });
            }
        }
        // A plan from before a forget is never served again.
        let stale = inner
            .slots
            .get(&uuid)
            .and_then(|slot| slot.ready.clone())
            .filter(|p| p.forgotten == s.forgotten);
        match Self::claim(&mut inner, &uuid, &key) {
            Some((generation, progress)) => {
                drop(inner);
                let st2 = st.clone();
                let p2 = progress.clone();
                let k2 = key.clone();
                let spawned = std::thread::Builder::new().name("cinelog-play".into()).spawn(move || {
                    let _ = st2.play.run_build(&s, generation, k2, &p2);
                });
                if let Err(e) = spawned {
                    me.finish(&uuid, generation, key, Err("no thread for the build".into()));
                    return Err(WebError::Io(e));
                }
                Ok(match stale {
                    Some(p) => Lookup::Ready(p),
                    None => Lookup::Building(progress.get()),
                })
            }
            None => Ok(match stale {
                Some(p) => Lookup::Ready(p),
                None => Lookup::Building(0.0),
            }),
        }
    }

    /// The plan to serve an index, a segment or an export from: the cached one (whatever it
    /// was built from, so a loaded player stays consistent), else the running build's
    /// result, else one built on this thread once fewer than [`MAX_BUILDS`] run.
    pub(crate) fn plan(&self, ctx: &WebContext, uuid: &str) -> Result<Arc<Plan>> {
        let deadline = Instant::now() + BUILD_WAIT;
        // Something forgotten since (also by `cinelog forget`) makes a cached plan stale.
        let forgotten = Forgets::load(ctx)?.fingerprint(uuid);
        let mut loaded: Option<Loaded> = None;
        loop {
            let mut inner = self.lock();
            // Wait for this session's plan, its running build or a free build slot.
            loop {
                inner.tick += 1;
                let tick = inner.tick;
                let building = inner.building;
                let slot = inner.slots.get_mut(uuid);
                let mut own_build = false;
                if let Some(slot) = slot {
                    if slot.ready.as_ref().is_some_and(|p| p.forgotten != forgotten) {
                        slot.ready = None;
                        slot.key.clear();
                    }
                    if let Some(p) = &slot.ready {
                        slot.used = tick;
                        return Ok(p.clone());
                    }
                    own_build = slot.build.is_some();
                }
                if !own_build && building < MAX_BUILDS {
                    break;
                }
                let now = Instant::now();
                if now >= deadline {
                    return Err(WebError::Unavailable("playback (segments are still being built)"));
                }
                inner = self
                    .done
                    .wait_timeout(inner, deadline - now)
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .0;
            }
            drop(inner);
            let s = match loaded.take() {
                Some(s) => s,
                None => Loaded::load(ctx, &self.markers, uuid)?,
            };
            let key = plan_key(&s);
            let claimed = Self::claim(&mut self.lock(), uuid, &key);
            match claimed {
                Some((generation, progress)) => return self.run_build(&s, generation, key, &progress),
                // Another build started in between: wait again.
                None => loaded = Some(s),
            }
        }
    }
}

/// For `cinelog forget <uuid>:<seq>`: records in meta.db what playback and export hide of
/// command `seq` (from the end of the command before it to its own end), read from the
/// index.db of `paths`. Call it before the forget is recorded and purged: once the command
/// is gone from index.db, this record is what keeps it masked (legacy casts have no markers
/// to recover it from). Returns whether the command was found.
pub fn record_forgotten_command_span(
    paths: &cinelog_index::DbPaths,
    meta: &mut dyn cinelog_index::MetaStore,
    uuid: &str,
    seq: u64,
) -> cinelog_index::Result<bool> {
    if !paths.index.exists() {
        return Ok(false);
    }
    let db = IndexDb::at(paths)?;
    let Some(row) = db.session(uuid)? else {
        return Ok(false);
    };
    let commands = db.commands(row.id)?;
    let Some(c) = commands.iter().find(|c| c.seq == seq) else {
        return Ok(false);
    };
    let (from, to) = session::command_span(&commands, c);
    crate::api::db::record_span(meta, uuid, seq, crate::api::db::CmdSpan { from, to })?;
    Ok(true)
}

/// Playback mode of a session row (for `SessionDetail`).
pub(crate) fn mode_of(ctx: &WebContext, db: &IndexDb, row: &SessionRow) -> PlayMode {
    let bytes = source::CastFiles::load(db, row)
        .map(|f| f.total_bytes())
        .unwrap_or_else(|_| row.session.cast_bytes.unwrap_or(0));
    mode_for(ctx, bytes)
}

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/sessions/{uuid}/play", get(play_info))
        .route("/sessions/{uuid}/cast", get(cast))
        .route("/sessions/{uuid}/play/index.json", get(play_index))
        .route("/sessions/{uuid}/play/seg/{n}", get(play_segment))
}

async fn play_info(State(st): State<AppState>, ApiPath(uuid): ApiPath<String>) -> Result<Json<PlayInfo>> {
    let uuid = parse_uuid(&uuid)?;
    blocking(move || {
        let s = Loaded::load(&st.ctx, &st.play.markers, &uuid)?;
        let session = s.row.session.clone();
        let (cols, rows) = clamp_size((session.cols, session.rows));
        let mode = mode_for(&st.ctx, s.files.total_bytes());
        let base = format!("/api/sessions/{uuid}");
        let info = match mode {
            PlayMode::Cast => PlayInfo {
                mode,
                url: format!("{base}/cast"),
                ready: true,
                progress: None,
                duration: session.duration,
                cols,
                rows,
            },
            PlayMode::Segmented => {
                let url = format!("{base}/play/index.json");
                match PlayState::lookup(&st, s)? {
                    Lookup::Ready(p) => PlayInfo {
                        mode,
                        url,
                        ready: true,
                        progress: None,
                        duration: Some(p.duration()),
                        cols: p.cols,
                        rows: p.rows,
                    },
                    Lookup::Building(progress) => PlayInfo {
                        mode,
                        url,
                        ready: false,
                        progress: Some(progress),
                        duration: session.duration,
                        cols,
                        rows,
                    },
                }
            }
        };
        Ok(info)
    })
    .await
    .map(Json)
}

async fn cast(State(st): State<AppState>, ApiPath(uuid): ApiPath<String>) -> Result<Response> {
    let uuid = parse_uuid(&uuid)?;
    let ctx = st.ctx.clone();
    let st2 = st.clone();
    let s = blocking(move || Loaded::load(&ctx, &st2.play.markers, &uuid)).await?;
    if mode_for(&st.ctx, s.files.total_bytes()) == PlayMode::Segmented {
        return Err(WebError::PayloadTooLarge(
            "the cast is above the playback segment threshold: use segmented playback".into(),
        ));
    }
    let body = body::streamed("cast stream", move |sink| cast::write_cast(&s, sink));
    let mut resp = body.into_response();
    resp.headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static(CAST_CONTENT_TYPE));
    Ok(resp)
}

async fn play_index(State(st): State<AppState>, ApiPath(uuid): ApiPath<String>) -> Result<Json<PlayIndex>> {
    let uuid = parse_uuid(&uuid)?;
    blocking(move || Ok(st.play.plan(&st.ctx, &uuid)?.index()))
        .await
        .map(Json)
}

async fn play_segment(
    State(st): State<AppState>,
    ApiPath((uuid, n)): ApiPath<(String, String)>,
) -> Result<Json<PlaySegment>> {
    let uuid = parse_uuid(&uuid)?;
    let n: usize = n.parse().map_err(|_| WebError::NotFound)?;
    blocking(move || st.play.plan(&st.ctx, &uuid)?.segment(n))
        .await
        .map(Json)
}
