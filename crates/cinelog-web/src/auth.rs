//! Token login and cookie sessions (work package M3-A; plan §7.2, docs/API.md "Auth").
//!
//! Provides, with exactly these signatures:
//! - [`Auth::new`]: built once per server from the context and the bound port; reads the
//!   token file (`ctx.paths.auth_file`, [`crate::tokens::TokenFile`]) on each login so
//!   `cinelog token new|revoke` apply without a restart; stores sessions in meta.db
//!   `auth_sessions` (hash of the 32-byte session id, principal, client IP, UA hash);
//! - [`routes`] (public, under `/api`): `GET /login` → `LoginInfo`, `POST /login`
//!   (`LoginRequest`; constant-time verify, 5 attempts per minute per client IP → 429,
//!   204 + cookie [`cookie_name`]`; HttpOnly; Secure; SameSite=Strict; Path=/; Max-Age`);
//! - [`session_routes`] (under `/api`, behind [`require_session`]): `POST /logout` → 204
//!   (deletes the session, clears the cookie), `GET /me` → `Me`;
//! - [`require_session`]: middleware; valid cookie + same client IP + same UA hash, idle
//!   TTL `server.session_idle_hours`, absolute TTL `server.session_max_days`; inserts
//!   [`crate::state::Principal`]; a cookie replayed from another IP or UA is rejected (401)
//!   and its session deleted; never logs cookie values or tokens;
//! - [`revoke_principal`]: deletes every auth session of a principal (`token revoke`).
//!
//! The client IP comes from `ConnectInfo<SocketAddr>` (the server is always started with
//! `into_make_service_with_connect_info`; tests use `MockConnectInfo`).
//!
//! The bodies below are the pre-integration stand-ins of the contracts commit: every
//! protected route fails closed with 503.

use std::path::Path;

use axum::Router;
use axum::extract::{Request, State};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};

use crate::error::{Result, WebError};
use crate::state::{AppState, WebContext};

/// `__Host-cl-<port>`: the port is part of the name because browsers do not isolate cookies
/// by port (§7.2).
pub fn cookie_name(port: u16) -> String {
    format!("__Host-cl-{port}")
}

#[derive(Debug, Clone)]
pub struct Auth {
    cookie: String,
}

impl Auth {
    pub fn new(ctx: &WebContext, port: u16) -> Self {
        let _ = ctx;
        Self {
            cookie: cookie_name(port),
        }
    }

    pub fn cookie_name(&self) -> &str {
        &self.cookie
    }
}

pub fn routes() -> Router<AppState> {
    Router::new().route("/login", get(unavailable).post(unavailable))
}

pub fn session_routes() -> Router<AppState> {
    Router::new()
        .route("/logout", post(unavailable))
        .route("/me", get(unavailable))
}

pub async fn require_session(State(state): State<AppState>, req: Request, next: Next) -> Response {
    let _ = (state, req, next);
    WebError::Unavailable("auth").into_response()
}

/// Deletes all auth sessions of `principal` from meta.db; returns how many.
pub fn revoke_principal(meta_db: &Path, principal: &str) -> Result<u64> {
    let _ = (meta_db, principal);
    Err(WebError::Unavailable("auth"))
}

async fn unavailable() -> WebError {
    WebError::Unavailable("auth")
}
