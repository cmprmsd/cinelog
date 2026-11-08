//! Token login and cookie sessions (work package M3-A; plan §7.2, docs/API.md "Auth").
//!
//! - [`Auth::new`]: built once per server from the context and the bound port; reads the
//!   token file (`ctx.paths.auth_file`, [`crate::tokens::TokenFile`]) on each login so
//!   `cinelog token new|revoke` apply without a restart; stores sessions in meta.db
//!   `auth_sessions` (hash of the 32-byte session id, principal, client IP, UA hash);
//! - [`routes`] (public, under `/api`): `GET /login` → `LoginInfo`, `POST /login`
//!   (`LoginRequest`; constant-time verify, 5 attempts per minute per client IPv4 address
//!   or IPv6 /64 → 429,
//!   204 + cookie [`cookie_name`]`; HttpOnly; Secure; SameSite=Strict; Path=/; Max-Age`);
//! - [`session_routes`] (under `/api`, behind [`require_session`]): `POST /logout` → 204
//!   (deletes the session, clears the cookie), `GET /me` → `Me`;
//! - [`require_session`]: middleware; valid cookie + same client IP + same UA hash, idle
//!   TTL `server.session_idle_hours`, absolute TTL `server.session_max_days`; inserts
//!   [`crate::state::Principal`]; a cookie replayed from another IP or UA is rejected (401)
//!   and its session deleted; never logs cookie values or tokens. Two exceptions keep
//!   dual-stack clients logged in: all loopback addresses (`127.0.0.0/8`, `::1`) count as
//!   one client, and a request from the other address family (IPv4 vs IPv6) with the same
//!   UA is rejected (401) without deleting the session, so a happy-eyeballs reconnect over
//!   the other family does not end it;
//! - [`revoke_principal`]: deletes every auth session of a principal (`token revoke`).
//!   A session whose principal no longer has a token is refused and deleted as well, so a
//!   login that read the token file just before a revoke cannot outlive it.
//!
//! The client IP comes from `ConnectInfo<SocketAddr>` (the server is always started with
//! `into_make_service_with_connect_info`; tests use `MockConnectInfo`).

use std::collections::{HashMap, HashSet, VecDeque};
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

use axum::extract::connect_info::{ConnectInfo, MockConnectInfo};
use axum::extract::{FromRequestParts, Request, State};
use axum::http::request::Parts;
use axum::http::{Extensions, HeaderMap, HeaderValue, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use cinelog_index::IndexError;
use cinelog_index::schema::{self, Access, DbKind};
use data_encoding::BASE64URL_NOPAD;
use rusqlite::{Connection, OptionalExtension as _, params};
use sha2::{Digest, Sha256};

use crate::api::types::{LoginInfo, LoginRequest, Me};
use crate::error::{Result, WebError};
use crate::state::{AppState, Principal, WebContext};
use crate::tokens::TokenFile;
use crate::{VERSION, now};

/// Login attempts per client (IPv4 address or IPv6 /64) per [`RATE_WINDOW_SECS`].
pub const LOGIN_ATTEMPTS: usize = 5;
pub const RATE_WINDOW_SECS: f64 = 60.0;
/// `last_seen` is written at most this often per session (the idle TTL is hours).
const TOUCH_EVERY_SECS: f64 = 60.0;
const SESSION_ID_BYTES: usize = 32;
/// Upper bound of a login body; a token is 52 characters.
const LOGIN_BODY_MAX: usize = 4096;
/// Hard cap on the clients the rate limiter remembers.
const LIMITER_CLIENTS: usize = 4096;
/// Cookies of our name a request may carry (other ports of the host can set one too).
const COOKIE_CANDIDATES: usize = 4;

/// `__Host-cl-<port>`: the port is part of the name because browsers do not isolate cookies
/// by port (§7.2).
pub fn cookie_name(port: u16) -> String {
    format!("__Host-cl-{port}")
}

#[derive(Debug, Clone)]
pub struct Auth {
    cookie: String,
    inner: Arc<Inner>,
}

#[derive(Debug)]
struct Inner {
    token_file: PathBuf,
    meta: PathBuf,
    idle_secs: f64,
    max_secs: f64,
    /// One meta.db connection, opened on first use and reopened after an error.
    db: Mutex<Option<Connection>>,
    limiter: Mutex<RateLimiter>,
    holders: Mutex<Option<TokenHolders>>,
}

/// The principals that hold a token, as of one version of the token file.
#[derive(Debug)]
struct TokenHolders {
    stamp: Option<FileStamp>,
    names: HashSet<String>,
}

/// Identifies a version of the token file: it is replaced by rename on every change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FileStamp {
    ino: u64,
    len: u64,
    mtime_ns: i128,
}

impl FileStamp {
    /// `None` when the file does not exist (no tokens).
    fn of(path: &Path) -> Result<Option<Self>> {
        use std::os::unix::fs::MetadataExt;
        match std::fs::metadata(path) {
            Ok(m) => Ok(Some(Self {
                ino: m.ino(),
                len: m.len(),
                mtime_ns: i128::from(m.mtime()) * 1_000_000_000 + i128::from(m.mtime_nsec()),
            })),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(WebError::Internal(format!("token file: {e}"))),
        }
    }
}

/// The authenticated session of a request (inserted next to [`Principal`]).
#[derive(Debug, Clone, PartialEq)]
struct SessionCtx {
    id_hash: [u8; 32],
    /// The earlier of idle and absolute expiry.
    expires_at: f64,
}

impl<S: Send + Sync> FromRequestParts<S> for SessionCtx {
    type Rejection = WebError;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        parts.extensions.get::<Self>().cloned().ok_or(WebError::Unauthorized)
    }
}

/// Where a request comes from, as sessions are bound to it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Client {
    ip: String,
    ua_hash: [u8; 32],
}

impl Client {
    fn of(extensions: &Extensions, headers: &HeaderMap) -> Result<Self> {
        let addr = extensions
            .get::<ConnectInfo<SocketAddr>>()
            .map(|c| c.0)
            .or_else(|| extensions.get::<MockConnectInfo<SocketAddr>>().map(|m| m.0))
            .ok_or_else(|| WebError::Internal("no client address on the request".into()))?;
        let ua = headers.get(header::USER_AGENT).map_or(&[][..], HeaderValue::as_bytes);
        Ok(Self {
            ip: addr.ip().to_canonical().to_string(),
            ua_hash: Sha256::digest(ua).into(),
        })
    }

