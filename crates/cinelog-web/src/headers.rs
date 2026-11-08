//! Security response headers (work package M3-A; plan §7.3). Applied to every response,
//! including errors and static assets.

use axum::extract::Request;
use axum::http::{HeaderName, HeaderValue, header};
use axum::middleware::Next;
use axum::response::Response;

/// `'wasm-unsafe-eval'`: asciinema-player compiles its terminal emulator from WebAssembly,
/// which CSP3 treats as eval. `'unsafe-inline'` styles: the player sets inline styles.
pub const CSP: &str = "default-src 'self'; script-src 'self' 'wasm-unsafe-eval'; \
    style-src 'self' 'unsafe-inline'; img-src 'self' data:; font-src 'self' data:; \
    connect-src 'self'; worker-src 'self'; object-src 'none'; base-uri 'none'; \
    form-action 'self'; frame-ancestors 'none'";

/// Paths whose responses must never be cached (they carry per-user data).
fn no_store(path: &str) -> bool {
    path == "/healthz" || path == "/api" || path.starts_with("/api/") || path.starts_with("/link/")
}

pub async fn security_headers(req: Request, next: Next) -> Response {
    let no_store = no_store(req.uri().path());
    let mut resp = next.run(req).await;
    let h = resp.headers_mut();
    let fixed: [(HeaderName, &str); 5] = [
        (header::CONTENT_SECURITY_POLICY, CSP),
        (header::X_FRAME_OPTIONS, "DENY"),
        (header::REFERRER_POLICY, "no-referrer"),
        (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        (HeaderName::from_static("cross-origin-opener-policy"), "same-origin"),
    ];
    for (name, value) in fixed {
        h.insert(name, HeaderValue::from_static(value));
    }
    if no_store {
        h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    }
    resp
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::Router;
    use axum::body::Body;
    use axum::routing::get;
    use tower::ServiceExt;

    #[tokio::test]
    async fn headers_on_every_response() {
        let app = Router::new()
            .route("/api/x", get(|| async { "x" }))
            .route("/assets/a.js", get(|| async { "a" }))
            .layer(axum::middleware::from_fn(security_headers));
        let api = app
            .clone()
            .oneshot(Request::get("/api/x").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(api.headers()[header::CONTENT_SECURITY_POLICY], CSP);
        assert_eq!(api.headers()[header::X_FRAME_OPTIONS], "DENY");
        assert_eq!(api.headers()[header::CACHE_CONTROL], "no-store");
        let asset = app
            .clone()
            .oneshot(Request::get("/assets/a.js").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert!(asset.headers().get(header::CACHE_CONTROL).is_none());
        let missing = app
            .oneshot(Request::get("/api/nope").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(missing.headers()[header::X_CONTENT_TYPE_OPTIONS], "nosniff");
    }
}
