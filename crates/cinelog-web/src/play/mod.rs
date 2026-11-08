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
//! The bodies below are the pre-integration stand-ins of the contracts commit.

use axum::Router;
use axum::routing::get;

use crate::error::WebError;
use crate::state::{AppState, WebContext};

#[derive(Debug, Default)]
pub struct PlayState {}

impl PlayState {
    pub fn new(ctx: &WebContext) -> Self {
        let _ = ctx;
        Self {}
    }
}

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/sessions/{uuid}/play", get(unavailable))
        .route("/sessions/{uuid}/cast", get(unavailable))
        .route("/sessions/{uuid}/play/index.json", get(unavailable))
        .route("/sessions/{uuid}/play/seg/{n}", get(unavailable))
}

async fn unavailable() -> WebError {
    WebError::Unavailable("playback")
}
