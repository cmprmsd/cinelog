//! cinelog HTTPS server (milestones M3 and M4; plan §7, §8; the HTTP contract is docs/API.md).
//!
//! | Module | Owner | Role |
//! |---|---|---|
//! | [`api::types`] | M3-0 contracts | wire types; generates `web/src/lib/api/types.ts` |
//! | [`state`], [`error`], [`server`], [`assets`], [`tokens`], this file | M3-0 contracts | shared state, errors, router, listeners, SPA, token files |
//! | [`tls`], [`auth`], [`hosts`], [`headers`], `build.rs` | M3-A server core | certificates, login and cookie sessions, Host/Origin checks, security headers |
//! | [`api`] (except `types`, `timeline`, `annotations`), [`play`], [`export`] | M3-B API and playback | JSON handlers, cast and segmented playback, forget, export |
//! | [`bus`], route tables, M4 types | M4-0 contracts | UI bus broadcast; wiring of the M4 packages |
//! | [`live`] | M4-A live | live SSE, the `/events` stream, the session watcher |
//! | `api::timeline` | M4-B timeline | `GET /api/timeline` |
//! | `api::annotations` | M4-C annotations | annotations and tags in meta.db |
//!
//! Request flow: security headers → Host allowlist → Origin/Content-Type check (mutating
//! requests) → route. `/healthz`, `/link/{uuid}`, `/api/login` and the SPA assets are
//! public; every other `/api` route sits behind `auth::require_session`, which provides
//! the [`state::Principal`].

pub mod api;
pub mod assets;
pub mod auth;
pub mod bus;
pub mod error;
pub mod export;
pub mod headers;
pub mod hosts;
pub mod live;
pub mod play;
pub mod server;
pub mod state;
pub mod tls;
pub mod tokens;

pub use error::{Result, WebError};
pub use server::{ServerHandle, router, spawn};
pub use state::{AppState, Principal, WebContext};

/// Version of this build (the workspace version).
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Current time as Unix epoch seconds.
pub fn now() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0.0, |d| d.as_secs_f64())
}