    fn ip_addr(&self) -> Option<IpAddr> {
        self.ip.parse().ok()
    }
}

/// How sessions treat a request's client compared with the one that logged in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Binding {
    Same,
    /// Same UA, but IPv4 vs IPv6: rejected, the session is kept.
    OtherFamily,
    /// Another address of the same family, or another UA: rejected, the session revoked.
    Other,
}

impl Client {
    fn binding(&self, ip: Option<&str>, ua_hash: Option<&[u8]>) -> Binding {
        let (Some(then), Some(now)) = (ip.and_then(|i| i.parse::<IpAddr>().ok()), self.ip_addr()) else {
            return Binding::Other;
        };
        if ua_hash != Some(&self.ua_hash[..]) {
            return Binding::Other;
        }
        let (then, now) = (then.to_canonical(), now.to_canonical());
        if then == now || (then.is_loopback() && now.is_loopback()) {
            Binding::Same
        } else if then.is_ipv4() != now.is_ipv4() {
            Binding::OtherFamily
        } else {
            Binding::Other
        }
    }
}

/// The rate-limited unit: an IPv4 address or an IPv6 /64 (one host usually owns a whole
/// /64, so per-address budgets would not limit it).
fn limiter_key(ip: IpAddr) -> IpAddr {
    match ip.to_canonical() {
        IpAddr::V6(v6) => IpAddr::V6((u128::from(v6) & !u128::from(u64::MAX)).into()),
        v4 => v4,
    }
}

#[derive(Debug, Default)]
struct RateLimiter {
    attempts: HashMap<IpAddr, VecDeque<f64>>,
}

impl RateLimiter {
    /// Records an attempt; `Err(retry_after_secs)` when the window is full.
    fn attempt(&mut self, ip: IpAddr, now: f64) -> std::result::Result<(), u64> {
        let key = limiter_key(ip);
        if self.attempts.len() >= LIMITER_CLIENTS && !self.attempts.contains_key(&key) {
            self.make_room(now);
        }
        let q = self.attempts.entry(key).or_default();
        while q.front().is_some_and(|t| now - t >= RATE_WINDOW_SECS) {
            q.pop_front();
        }
        if q.len() >= LOGIN_ATTEMPTS {
            let oldest = q.front().copied().unwrap_or(now);
            return Err((oldest + RATE_WINDOW_SECS - now).ceil().max(1.0) as u64);
        }
        q.push_back(now);
        Ok(())
    }

    /// Forgets idle clients, then the least recently active ones down to half the cap, so
    /// the map stays bounded and this O(n log n) pass runs at most once per
    /// `LIMITER_CLIENTS / 2` new clients.
    fn make_room(&mut self, now: f64) {
        self.attempts
            .retain(|_, q| q.back().is_some_and(|t| now - t < RATE_WINDOW_SECS));
        let keep = LIMITER_CLIENTS / 2;
        if self.attempts.len() <= keep {
            return;
        }
        let mut by_last: Vec<(f64, IpAddr)> = self
            .attempts
            .iter()
            .map(|(k, q)| (q.back().copied().unwrap_or(f64::MIN), *k))
            .collect();
        by_last.sort_unstable_by(|a, b| a.0.total_cmp(&b.0));
        for (_, k) in &by_last[..by_last.len() - keep] {
            self.attempts.remove(k);
        }
    }
}

/// A row of `auth_sessions`.
struct Row {
    principal: String,
    client_ip: Option<String>,
    ua_hash: Option<Vec<u8>>,
    last_seen: f64,
    expires_at: f64,
}

impl Auth {
    pub fn new(ctx: &WebContext, port: u16) -> Self {
        let s = &ctx.config.server;
        Self {
            cookie: cookie_name(port),
            inner: Arc::new(Inner {
                token_file: ctx.paths.auth_file.clone(),
                meta: ctx.db.meta.clone(),
                idle_secs: s.session_idle_hours as f64 * 3600.0,
                max_secs: s.session_max_days as f64 * 86_400.0,
                db: Mutex::new(None),
                limiter: Mutex::new(RateLimiter::default()),
                holders: Mutex::new(None),
            }),
        }
    }

    pub fn cookie_name(&self) -> &str {
        &self.cookie
    }

    fn set_cookie(&self, value: &str, max_age: u64) -> HeaderValue {
        let c = format!(
            "{}={value}; HttpOnly; Secure; SameSite=Strict; Path=/; Max-Age={max_age}",
            self.cookie
        );
        HeaderValue::from_str(&c).expect("cookie names and base64url values are valid header text")
    }

    /// Values of our cookie in the request, at most [`COOKIE_CANDIDATES`].
    fn cookie_values<'h>(&self, headers: &'h HeaderMap) -> Vec<&'h str> {
        headers
            .get_all(header::COOKIE)
            .iter()
            .filter_map(|v| v.to_str().ok())
            .flat_map(|v| v.split(';'))
            .filter_map(|kv| kv.trim().split_once('='))
            .filter(|(k, _)| *k == self.cookie)
            .map(|(_, v)| v.trim().trim_matches('"'))
            .take(COOKIE_CANDIDATES)
            .collect()
    }

    /// Runs blocking meta.db work off the async workers.
    async fn blocking<T: Send + 'static>(&self, f: impl FnOnce(&Inner) -> Result<T> + Send + 'static) -> Result<T> {
        let inner = Arc::clone(&self.inner);
        tokio::task::spawn_blocking(move || f(&inner))
            .await
            .map_err(|e| WebError::Internal(format!("auth task: {e}")))?
    }
}

impl Inner {
    fn with_db<T>(&self, f: impl FnOnce(&Connection) -> rusqlite::Result<T>) -> Result<T> {
        let mut guard: MutexGuard<'_, Option<Connection>> = self.db.lock().unwrap_or_else(|e| e.into_inner());
        if guard.is_none() {
            *guard = Some(schema::open(&self.meta, DbKind::Meta, Access::ReadWrite)?);
        }
        let conn = guard.as_ref().expect("opened above");
        f(conn).map_err(|e| {
            *guard = None;
            WebError::Index(IndexError::Sqlite(e))
        })
    }

