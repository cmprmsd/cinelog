//! Host allowlist and Origin checks (work package M3-A; plan §7.3).
//!
//! Provides, with exactly these signatures:
//! - [`HostAllowlist::new`]: `server.hostnames` plus the hostname, FQDN, current local IPs
//!   (getifaddrs) and `localhost`/`127.0.0.1`/`::1`; [`HostAllowlist::refresh`] re-reads the
//!   local names, [`HostAllowlist::start_refresh`] does so every 60 s on the current tokio
//!   runtime;
//! - [`HostAllowlist::cert_names`]: the SAN list for a self-signed certificate (DNS names
//!   and IP literals), used by `tls::TlsOptions::from_context`;
//! - [`request_host`]: the host of a request from `uri().authority()` first (HTTP/2 has no
//!   Host header), else the `Host` header; port stripped, IPv6 brackets removed, lowercase;
//! - [`check_host`]: middleware, 400 (`bad_request`) for a host outside the list;
//! - [`check_origin`]: middleware for non-GET/HEAD/OPTIONS requests: `Origin` must equal
//!   `https://<authority of this request>` exactly (403 `forbidden`), and the content type
//!   must be `application/json` (415) for requests with a body.
//!
//! The bodies below are the pre-integration stand-ins of the contracts commit (they fail
//! closed).

use axum::extract::{Request, State};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

use crate::error::WebError;
use crate::state::{AppState, WebContext};

#[derive(Debug, Clone)]
pub struct HostAllowlist {
    configured: Vec<String>,
}

impl HostAllowlist {
    pub fn new(ctx: &WebContext) -> Self {
        Self {
            configured: ctx.config.server.hostnames.clone(),
        }
    }

    pub fn allows(&self, host: &str) -> bool {
        let _ = host;
        false
    }

    pub fn refresh(&self) {}

    pub fn start_refresh(&self) {}

    pub fn cert_names(&self) -> Vec<String> {
        self.configured.clone()
    }
}

pub fn request_host(req: &Request) -> Option<String> {
    let _ = req;
    None
}

pub async fn check_host(State(state): State<AppState>, req: Request, next: Next) -> Response {
    let _ = (state, req, next);
    WebError::Unavailable("host check").into_response()
}

pub async fn check_origin(State(state): State<AppState>, req: Request, next: Next) -> Response {
    let _ = (state, req, next);
    WebError::Unavailable("origin check").into_response()
}
