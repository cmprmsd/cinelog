//! cinelog HTTPS server (milestone M3; plan §7, §8; the HTTP contract is docs/API.md).
//!
//! | Module | Owner | Role |
//! |---|---|---|
//! | [`api::types`] | M3-0 contracts | wire types; generates `web/src/lib/api/types.ts` |
//! | [`state`], [`error`], [`server`], [`assets`], [`tokens`], this file | M3-0 contracts | shared state, errors, router, listeners, SPA, token files |
//! | [`tls`], [`auth`], [`hosts`], [`headers`], `build.rs` | M3-A server core | certificates, login and cookie sessions, Host/Origin checks, security headers |
//! | [`api`] (except `types`), [`play`], [`export`] | M3-B API and playback | JSON handlers, cast and segmented playback, forget, export |
//!
//! Request flow: security headers → Host allowlist → Origin/Content-Type check (mutating
//! requests) → route. `/healthz`, `/link/{uuid}`, `/api/login` and the SPA assets are
//! public; every other `/api` route sits behind `auth::require_session`, which provides
//! the [`state::Principal`].

pub mod api;
pub mod assets;
pub mod auth;
pub mod error;
pub mod export;
pub mod headers;
pub mod hosts;
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