    /// Verifies `token` and creates a session; returns the cookie value.
    fn login(&self, token: &str, client: &Client, now: f64) -> Result<String> {
        let tokens = TokenFile::load(&self.token_file).map_err(|e| WebError::Internal(format!("token file: {e}")))?;
        let principal = tokens.verify(token).ok_or(WebError::Unauthorized)?.name.clone();
        let mut id = [0u8; SESSION_ID_BYTES];
        getrandom::fill(&mut id).map_err(|e| WebError::Internal(format!("random source: {e}")))?;
        let id_hash: [u8; 32] = Sha256::digest(id).into();
        self.with_db(|c| {
            c.execute(
                "DELETE FROM auth_sessions WHERE expires_at <= ?1 OR last_seen <= ?2",
                params![now, now - self.idle_secs],
            )?;
            c.execute(
                "INSERT INTO auth_sessions(id_hash, principal, client_ip, ua_hash, created_at, last_seen, expires_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?5, ?6)",
                params![
                    &id_hash[..],
                    principal,
                    client.ip,
                    &client.ua_hash[..],
                    now,
                    now + self.max_secs
                ],
            )
        })?;
        tracing::info!("login of {principal} from {}", client.ip);
        Ok(BASE64URL_NOPAD.encode(&id))
    }

    /// Whether `principal` still has a token; the file is re-read only when it changed.
    fn holds_token(&self, principal: &str) -> Result<bool> {
        let stamp = FileStamp::of(&self.token_file)?;
        let mut guard = self.holders.lock().unwrap_or_else(|e| e.into_inner());
        if guard.as_ref().is_none_or(|h| h.stamp != stamp) {
            let tokens =
                TokenFile::load(&self.token_file).map_err(|e| WebError::Internal(format!("token file: {e}")))?;
            *guard = Some(TokenHolders {
                stamp,
                names: tokens.tokens.into_iter().map(|t| t.name).collect(),
            });
        }
        Ok(guard.as_ref().is_some_and(|h| h.names.contains(principal)))
    }

    /// The session of the first cookie value that is valid for `client`.
    fn validate(&self, values: &[String], client: &Client, now: f64) -> Result<(String, SessionCtx)> {
        for value in values {
            let Some(id_hash) = id_hash(value) else { continue };
            let row = self.with_db(|c| {
                c.query_row(
                    "SELECT principal, client_ip, ua_hash, last_seen, expires_at
                     FROM auth_sessions WHERE id_hash = ?1",
                    [&id_hash[..]],
                    |r| {
                        Ok(Row {
                            principal: r.get(0)?,
                            client_ip: r.get(1)?,
                            ua_hash: r.get(2)?,
                            last_seen: r.get(3)?,
                            expires_at: r.get(4)?,
                        })
                    },
                )
                .optional()
            })?;
            let Some(row) = row else { continue };
            let expired = now >= row.expires_at || now - row.last_seen >= self.idle_secs;
            let binding = client.binding(row.client_ip.as_deref(), row.ua_hash.as_deref());
            if !expired && binding == Binding::OtherFamily {
                tracing::info!(
                    "session of {} presented over the other address family ({}); refused, kept",
                    row.principal,
                    client.ip
                );
                continue;
            }
            let revoked = !expired && binding == Binding::Same && !self.holds_token(&row.principal)?;
            if revoked {
                tracing::info!("session of {} ended: its token was revoked", row.principal);
            }
            if expired || revoked || binding != Binding::Same {
                if !expired && !revoked {
                    tracing::warn!(
                        "session of {} presented from another client ({}); revoked",
                        row.principal,
                        client.ip
                    );
                }
                self.delete(&id_hash)?;
                continue;
            }
            let last_seen = if now - row.last_seen >= TOUCH_EVERY_SECS {
                self.with_db(|c| {
                    c.execute(
                        "UPDATE auth_sessions SET last_seen = ?2 WHERE id_hash = ?1",
                        params![&id_hash[..], now],
                    )
                })?;
                now
            } else {
                row.last_seen
            };
            let expires_at = (last_seen + self.idle_secs).min(row.expires_at);
            return Ok((row.principal, SessionCtx { id_hash, expires_at }));
        }
        Err(WebError::Unauthorized)
    }

    fn delete(&self, id_hash: &[u8; 32]) -> Result<()> {
        self.with_db(|c| c.execute("DELETE FROM auth_sessions WHERE id_hash = ?1", [&id_hash[..]]))?;
        Ok(())
    }
}

fn id_hash(cookie_value: &str) -> Option<[u8; 32]> {
    let raw = BASE64URL_NOPAD.decode(cookie_value.as_bytes()).ok()?;
    (raw.len() == SESSION_ID_BYTES).then(|| Sha256::digest(&raw).into())
}

pub fn routes() -> Router<AppState> {
    Router::new().route("/login", get(login_info).post(login))
}

pub fn session_routes() -> Router<AppState> {
    Router::new().route("/logout", post(logout)).route("/me", get(me))
}

pub async fn require_session(State(state): State<AppState>, mut req: Request, next: Next) -> Response {
    let auth = &state.auth;
    let values: Vec<String> = auth
        .cookie_values(req.headers())
        .into_iter()
        .map(str::to_owned)
        .collect();
    if values.is_empty() {
        return WebError::Unauthorized.into_response();
    }
    let client = match Client::of(req.extensions(), req.headers()) {
        Ok(c) => c,
        Err(e) => return e.into_response(),
    };
    let now = now();
    match auth.blocking(move |i| i.validate(&values, &client, now)).await {
        Ok((principal, session)) => {
            req.extensions_mut().insert(Principal(principal));
            req.extensions_mut().insert(session);
            next.run(req).await
        }
        Err(e) => e.into_response(),
    }
}

/// Deletes all auth sessions of `principal` from meta.db; returns how many.
pub fn revoke_principal(meta_db: &Path, principal: &str) -> Result<u64> {
    if !meta_db.exists() {
        return Ok(0);
    }
    let conn = schema::open(meta_db, DbKind::Meta, Access::ReadWrite)?;
    let n = conn
        .execute("DELETE FROM auth_sessions WHERE principal = ?1", [principal])
        .map_err(IndexError::from)?;
    Ok(n as u64)
}

async fn login_info(State(state): State<AppState>) -> Json<LoginInfo> {
    Json(LoginInfo {
        version: VERSION.to_owned(),
        scope: state.ctx.scope.into(),
        fingerprint: state.bound.fingerprint.clone(),
    })
}

