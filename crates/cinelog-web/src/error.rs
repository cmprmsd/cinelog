//! The one error type of every handler and middleware: maps to an HTTP status plus an
//! [`ApiError`] JSON body (docs/API.md "Errors").

use axum::Json;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use cinelog_index::IndexError;

use crate::api::types::{ApiError, ErrorCode};

#[derive(Debug, thiserror::Error)]
pub enum WebError {
    #[error("{0}")]
    BadRequest(String),
    /// No valid session, or a wrong token. Deliberately without detail.
    #[error("not logged in")]
    Unauthorized,
    #[error("{0}")]
    Forbidden(String),
    #[error("not found")]
    NotFound,
    #[error("method not allowed")]
    MethodNotAllowed,
    #[error("{0}")]
    Conflict(String),
    #[error("{0}")]
    PayloadTooLarge(String),
    #[error("expected Content-Type: application/json")]
    UnsupportedMediaType,
    #[error("too many attempts; retry in {retry_after_secs} s")]
    TooManyRequests { retry_after_secs: u64 },
    /// A component is not available (index not built yet, a stand-in of a work package).
    #[error("{0} is not available")]
    Unavailable(&'static str),
    #[error(transparent)]
    Index(#[from] IndexError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("{0}")]
    Internal(String),
}

pub type Result<T, E = WebError> = std::result::Result<T, E>;

impl WebError {
    pub fn code(&self) -> ErrorCode {
        match self {
            Self::BadRequest(_) => ErrorCode::BadRequest,
            Self::Unauthorized => ErrorCode::Unauthorized,
            Self::Forbidden(_) => ErrorCode::Forbidden,
            Self::NotFound => ErrorCode::NotFound,
            Self::MethodNotAllowed => ErrorCode::MethodNotAllowed,
            Self::Conflict(_) => ErrorCode::Conflict,
            Self::PayloadTooLarge(_) => ErrorCode::PayloadTooLarge,
            Self::UnsupportedMediaType => ErrorCode::UnsupportedMediaType,
            Self::TooManyRequests { .. } => ErrorCode::TooManyRequests,
            Self::Unavailable(_) => ErrorCode::Unavailable,
            Self::Index(
                IndexError::Locked(_)
                | IndexError::Unavailable(_)
                | IndexError::SchemaMismatch { .. }
                | IndexError::NewerSchema { .. },
            ) => ErrorCode::Unavailable,
            Self::Index(IndexError::Invalid(_)) => ErrorCode::BadRequest,
            Self::Index(IndexError::NotFound(_)) => ErrorCode::NotFound,
            Self::Index(_) | Self::Io(_) | Self::Internal(_) => ErrorCode::Internal,
        }
    }

    pub fn status(&self) -> StatusCode {
        StatusCode::from_u16(self.code().status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR)
    }

    /// The client-facing message. Internal errors are logged and replaced by a generic
    /// text, so file paths and SQL never reach the browser.
    pub fn message(&self) -> String {
        // Index errors carry SQL and paths: only their class reaches the client.
        let generic = match (self.code(), self) {
            (ErrorCode::Internal, _) => "internal error (see the server log)",
            (ErrorCode::Unavailable, Self::Index(IndexError::NewerSchema { .. })) => {
                "read-only: the database was written by a newer cinelog"
            }
            (ErrorCode::Unavailable, Self::Index(_)) => "the index is not available yet",
            (ErrorCode::NotFound, Self::Index(_)) => "not found",
            (ErrorCode::BadRequest, Self::Index(_)) => "invalid request",
            _ => return self.to_string(),
        };
        generic.to_owned()
    }
}

impl IntoResponse for WebError {
    fn into_response(self) -> Response {
        let status = self.status();
        if status.is_server_error() {
            tracing::error!("{self}");
        }
        let body = ApiError {
            error: self.code(),
            message: self.message(),
        };
        let mut resp = (status, Json(body)).into_response();
        if let Self::TooManyRequests { retry_after_secs } = self {
            resp.headers_mut()
                .insert(header::RETRY_AFTER, HeaderValue::from(retry_after_secs));
        }
        resp
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn statuses_and_messages() {
        assert_eq!(WebError::Unauthorized.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(
            WebError::Index(IndexError::Unavailable("db")).status(),
            StatusCode::SERVICE_UNAVAILABLE
        );
        let io = WebError::Io(std::io::Error::other("/root/logs/secret.cast: denied"));
        assert_eq!(io.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert!(!io.message().contains("/root"));
        let nf = WebError::Index(IndexError::NotFound("/root/logs/x.cast".into()));
        assert_eq!(
            (nf.status(), nf.message().as_str()),
            (StatusCode::NOT_FOUND, "not found")
        );
        let newer = WebError::Index(IndexError::NewerSchema {
            path: "/root/db/meta.db".into(),
            found: 2,
            supported: 1,
        });
        assert_eq!(newer.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert!(newer.message().contains("newer cinelog") && !newer.message().contains("/root"));
        let r = WebError::TooManyRequests { retry_after_secs: 7 }.into_response();
        assert_eq!(r.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(r.headers()[header::RETRY_AFTER], "7");
    }
}
