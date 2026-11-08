//! `GET /api/timeline` (work package M4-B; plan §8.3; docs/API.md §9.3). Stand-in until M4-B
//! lands: 503 `unavailable`.
//!
//! Provides, with exactly this signature, [`routes`] (merged by `api::routes`):
//! `GET /timeline` (`TimelineParams`) → `Timeline`. Reads index.db read-only (`api::db`)
//! and the annotations of the window's sessions from meta.db
//! (`MetaStore::find_annotations`).

use axum::Json;
use axum::Router;
use axum::extract::State;
use axum::routing::get;

use super::extract::ApiQuery;
use super::types::{Timeline, TimelineParams};
use crate::error::{Result, WebError};
use crate::state::AppState;

pub(crate) fn routes() -> Router<AppState> {
    Router::new().route("/timeline", get(timeline))
}

async fn timeline(State(_st): State<AppState>, ApiQuery(_p): ApiQuery<TimelineParams>) -> Result<Json<Timeline>> {
    Err(WebError::Unavailable("the timeline"))
}
