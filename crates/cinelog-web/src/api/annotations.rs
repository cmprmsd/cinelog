//! Annotations and tags (work package M4-C; plan §6.3, §8.4; docs/API.md §9.4). Stand-in
//! until M4-C lands: 503 `unavailable`.
//!
//! Provides, with exactly this signature, [`routes`] (merged by `api::routes`):
//!
//! | Route | Body / query | Response |
//! |---|---|---|
//! | `GET /annotations` | `AnnotationParams` | `AnnotationPage` |
//! | `POST /annotations` | `AnnotationCreate` | 201 `Annotation` |
//! | `PATCH /annotations/{uuid}` | `AnnotationPatch` | `Annotation` |
//! | `DELETE /annotations/{uuid}` | | 204 |
//! | `GET /tags` | | `TagList` |
//! | `POST /tags` | `TagRequest` | `Tag` |
//!
//! The author is the `state::Principal`. Storage is meta.db through
//! `cinelog_index::db::open_meta` (`put_annotation`, `annotation`, `find_annotations`,
//! `tags`, `put_tag`); validation through `AnnotationCreate::validate`,
//! `AnnotationPatch::validate` and `TagRequest::validate`; every change is published on
//! `state.bus` (`BusEvent::Annotation`, `BusEvent::Tag`).

use axum::Json;
use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::{get, patch};

use super::extract::{ApiJson, ApiPath, ApiQuery};
use super::types::{
    Annotation, AnnotationCreate, AnnotationPage, AnnotationParams, AnnotationPatch, Tag, TagList, TagRequest,
};
use crate::error::{Result, WebError};
use crate::state::{AppState, Principal};

pub(crate) fn routes() -> Router<AppState> {
    Router::new()
        .route("/annotations", get(list).post(create))
        .route("/annotations/{uuid}", patch(update).delete(delete))
        .route("/tags", get(tags).post(put_tag))
}

const STAND_IN: WebError = WebError::Unavailable("annotations");

async fn list(State(_st): State<AppState>, ApiQuery(_p): ApiQuery<AnnotationParams>) -> Result<Json<AnnotationPage>> {
    Err(STAND_IN)
}

async fn create(
    State(_st): State<AppState>,
    Principal(_by): Principal,
    ApiJson(_req): ApiJson<AnnotationCreate>,
) -> Result<(StatusCode, Json<Annotation>)> {
    Err(STAND_IN)
}

async fn update(
    State(_st): State<AppState>,
    Principal(_by): Principal,
    ApiPath(_uuid): ApiPath<String>,
    ApiJson(_req): ApiJson<AnnotationPatch>,
) -> Result<Json<Annotation>> {
    Err(STAND_IN)
}

async fn delete(
    State(_st): State<AppState>,
    Principal(_by): Principal,
    ApiPath(_uuid): ApiPath<String>,
) -> Result<StatusCode> {
    Err(STAND_IN)
}

async fn tags(State(_st): State<AppState>) -> Result<Json<TagList>> {
    Err(STAND_IN)
}

async fn put_tag(
    State(_st): State<AppState>,
    Principal(_by): Principal,
    ApiJson(_req): ApiJson<TagRequest>,
) -> Result<Json<Tag>> {
    Err(STAND_IN)
}