async fn login(State(state): State<AppState>, req: Request) -> Result<Response> {
    let client = Client::of(req.extensions(), req.headers())?;
    let now = now();
    if let Some(ip) = client.ip_addr() {
        let mut limiter = state.auth.inner.limiter.lock().unwrap_or_else(|e| e.into_inner());
        if let Err(retry_after_secs) = limiter.attempt(ip, now) {
            tracing::warn!("login rate limit for {ip}");
            return Err(WebError::TooManyRequests { retry_after_secs });
        }
    }
    let body = axum::body::to_bytes(req.into_body(), LOGIN_BODY_MAX)
        .await
        .map_err(|_| WebError::BadRequest("login body too large or unreadable".into()))?;
    let LoginRequest { token } =
        serde_json::from_slice(&body).map_err(|_| WebError::BadRequest("expected {\"token\": \"…\"}".into()))?;
    let auth = state.auth.clone();
    let id = auth.blocking(move |i| i.login(&token, &client, now)).await?;
    let max_age = auth.inner.max_secs as u64;
    Ok((
        StatusCode::NO_CONTENT,
        [(header::SET_COOKIE, auth.set_cookie(&id, max_age))],
    )
        .into_response())
}

async fn logout(State(state): State<AppState>, session: SessionCtx) -> Result<Response> {
    let auth = state.auth.clone();
    auth.blocking(move |i| i.delete(&session.id_hash)).await?;
    Ok((StatusCode::NO_CONTENT, [(header::SET_COOKIE, auth.set_cookie("", 0))]).into_response())
}

