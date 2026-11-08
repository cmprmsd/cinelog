//! Extractors whose rejections are [`WebError`]s, so malformed input gets the `ApiError`
//! JSON body of docs/API.md §2 instead of axum's plain-text rejections.

use axum::extract::rejection::JsonRejection;
use axum::extract::{FromRequest, FromRequestParts, Json, Path, Query, Request};
use axum::http::request::Parts;
use serde::de::DeserializeOwned;

use crate::error::WebError;

/// Query string as `T` (the `*Params` types); malformed values are 400.
#[derive(Debug, Clone)]
pub struct ApiQuery<T>(pub T);

impl<T: DeserializeOwned, S: Send + Sync> FromRequestParts<S> for ApiQuery<T> {
    type Rejection = WebError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        Query::<T>::from_request_parts(parts, state)
            .await
            .map(|Query(v)| Self(v))
            .map_err(|e| WebError::BadRequest(format!("invalid query: {}", e.body_text())))
    }
}

/// Path parameters as `T`.
#[derive(Debug, Clone)]
pub struct ApiPath<T>(pub T);

impl<T: DeserializeOwned + Send, S: Send + Sync> FromRequestParts<S> for ApiPath<T> {
    type Rejection = WebError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        Path::<T>::from_request_parts(parts, state)
            .await
            .map(|Path(v)| Self(v))
            .map_err(|_| WebError::BadRequest("invalid path parameter".into()))
    }
}

/// A JSON body as `T`: 415 without `Content-Type: application/json`, 400 when it does not
/// parse. The body is never echoed back.
#[derive(Debug, Clone)]
pub struct ApiJson<T>(pub T);

impl<T: DeserializeOwned, S: Send + Sync> FromRequest<S> for ApiJson<T> {
    type Rejection = WebError;

    async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
        match Json::<T>::from_request(req, state).await {
            Ok(Json(v)) => Ok(Self(v)),
            Err(JsonRejection::MissingJsonContentType(_)) => Err(WebError::UnsupportedMediaType),
            Err(JsonRejection::JsonDataError(_)) => {
                Err(WebError::BadRequest("the body does not match the request type".into()))
            }
            Err(_) => Err(WebError::BadRequest("the body is not valid JSON".into())),
        }
    }
}
