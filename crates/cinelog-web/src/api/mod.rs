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
//! | `POST /forget` (`ForgetRequest`) | `Forgotten` (author = the `Principal`; published on the bus) |
//!
//! M4 merges `timeline::routes` (M4-B) and `annotations::routes` (M4-C) here.
//!
//! Reads go through read-only index.db connections (`cinelog_index::db::open_reader` plus
//! M3-B's own queries in `api::db`) on `spawn_blocking`; `forget` writes meta.db through
//! `cinelog_index::db::open_meta` (the serve daemon's forget sync then deletes the rows).

pub(crate) mod annotations;
pub(crate) mod db;
pub(crate) mod extract;
mod search;
pub(crate) mod sessions;
#[cfg(test)]
pub(crate) mod testkit;
#[cfg(test)]
mod tests;
pub(crate) mod timeline;
mod tree;
pub mod types;

use axum::Json;
use axum::Router;
use axum::extract::State;
use axum::routing::{get, post};

use self::db::blocking;
use self::extract::{ApiJson, ApiPath, ApiQuery};
use self::types::{
    BusEvent, CommandList, Entry, ForgetRequest, Forgotten, ImportReport, SearchParams, SearchResponse, SessionDetail,
    SessionPage, SessionParams, ShowParams, Status, Tree, TreeParams,
};
use crate::error::Result;
use crate::state::{AppState, Principal};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/status", get(status))
        .route("/sessions", get(session_list))
        .route("/tree", get(tree))
        .route("/sessions/{uuid}", get(session_detail))
        .route("/sessions/{uuid}/commands", get(commands))
        .route("/search", get(search))
        .route("/show", get(show))
        .route("/import", get(import))
        .route("/forget", post(forget))
        .merge(timeline::routes())
        .merge(annotations::routes())
}

async fn status(State(st): State<AppState>) -> Result<Json<Status>> {
    blocking(move || sessions::status(&st)).await.map(Json)
}

async fn session_list(State(st): State<AppState>, ApiQuery(p): ApiQuery<SessionParams>) -> Result<Json<SessionPage>> {
    blocking(move || sessions::list(&st.ctx, &p)).await.map(Json)
}

async fn tree(State(st): State<AppState>, ApiQuery(p): ApiQuery<TreeParams>) -> Result<Json<Tree>> {
    blocking(move || tree::tree(&st.ctx, &p, crate::now())).await.map(Json)
}

async fn session_detail(State(st): State<AppState>, ApiPath(uuid): ApiPath<String>) -> Result<Json<SessionDetail>> {
    let uuid = db::parse_uuid(&uuid)?;
    blocking(move || sessions::detail(&st, &uuid)).await.map(Json)
}

async fn commands(State(st): State<AppState>, ApiPath(uuid): ApiPath<String>) -> Result<Json<CommandList>> {
    let uuid = db::parse_uuid(&uuid)?;
    blocking(move || sessions::commands(&st.ctx, &uuid)).await.map(Json)
}

async fn search(State(st): State<AppState>, ApiQuery(p): ApiQuery<SearchParams>) -> Result<Json<SearchResponse>> {
    blocking(move || search::search(&st.ctx, &p)).await.map(Json)
}

async fn show(State(st): State<AppState>, ApiQuery(p): ApiQuery<ShowParams>) -> Result<Json<Entry>> {
    blocking(move || search::show(&st.ctx, &p)).await.map(Json)
}

async fn import(State(st): State<AppState>) -> Result<Json<ImportReport>> {
    blocking(move || sessions::import_report(&st.ctx)).await.map(Json)
}

async fn forget(
    State(st): State<AppState>,
    Principal(by): Principal,
    ApiJson(req): ApiJson<ForgetRequest>,
) -> Result<Json<Forgotten>> {
    let bus = st.bus.clone();
    let forgotten = blocking(move || {
        let f = sessions::forget(&st.ctx, &req, &by)?;
        st.play.invalidate(f.target.uuid());
        Ok(f)
    })
    .await?;
    let forgotten: Forgotten = (&forgotten).into();
    bus.publish(BusEvent::Forgotten(forgotten.clone()));
    Ok(Json(forgotten))
}
