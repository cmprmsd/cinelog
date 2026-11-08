//! JSON API handlers (work package M3-B; docs/API.md). [`types`] is frozen by the contracts
//! work package; everything else in `api/**` belongs to M3-B.
//!
//! Provides, with exactly this signature, [`routes`]: mounted under `/api` behind
//! `auth::require_session`, with these routes (paths relative to `/api`):
//!
//! | Route | Response |
//! |---|---|
//! | `GET /status` | `Status` |
//! | `GET /sessions` (`SessionParams`) | `SessionPage` |
//! | `GET /tree` (`TreeParams`) | `Tree` |
//! | `GET /sessions/{uuid}` | `SessionDetail` |
//! | `GET /sessions/{uuid}/commands` | `CommandList` |
//! | `GET /search` (`SearchParams`) | `SearchResponse` |
//! | `GET /show` (`ShowParams`) | `Entry` |
//! | `GET /import` | `ImportReport` |
//! | `POST /forget` (`ForgetRequest`) | `Forgotten` (author = the `Principal`) |
//!
//! Reads go through read-only index.db connections (`cinelog_index::db::open_reader` plus
//! M3-B's own queries) on `spawn_blocking`; `forget` writes meta.db through
//! `cinelog_index::db::open_meta` (the serve daemon's forget sync then deletes the rows).
//!
//! The bodies below are the pre-integration stand-ins of the contracts commit.

pub mod types;

use axum::Router;
use axum::routing::{get, post};

use crate::error::WebError;
use crate::state::AppState;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/status", get(unavailable))
        .route("/sessions", get(unavailable))
        .route("/tree", get(unavailable))
        .route("/sessions/{uuid}", get(unavailable))
        .route("/sessions/{uuid}/commands", get(unavailable))
        .route("/search", get(unavailable))
        .route("/show", get(unavailable))
        .route("/import", get(unavailable))
        .route("/forget", post(unavailable))
}

async fn unavailable() -> WebError {
    WebError::Unavailable("the API")
}
