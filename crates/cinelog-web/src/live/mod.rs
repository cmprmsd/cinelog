//! Live viewing and the UI bus stream (work package M4-A; plan §6.4, §7.4, §8.4; docs/API.md
//! §9.1, §9.2). Stand-in until M4-A lands: the routes answer 503 `unavailable`, [`start`]
//! starts nothing.
//!
//! Provides, with exactly these signatures:
//! - [`LiveState::new`]: per-server live state (viewer accounting, watcher bookkeeping);
//! - [`start`]: called once by `server::spawn` inside the server's tokio runtime, after the
//!   listeners are bound. Spawns the watcher that publishes `session-start`,
//!   `session-update`, `session-end`, `index-progress` and `import-progress` on
//!   `state.bus` (from the recorder registry in `ctx.paths.runtime_dir` and index.db);
//! - [`routes`]: mounted under `/api` behind `auth::require_session`:
//!
//! | Route | Response |
//! |---|---|
//! | `GET /sessions/{uuid}/live` | `text/event-stream` of `LiveMessage`s, then event `done` |
//! | `GET /events` | `text/event-stream` of `BusEvent`s (`hello` first) |
//!
//! Both streams end when `state.bus.closed()` resolves (server shutdown). Playback internals
//! (`play::source`, `play::stream`: part reading, label rewrite, masking) are `pub(crate)`
//! for reuse.

use axum::Router;
use axum::routing::get;

use crate::error::{Result, WebError};
use crate::state::{AppState, WebContext};

/// Per-server live state, held in `Shared::live`.
#[derive(Debug, Default)]
pub struct LiveState {}

impl LiveState {
    pub fn new(_ctx: &WebContext) -> Self {
        Self {}
    }
}

/// Starts the background watcher of `state` (see the module docs).
pub fn start(_state: &AppState) {}

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/sessions/{uuid}/live", get(unavailable))
        .route("/events", get(unavailable))
}

async fn unavailable() -> Result<()> {
    Err(WebError::Unavailable("live viewing"))
}
