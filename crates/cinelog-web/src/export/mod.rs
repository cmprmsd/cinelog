//! Export (work package M3-B; plan §7.5, §7.6; docs/API.md "Export").
//!
//! Provides, with exactly this signature, [`routes`]: mounted under `/api` behind
//! `auth::require_session`: `GET /sessions/{uuid}/export` (`ExportParams`), streamed with
//! `Content-Disposition: attachment`:
//! - `cast`: asciicast v3 slice: header, a t=0 `o` event with the avt dump at `from`, then
//!   the events up to `to` (relative times; same label rewrite and masking as playback);
//! - `txt`: the rendered rows of the range as UTF-8 text;
//! - `html`: one self-contained document: every character escaped, SGR colours and
//!   attributes as inline-styled spans (whitelist), no script, its own CSP meta tag.
//!
//! The bodies below are the pre-integration stand-ins of the contracts commit.

use axum::Router;
use axum::routing::get;

use crate::error::WebError;
use crate::state::AppState;

pub fn routes() -> Router<AppState> {
    Router::new().route("/sessions/{uuid}/export", get(unavailable))
}

async fn unavailable() -> WebError {
    WebError::Unavailable("export")
}
