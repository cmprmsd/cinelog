//! The embedded SPA (`web/dist`, or the stub page chosen by `build.rs`). Unauthenticated:
//! the assets hold no data. Unknown paths without a file extension get `index.html`
//! (client-side routes such as `/s/<uuid>`); unknown files are 404.

use axum::body::Body;
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use rust_embed::RustEmbed;

use crate::error::WebError;

#[derive(RustEmbed)]
#[folder = "$CINELOG_WEB_DIST"]
struct Dist;

/// The binary carries the stub page instead of the UI build.
pub const IS_STUB: bool = option_env!("CINELOG_WEB_STUB").is_some();

const INDEX: &str = "index.html";

/// Fallback handler for every non-API path.
pub async fn spa(method: Method, uri: Uri, headers: HeaderMap) -> Response {
    if method != Method::GET && method != Method::HEAD {
        return WebError::MethodNotAllowed.into_response();
    }
    let path = uri.path().trim_start_matches('/');
    if path.split('/').any(|s| s == "..") || path.contains('\\') {
        return WebError::NotFound.into_response();
    }
    let path = if path.is_empty() { INDEX } else { path };
    let (name, file) = match Dist::get(path) {
        Some(f) => (path, f),
        None if is_file_like(path) => return WebError::NotFound.into_response(),
        None => match Dist::get(INDEX) {
            Some(f) => (INDEX, f),
            None => return WebError::NotFound.into_response(),
        },
    };
    let etag = format!(
        "\"{}\"",
        data_encoding::HEXLOWER.encode(&file.metadata.sha256_hash()[..16])
    );
    let cache = if name.starts_with("assets/") {
        "public, max-age=31536000, immutable"
    } else {
        "no-cache"
    };
    let mut resp = if headers
        .get(header::IF_NONE_MATCH)
        .is_some_and(|v| v.as_bytes() == etag.as_bytes())
    {
        StatusCode::NOT_MODIFIED.into_response()
    } else if method == Method::HEAD {
        Response::new(Body::empty())
    } else {
        Response::new(Body::from(file.data.into_owned()))
    };
    let h = resp.headers_mut();
    if let Ok(v) = HeaderValue::from_str(file.metadata.mimetype()) {
        h.insert(header::CONTENT_TYPE, v);
    }
    if let Ok(v) = HeaderValue::from_str(&etag) {
        h.insert(header::ETAG, v);
    }
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static(cache));
    resp
}

fn is_file_like(path: &str) -> bool {
    path.rsplit('/').next().is_some_and(|last| last.contains('.'))
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn get(path: &str) -> Response {
        spa(Method::GET, path.parse().unwrap(), HeaderMap::new()).await
    }

    #[tokio::test]
    async fn index_and_spa_routes() {
        for p in ["/", "/s/0193a1b2-0000-7000-8000-000000000001", "/login"] {
            let r = get(p).await;
            assert_eq!(r.status(), StatusCode::OK, "{p}");
            assert!(
                r.headers()[header::CONTENT_TYPE]
                    .to_str()
                    .unwrap()
                    .starts_with("text/html")
            );
            assert_eq!(r.headers()[header::CACHE_CONTROL], "no-cache");
        }
        assert_eq!(get("/assets/missing.js").await.status(), StatusCode::NOT_FOUND);
        assert_eq!(get("/../Cargo.toml").await.status(), StatusCode::NOT_FOUND);
        let r = spa(Method::POST, "/".parse().unwrap(), HeaderMap::new()).await;
        assert_eq!(r.status(), StatusCode::METHOD_NOT_ALLOWED);
    }

    #[tokio::test]
    async fn etag_revalidation() {
        let etag = get("/").await.headers()[header::ETAG].clone();
        let mut h = HeaderMap::new();
        h.insert(header::IF_NONE_MATCH, etag);
        let r = spa(Method::GET, "/".parse().unwrap(), h).await;
        assert_eq!(r.status(), StatusCode::NOT_MODIFIED);
    }
}