async fn me(State(state): State<AppState>, Principal(principal): Principal, session: SessionCtx) -> Json<Me> {
    Json(Me {
        principal,
        scope: state.ctx.scope.into(),
        host: state.ctx.host.clone(),
        version: VERSION.to_owned(),
        protocol: cinelog_proto::PROTOCOL_VERSION,
        fingerprint: state.bound.fingerprint.clone(),
        expires_at: session.expires_at,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    use axum::body::Body;
    use axum::http::{Method, Response as HttpResponse};
    use tower::ServiceExt as _;

    use crate::api::types::{ApiError, ErrorCode, Scope};
    use crate::headers::CSP;

    const HOST: &str = "localhost:10443";
    const ORIGIN: &str = "https://localhost:10443";
    const UA: &str = "Mozilla/5.0 test";
    const ME: [u8; 4] = [192, 168, 1, 5];

    struct Fixture {
        _dir: tempfile::TempDir,
        state: AppState,
        app: Router,
        token: String,
    }

    fn fixture() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let state = AppState::for_tests(dir.path());
        let token = add_token(&state.ctx.paths.auth_file, "tester");
        let app = crate::server::router(state.clone());
        Fixture {
            _dir: dir,
            state,
            app,
            token,
        }
    }

    fn add_token(file: &Path, name: &str) -> String {
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        let mut f = TokenFile::load(file).unwrap();
        let t = f.add(name, 1.0).unwrap();
        f.save(file).unwrap();
        t.secret
    }

    struct Call<'a> {
        method: Method,
        path: &'a str,
        ip: IpAddr,
        ua: &'a str,
        cookie: Option<&'a str>,
        body: Option<String>,
        origin: &'a str,
    }

    impl<'a> Call<'a> {
        fn get(path: &'a str) -> Self {
            Self {
                method: Method::GET,
                path,
                ip: ME.into(),
                ua: UA,
                cookie: None,
                body: None,
                origin: ORIGIN,
            }
        }

        fn post(path: &'a str, body: Option<String>) -> Self {
            Self {
                method: Method::POST,
                body,
                ..Self::get(path)
            }
        }

        fn login(token: &str) -> Self {
            Self::post("/api/login", Some(serde_json::json!({ "token": token }).to_string()))
        }

        fn cookie(self, c: &'a str) -> Self {
            Self {
                cookie: Some(c),
                ..self
            }
        }
    }

    async fn send(app: &Router, c: Call<'_>) -> HttpResponse<Body> {
        let mut b = Request::builder()
            .method(c.method)
            .uri(c.path)
            .header(header::HOST, HOST)
            .header(header::USER_AGENT, c.ua)
            .header(header::ORIGIN, c.origin);
        if let Some(cookie) = c.cookie {
            b = b.header(header::COOKIE, format!("other=1; {cookie}"));
        }
        let body = match c.body {
            Some(s) => {
                b = b.header(header::CONTENT_TYPE, "application/json");
                Body::from(s)
            }
            None => Body::empty(),
        };
        let mut req = b.body(body).unwrap();
        req.extensions_mut().insert(ConnectInfo(SocketAddr::new(c.ip, 40000)));
        app.clone().oneshot(req).await.unwrap()
    }

    async fn error_of(r: HttpResponse<Body>) -> ErrorCode {
        let body = axum::body::to_bytes(r.into_body(), 1 << 16).await.unwrap();
        serde_json::from_slice::<ApiError>(&body).unwrap().error
    }

    /// Logs in and returns `name=value` for the Cookie header.
    async fn login(f: &Fixture, ip: impl Into<IpAddr>, ua: &str) -> String {
        let r = send(
            &f.app,
            Call {
                ip: ip.into(),
                ua,
                ..Call::login(&f.token)
            },
        )
        .await;
        assert_eq!(r.status(), StatusCode::NO_CONTENT);
        let set = r.headers()[header::SET_COOKIE].to_str().unwrap();
        set.split(';').next().unwrap().to_owned()
    }

    fn rows(f: &Fixture) -> i64 {
        f.state
            .auth
            .inner
            .with_db(|c| c.query_row("SELECT count(*) FROM auth_sessions", [], |r| r.get(0)))
            .unwrap()
    }

    fn sql(f: &Fixture, q: &str) {
        f.state.auth.inner.with_db(|c| c.execute(q, [])).unwrap();
    }

    #[test]
    fn rate_limiter_window() {
        let mut l = RateLimiter::default();
        let a: IpAddr = "10.0.0.1".parse().unwrap();
        let b: IpAddr = "10.0.0.2".parse().unwrap();
        for i in 0..LOGIN_ATTEMPTS {
            assert_eq!(l.attempt(a, 100.0 + i as f64), Ok(()));
        }
        assert_eq!(l.attempt(a, 110.0), Err(50));
        assert_eq!(l.attempt(a, 159.5), Err(1));
        assert_eq!(l.attempt(b, 110.0), Ok(()), "per IP");
        assert_eq!(l.attempt(a, 160.0), Ok(()), "the oldest attempt left the window");
        assert_eq!(l.attempt(a, 160.5), Err(1));
    }

    #[test]
    fn rate_limiter_prunes_idle_ips() {
        let mut l = RateLimiter::default();
        for i in 0..LIMITER_CLIENTS as u32 {
            l.attempt(IpAddr::from(i.to_be_bytes()), 0.0).unwrap();
        }
        l.attempt("10.0.0.1".parse().unwrap(), 100.0).unwrap();
        assert_eq!(l.attempts.len(), 1);
    }

    /// Regression: one IPv6 host owns a /64 and got a separate budget per address.
    #[test]
    fn rate_limiter_keys_ipv6_by_64() {
        let mut l = RateLimiter::default();
        for i in 0..LOGIN_ATTEMPTS as u16 {
            let ip = IpAddr::from([0x2001, 0xdb8, 0, 1, i, 7, 7, i]);
            assert_eq!(l.attempt(ip, 100.0), Ok(()));
        }
        assert!(l.attempt("2001:db8:0:1:ffff::1".parse().unwrap(), 101.0).is_err());
        assert_eq!(l.attempt("2001:db8:0:2::1".parse().unwrap(), 101.0), Ok(()));
        // IPv4-mapped addresses count as IPv4.
        let v4: IpAddr = "10.0.0.1".parse().unwrap();
        for _ in 0..LOGIN_ATTEMPTS {
            l.attempt("::ffff:10.0.0.1".parse().unwrap(), 101.0).unwrap();
        }
        assert!(l.attempt(v4, 101.0).is_err());
        assert_eq!(l.attempts.len(), 3);
    }

    /// Regression: with every client inside the window the map grew without bound and
    /// each new client paid an O(n) prune.
    #[test]
    fn rate_limiter_is_bounded_under_a_flood() {
        let mut l = RateLimiter::default();
        let flood = |i: u32| IpAddr::from([0x2001, 0xdb8, (i >> 16) as u16, i as u16, 0, 0, 0, 1]);
        for i in 0..(LIMITER_CLIENTS as u32 * 3) {
            l.attempt(flood(i), 10.0 + f64::from(i) / 1e4).unwrap();
            assert!(l.attempts.len() <= LIMITER_CLIENTS);
        }
        // The most recently active clients are kept, and keep their budget.
        let last = flood(LIMITER_CLIENTS as u32 * 3 - 1);
        assert_eq!(l.attempts[&limiter_key(last)].len(), 1);
        assert!(!l.attempts.contains_key(&limiter_key(flood(0))));
    }

    #[test]
    fn client_binding() {
        let c = |ip: &str, ua: &str| Client {
            ip: ip.into(),
            ua_hash: Sha256::digest(ua).into(),
        };
        let ua = Sha256::digest(UA).to_vec();
        let b = |client: Client, ip: &str| client.binding(Some(ip), Some(&ua));
        assert_eq!(b(c("192.168.1.5", UA), "192.168.1.5"), Binding::Same);
        assert_eq!(b(c("::1", UA), "127.0.0.1"), Binding::Same, "loopback is one client");
        assert_eq!(b(c("127.0.0.2", UA), "::1"), Binding::Same);
        assert_eq!(b(c("2001:db8::5", UA), "192.168.1.5"), Binding::OtherFamily);
        assert_eq!(b(c("192.168.1.5", UA), "2001:db8::5"), Binding::OtherFamily);
        assert_eq!(b(c("192.168.1.6", UA), "192.168.1.5"), Binding::Other);
        // Same /64 is still another client: the server host may share the victim's /64.
        assert_eq!(b(c("2001:db8::6", UA), "2001:db8::5"), Binding::Other);
        assert_eq!(b(c("2001:db8::5", "curl/8"), "192.168.1.5"), Binding::Other, "UA first");
        assert_eq!(c("10.0.0.1", UA).binding(None, Some(&ua)), Binding::Other);
        assert_eq!(c("10.0.0.1", UA).binding(Some("10.0.0.1"), None), Binding::Other);
        assert_eq!(c("10.0.0.1", UA).binding(Some("junk"), Some(&ua)), Binding::Other);
    }

    #[test]
    fn cookie_parsing() {
        let dir = tempfile::tempdir().unwrap();
        let auth = Auth::new(&WebContext::for_tests(dir.path()), 10443);
        assert_eq!(auth.cookie_name(), "__Host-cl-10443");
        let mut h = HeaderMap::new();
        h.append(
            header::COOKIE,
            "a=1; __Host-cl-10000=x; __Host-cl-10443=one".parse().unwrap(),
        );
        h.append(header::COOKIE, "__Host-cl-10443=\"two\"".parse().unwrap());
        assert_eq!(auth.cookie_values(&h), ["one", "two"]);
        assert_eq!(id_hash("one"), None);
        let id = BASE64URL_NOPAD.encode(&[7u8; 32]);
        assert_eq!(id_hash(&id), Some(Sha256::digest([7u8; 32]).into()));
    }

    #[tokio::test]
    async fn login_info_is_public() {
        let f = fixture();
        let r = send(&f.app, Call::get("/api/login")).await;
        assert_eq!(r.status(), StatusCode::OK);
        assert_eq!(r.headers()[header::CACHE_CONTROL], "no-store");
        let body = axum::body::to_bytes(r.into_body(), 1 << 16).await.unwrap();
        let info: LoginInfo = serde_json::from_slice(&body).unwrap();
        assert_eq!(
            info,
            LoginInfo {
                version: VERSION.into(),
                scope: Scope::Personal,
                fingerprint: "00".into()
            }
        );
    }

    #[tokio::test]
    async fn login_me_logout() {
        let f = fixture();
        let r = send(&f.app, Call::get("/api/me")).await;
        assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(r.headers()[header::CONTENT_SECURITY_POLICY], CSP);
        assert_eq!(error_of(r).await, ErrorCode::Unauthorized);

        let sloppy = format!("  {}\n", f.token.to_lowercase());
        let r = send(&f.app, Call::login(&sloppy)).await;
        assert_eq!(r.status(), StatusCode::NO_CONTENT);
        let set = r.headers()[header::SET_COOKIE].to_str().unwrap().to_owned();
        let (pair, attrs) = set.split_once("; ").unwrap();
        assert_eq!(attrs, "HttpOnly; Secure; SameSite=Strict; Path=/; Max-Age=604800");
        let (name, value) = pair.split_once('=').unwrap();
        assert_eq!(name, "__Host-cl-10443");
        assert_eq!(value.len(), 43);
        assert!(!set.contains(&f.token));

        // Only hashes are stored.
        let (stored, ip, ua): (Vec<u8>, String, Vec<u8>) = f
            .state
            .auth
            .inner
            .with_db(|c| {
                c.query_row("SELECT id_hash, client_ip, ua_hash FROM auth_sessions", [], |r| {
                    Ok((r.get(0)?, r.get(1)?, r.get(2)?))
                })
            })
            .unwrap();
        assert_eq!(Some(stored.as_slice()), id_hash(value).as_ref().map(|h| &h[..]));
        assert_eq!(ip, "192.168.1.5");
        assert_eq!(ua, Sha256::digest(UA).to_vec());

        let r = send(&f.app, Call::get("/api/me").cookie(pair)).await;
        assert_eq!(r.status(), StatusCode::OK);
        assert_eq!(r.headers()[header::CACHE_CONTROL], "no-store");
        let body = axum::body::to_bytes(r.into_body(), 1 << 16).await.unwrap();
        let me: Me = serde_json::from_slice(&body).unwrap();
        assert_eq!(me.principal, "tester");
        assert_eq!(
            (me.host.as_str(), me.protocol, me.scope),
            ("testhost", 1, Scope::Personal)
        );
        let idle_left = me.expires_at - now();
        assert!(
            (12.0 * 3600.0 - 5.0..=12.0 * 3600.0).contains(&idle_left),
            "{idle_left}"
        );

        let r = send(&f.app, Call::post("/api/logout", None).cookie(pair)).await;
        assert_eq!(r.status(), StatusCode::NO_CONTENT);
        assert_eq!(
            r.headers()[header::SET_COOKIE],
            "__Host-cl-10443=; HttpOnly; Secure; SameSite=Strict; Path=/; Max-Age=0"
        );
        assert_eq!(rows(&f), 0);
        let r = send(&f.app, Call::get("/api/me").cookie(pair)).await;
        assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn bad_logins() {
        let f = fixture();
        let (other, _) = crate::tokens::generate();
        let cases = [
            (Call::login(&other), StatusCode::UNAUTHORIZED),
            (Call::login("not base32!"), StatusCode::UNAUTHORIZED),
            (
                Call::post("/api/login", Some("{\"tok\":1}".into())),
                StatusCode::BAD_REQUEST,
            ),
            (
                Call {
                    ip: [10, 0, 0, 2].into(),
                    ..Call::post("/api/login", Some("x".repeat(LOGIN_BODY_MAX + 1)))
                },
                StatusCode::BAD_REQUEST,
            ),
            (
                Call {
                    ip: [10, 0, 0, 2].into(),
                    origin: "https://localhost:10444",
                    ..Call::login(&f.token)
                },
                StatusCode::FORBIDDEN,
            ),
        ];
        for (call, want) in cases {
            let r = send(&f.app, call).await;
            assert_eq!(r.status(), want);
        }
        assert_eq!(rows(&f), 0);
    }

    #[tokio::test]
    async fn login_rate_limit() {
        let f = fixture();
        let (wrong, _) = crate::tokens::generate();
        for _ in 0..LOGIN_ATTEMPTS {
            let r = send(&f.app, Call::login(&wrong)).await;
            assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
        }
        // Even the right token is refused while the window is full.
        let r = send(&f.app, Call::login(&f.token)).await;
        assert_eq!(r.status(), StatusCode::TOO_MANY_REQUESTS);
        let retry: u64 = r.headers()[header::RETRY_AFTER].to_str().unwrap().parse().unwrap();
        assert!((1..=60).contains(&retry));
        assert_eq!(error_of(r).await, ErrorCode::TooManyRequests);
        // Another client IP is not affected.
        login(&f, [10, 0, 0, 9], UA).await;
    }

    #[tokio::test]
    async fn replay_from_another_client_revokes() {
        let f = fixture();
        let c = login(&f, ME, UA).await;
        let other_ua = send(
            &f.app,
            Call {
                ua: "curl/8",
                ..Call::get("/api/me").cookie(&c)
            },
        )
        .await;
        assert_eq!(other_ua.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(rows(&f), 0, "the session is deleted");
        let r = send(&f.app, Call::get("/api/me").cookie(&c)).await;
        assert_eq!(r.status(), StatusCode::UNAUTHORIZED, "also for its owner");

        let c = login(&f, ME, UA).await;
        let other_ip = send(
            &f.app,
            Call {
                ip: [192, 168, 1, 6].into(),
                ..Call::get("/api/me").cookie(&c)
            },
        )
        .await;
        assert_eq!(other_ip.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(rows(&f), 0);

        // Same family, same /64: revoked as well.
        let v6: IpAddr = "2001:db8::5".parse().unwrap();
        let c = login(&f, v6, UA).await;
        let r = send(
            &f.app,
            Call {
                ip: "2001:db8::6".parse().unwrap(),
                ..Call::get("/api/me").cookie(&c)
            },
        )
        .await;
        assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(rows(&f), 0);

        // A foreign cookie of the same name (set from another port) does not hide ours.
        let c = login(&f, ME, UA).await;
        let junk = format!("__Host-cl-10443={}", BASE64URL_NOPAD.encode(&[1u8; 32]));
        let both = format!("{junk}; {c}");
        let r = send(&f.app, Call::get("/api/me").cookie(&both)).await;
        assert_eq!(r.status(), StatusCode::OK);
    }

    /// Regression: a dual-stack client that reconnected over the other address family lost
    /// its session.
    #[tokio::test]
    async fn dual_stack_clients_keep_their_session() {
        let f = fixture();
        let c = login(&f, ME, UA).await;
        let v6 = Call {
            ip: "2001:db8::5".parse().unwrap(),
            ..Call::get("/api/me").cookie(&c)
        };
        assert_eq!(send(&f.app, v6).await.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(rows(&f), 1, "kept");
        assert_eq!(
            send(&f.app, Call::get("/api/me").cookie(&c)).await.status(),
            StatusCode::OK
        );
        // ... but another UA over the other family still revokes it.
        let v6_curl = Call {
            ip: "2001:db8::5".parse().unwrap(),
            ua: "curl/8",
            ..Call::get("/api/me").cookie(&c)
        };
        assert_eq!(send(&f.app, v6_curl).await.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(rows(&f), 0);

        // localhost resolves to ::1 and 127.0.0.1: one client.
        let c = login(&f, [127, 0, 0, 1], UA).await;
        let r = send(
            &f.app,
            Call {
                ip: "::1".parse().unwrap(),
                ..Call::get("/api/me").cookie(&c)
            },
        )
        .await;
        assert_eq!(r.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn idle_and_absolute_ttl() {
        let f = fixture();
        let c = login(&f, ME, UA).await;
        sql(&f, "UPDATE auth_sessions SET last_seen = last_seen - 12 * 3600 - 1");
        assert_eq!(
            send(&f.app, Call::get("/api/me").cookie(&c)).await.status(),
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(rows(&f), 0);

        let c = login(&f, ME, UA).await;
        sql(&f, "UPDATE auth_sessions SET expires_at = last_seen - 1");
        assert_eq!(
            send(&f.app, Call::get("/api/me").cookie(&c)).await.status(),
            StatusCode::UNAUTHORIZED
        );

        // Activity moves last_seen (at most once a minute) and keeps the session alive.
        let c = login(&f, ME, UA).await;
        sql(&f, "UPDATE auth_sessions SET last_seen = last_seen - 11 * 3600");
        assert_eq!(
            send(&f.app, Call::get("/api/me").cookie(&c)).await.status(),
            StatusCode::OK
        );
        let seen: f64 = f
            .state
            .auth
            .inner
            .with_db(|c| c.query_row("SELECT last_seen FROM auth_sessions", [], |r| r.get(0)))
            .unwrap();
        assert!(now() - seen < 5.0);
        // The absolute TTL caps `expires_at`.
        sql(
            &f,
            "UPDATE auth_sessions SET expires_at = last_seen + 100, last_seen = last_seen - 120",
        );
        let r = send(&f.app, Call::get("/api/me").cookie(&c)).await;
        let body = axum::body::to_bytes(r.into_body(), 1 << 16).await.unwrap();
        let me: Me = serde_json::from_slice(&body).unwrap();
        assert!(me.expires_at - now() < 101.0);
    }

    #[tokio::test]
    async fn revoke_and_token_reload() {
        let f = fixture();
        assert_eq!(revoke_principal(&f._dir.path().join("none.db"), "tester").unwrap(), 0);
        let a = login(&f, ME, UA).await;
        let b = login(&f, [10, 0, 0, 3], UA).await;
        // A token added while the server runs works at once.
        let bob = add_token(&f.state.ctx.paths.auth_file, "bob");
        let r = send(
            &f.app,
            Call {
                ip: [10, 0, 0, 4].into(),
                ..Call::login(&bob)
            },
        )
        .await;
        assert_eq!(r.status(), StatusCode::NO_CONTENT);
        assert_eq!(rows(&f), 3);

        assert_eq!(revoke_principal(&f.state.ctx.db.meta, "tester").unwrap(), 2);
        assert_eq!(
            send(&f.app, Call::get("/api/me").cookie(&a)).await.status(),
            StatusCode::UNAUTHORIZED
        );
        let r = send(
            &f.app,
            Call {
                ip: [10, 0, 0, 3].into(),
                ..Call::get("/api/me").cookie(&b)
            },
        )
        .await;
        assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(rows(&f), 1);
    }

    /// A session stored by a login that read the token file just before `token revoke`
    /// rewrote it (so `revoke_principal` ran before the row existed) still ends: the
    /// principal no longer holds a token.
    #[tokio::test]
    async fn a_session_without_a_token_holder_is_refused() {
        let f = fixture();
        let bob = add_token(&f.state.ctx.paths.auth_file, "bob");
        let cookie = login(&f, ME, UA).await;
        let r = send(
            &f.app,
            Call {
                ip: [10, 0, 0, 4].into(),
                ..Call::login(&bob)
            },
        )
        .await;
        assert_eq!(r.status(), StatusCode::NO_CONTENT);
        assert_eq!(
            send(&f.app, Call::get("/api/me").cookie(&cookie)).await.status(),
            StatusCode::OK
        );
        assert_eq!(rows(&f), 2);

        let mut file = TokenFile::load(&f.state.ctx.paths.auth_file).unwrap();
        assert!(file.remove("tester"));
        file.save(&f.state.ctx.paths.auth_file).unwrap();
        assert_eq!(
            send(&f.app, Call::get("/api/me").cookie(&cookie)).await.status(),
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(rows(&f), 1, "the orphaned session is deleted, bob's is kept");
    }

    #[tokio::test]
    async fn principal_reaches_handlers() {
        let f = fixture();
        let app = Router::new()
            .route("/x", get(|Principal(p): Principal| async move { p }))
            .route_layer(axum::middleware::from_fn_with_state(f.state.clone(), require_session))
            .with_state(f.state.clone())
            .layer(MockConnectInfo(SocketAddr::from((ME, 1))));
        let c = login(&f, ME, UA).await;
        let req = |cookie: Option<&str>| {
            let mut b = Request::get("/x").header(header::USER_AGENT, UA);
            if let Some(c) = cookie {
                b = b.header(header::COOKIE, c);
            }
            b.body(Body::empty()).unwrap()
        };
        let r = app.clone().oneshot(req(Some(&c))).await.unwrap();
        assert_eq!(r.status(), StatusCode::OK);
        assert_eq!(&axum::body::to_bytes(r.into_body(), 64).await.unwrap()[..], b"tester");
        let r = app.oneshot(req(None)).await.unwrap();
        assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
    }

    /// The accept-1 matrix over real TLS, HTTP/1.1 and HTTP/2.
    mod over_tls {
        use super::*;
        use std::sync::Arc;

        use bytes::Bytes;
        use http_body_util::{BodyExt as _, Full};
        use hyper_util::rt::{TokioExecutor, TokioIo};
        use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
        use rustls::crypto::CryptoProvider;
        use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
        use rustls::{DigitallySignedStruct, SignatureScheme};

        /// Accepts exactly one certificate fingerprint (what `search --team` does).
        #[derive(Debug)]
        struct Pinned {
            fingerprint: String,
            provider: Arc<CryptoProvider>,
        }

        impl ServerCertVerifier for Pinned {
            fn verify_server_cert(
                &self,
                end_entity: &CertificateDer<'_>,
                _: &[CertificateDer<'_>],
                _: &ServerName<'_>,
                _: &[u8],
                _: UnixTime,
            ) -> std::result::Result<ServerCertVerified, rustls::Error> {
                if crate::tls::fingerprint(end_entity) == self.fingerprint {
                    Ok(ServerCertVerified::assertion())
                } else {
                    Err(rustls::Error::General("fingerprint mismatch".into()))
                }
            }

            fn verify_tls12_signature(
                &self,
                message: &[u8],
                cert: &CertificateDer<'_>,
                dss: &DigitallySignedStruct,
            ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
                rustls::crypto::verify_tls12_signature(
                    message,
                    cert,
                    dss,
                    &self.provider.signature_verification_algorithms,
                )
            }

            fn verify_tls13_signature(
                &self,
                message: &[u8],
                cert: &CertificateDer<'_>,
                dss: &DigitallySignedStruct,
            ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
                rustls::crypto::verify_tls13_signature(
                    message,
                    cert,
                    dss,
                    &self.provider.signature_verification_algorithms,
                )
            }

            fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
                self.provider.signature_verification_algorithms.supported_schemes()
            }
        }

        struct Client {
            port: u16,
            fingerprint: String,
            h2: bool,
        }

        struct Req<'a> {
            method: Method,
            path: &'a str,
            host: String,
            headers: Vec<(&'a str, String)>,
            body: Option<String>,
        }

        impl Client {
            fn req<'a>(&self, method: Method, path: &'a str) -> Req<'a> {
                Req {
                    method,
                    path,
                    host: format!("localhost:{}", self.port),
                    headers: vec![("user-agent", UA.into())],
                    body: None,
                }
            }

            async fn send(&self, r: Req<'_>) -> (StatusCode, HeaderMap, Bytes) {
                let provider = Arc::new(rustls::crypto::ring::default_provider());
                let mut cfg = rustls::ClientConfig::builder_with_provider(Arc::clone(&provider))
                    .with_safe_default_protocol_versions()
                    .unwrap()
                    .dangerous()
                    .with_custom_certificate_verifier(Arc::new(Pinned {
                        fingerprint: self.fingerprint.clone(),
                        provider,
                    }))
                    .with_no_client_auth();
                cfg.alpn_protocols = vec![if self.h2 { b"h2".to_vec() } else { b"http/1.1".to_vec() }];
                let tcp = tokio::net::TcpStream::connect(("127.0.0.1", self.port)).await.unwrap();
                let tls = tokio_rustls::TlsConnector::from(Arc::new(cfg))
                    .connect(ServerName::try_from("localhost").unwrap(), tcp)
                    .await
                    .unwrap();
                let io = TokioIo::new(tls);
                let mut b = axum::http::Request::builder().method(r.method);
                b = if self.h2 {
                    b.uri(format!("https://{}{}", r.host, r.path))
                } else {
                    b.uri(r.path).header(header::HOST, &r.host)
                };
                for (k, v) in &r.headers {
                    b = b.header(*k, v);
                }
                let req = b.body(Full::new(Bytes::from(r.body.unwrap_or_default()))).unwrap();
                let resp = if self.h2 {
                    let (mut s, conn) = hyper::client::conn::http2::handshake(TokioExecutor::new(), io)
                        .await
                        .unwrap();
                    tokio::spawn(conn);
                    s.send_request(req).await.unwrap()
                } else {
                    let (mut s, conn) = hyper::client::conn::http1::handshake(io).await.unwrap();
                    tokio::spawn(conn);
                    s.send_request(req).await.unwrap()
                };
                let want = if self.h2 {
                    axum::http::Version::HTTP_2
                } else {
                    axum::http::Version::HTTP_11
                };
                assert_eq!(resp.version(), want);
                let (parts, body) = resp.into_parts();
                (parts.status, parts.headers, body.collect().await.unwrap().to_bytes())
            }
        }

        #[test]
        fn request_matrix_over_h1_and_h2() {
            let dir = tempfile::tempdir().unwrap();
            let mut ctx = WebContext::for_tests(dir.path());
            ctx.config.server.bind = vec!["127.0.0.1:0".into()];
            let token = add_token(&ctx.paths.auth_file, "tester");
            let server = crate::server::spawn(ctx).unwrap();
            let port = server.info().port;
            let fingerprint = server.info().fingerprint.clone();
            assert_eq!(fingerprint.len(), 95);

            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async {
                for h2 in [false, true] {
                    let c = Client {
                        port,
                        fingerprint: fingerprint.clone(),
                        h2,
                    };
                    let origin = format!("https://localhost:{port}");
                    let (s, h, _) = c.send(c.req(Method::GET, "/healthz")).await;
                    assert_eq!(s, StatusCode::OK, "h2={h2}");
                    assert_eq!(h[header::CONTENT_SECURITY_POLICY], CSP);

                    // Bad host: 400 (h2 carries it only in :authority).
                    let mut r = c.req(Method::GET, "/healthz");
                    r.host = format!("evil.example:{port}");
                    assert_eq!(c.send(r).await.0, StatusCode::BAD_REQUEST, "h2={h2}");

                    let (s, _, _) = c.send(c.req(Method::GET, "/api/me")).await;
                    assert_eq!(s, StatusCode::UNAUTHORIZED);

                    let login = |origin: String| {
                        let mut r = c.req(Method::POST, "/api/login");
                        r.headers.push(("origin", origin));
                        r.headers.push(("content-type", "application/json".into()));
                        r.body = Some(serde_json::json!({ "token": token }).to_string());
                        r
                    };
                    // Origin mismatch, including another port of the same host: 403.
                    for bad in [format!("https://localhost:{}", port + 1), "https://evil.example".into()] {
                        assert_eq!(c.send(login(bad)).await.0, StatusCode::FORBIDDEN);
                    }
                    let (s, h, _) = c.send(login(origin.clone())).await;
                    assert_eq!(s, StatusCode::NO_CONTENT, "h2={h2}");
                    let cookie = h[header::SET_COOKIE]
                        .to_str()
                        .unwrap()
                        .split(';')
                        .next()
                        .unwrap()
                        .to_owned();
                    assert!(cookie.starts_with(&format!("__Host-cl-{port}=")));

                    let mut r = c.req(Method::GET, "/api/me");
                    r.headers.push(("cookie", cookie.clone()));
                    let (s, _, body) = c.send(r).await;
                    assert_eq!(s, StatusCode::OK);
                    assert_eq!(serde_json::from_slice::<Me>(&body).unwrap().fingerprint, fingerprint);

                    // Replayed with another User-Agent: 401, and the session is gone.
                    let mut r = c.req(Method::GET, "/api/me");
                    r.headers = vec![("user-agent", "curl/8".into()), ("cookie", cookie.clone())];
                    assert_eq!(c.send(r).await.0, StatusCode::UNAUTHORIZED);
                    let mut r = c.req(Method::GET, "/api/me");
                    r.headers.push(("cookie", cookie));
                    assert_eq!(c.send(r).await.0, StatusCode::UNAUTHORIZED);
                }
            });
            drop(rt);
            server.shutdown();
        }
    }
}
