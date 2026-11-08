//! Router assembly, listeners and the server thread (contracts; plan §7.1, §10.4).
//!
//! `cinelog serve` calls [`spawn`] next to the indexer daemon: it binds the listeners
//! (`server.bind`, `auto` port with up to 16 following ports probed, persisted in
//! meta.settings), loads TLS, writes `server.json` and serves HTTP/1.1 and HTTP/2 on its
//! own tokio runtime until [`ServerHandle::shutdown`].

use std::io;
use std::net::{IpAddr, SocketAddr, TcpListener};
use std::path::PathBuf;
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::Duration;

use axum::Json;
use axum::Router;
use axum::extract::Path;
use axum::middleware::{from_fn, from_fn_with_state};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::get;
use axum_server::tls_rustls::RustlsConfig;
use cinelog_core::server::{ServerInfo, auto_port};
use cinelog_index::model::settings_keys;
use socket2::{Domain, Protocol, Socket, Type};

use crate::api::types::Health;
use crate::error::{Result, WebError};
use crate::state::{AppState, Bound, WebContext};
use crate::{VERSION, api, assets, auth, export, headers, hosts, play, tls};

/// Ports probed after the first one when it is in use (§7.1).
pub const PORT_PROBES: u16 = 16;
const SHUTDOWN_GRACE: Duration = Duration::from_secs(5);

/// The complete application: public routes, `/api` (login public, everything else behind
/// `auth::require_session`) and the SPA fallback; Host/Origin checks and security headers
/// on every response.
pub fn router(state: AppState) -> Router {
    let protected = Router::new()
        .merge(auth::session_routes())
        .merge(api::routes())
        .merge(play::routes())
        .merge(export::routes())
        .route_layer(from_fn_with_state(state.clone(), auth::require_session));
    let api = Router::new()
        .merge(auth::routes())
        .merge(protected)
        .fallback(|| async { WebError::NotFound });
    Router::new()
        .route("/healthz", get(healthz))
        .route("/link/{uuid}", get(link))
        .nest("/api", api)
        .fallback(assets::spa)
        .layer(from_fn_with_state(state.clone(), hosts::check_origin))
        .layer(from_fn_with_state(state.clone(), hosts::check_host))
        .layer(from_fn(headers::security_headers))
        .with_state(state)
}

async fn healthz() -> Json<Health> {
    Json(Health {
        version: VERSION.to_owned(),
    })
}

/// `/link/{uuid}` → `/s/{uuid}` (the URL `cinelog link` printed before M3 used this form).
async fn link(Path(uuid): Path<String>) -> Response {
    match uuid::Uuid::try_parse(&uuid) {
        Ok(u) => Redirect::to(&format!("/s/{}", u.hyphenated())).into_response(),
        Err(_) => WebError::NotFound.into_response(),
    }
}

// ------------------------------------------------------------------ listeners

/// One `server.bind` entry: `<ip>:<port>`, `[<ipv6>]:<port>`; port `auto` or a number
/// (`0`: ephemeral, shared by every entry; tests).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BindSpec {
    pub ip: IpAddr,
    pub port: Option<u16>,
}

impl std::str::FromStr for BindSpec {
    type Err = WebError;
    fn from_str(s: &str) -> Result<Self> {
        let bad = || WebError::BadRequest(format!("server.bind entry {s:?} (expected ADDR:PORT or ADDR:auto)"));
        let (host, port) = s.trim().rsplit_once(':').ok_or_else(bad)?;
        let host = host.strip_prefix('[').and_then(|h| h.strip_suffix(']')).unwrap_or(host);
        let ip = host.parse().map_err(|_| bad())?;
        let port = match port {
            "auto" => None,
            p => Some(p.parse().map_err(|_| bad())?),
        };
        Ok(Self { ip, port })
    }
}

fn bind_one(addr: SocketAddr) -> io::Result<TcpListener> {
    let s = Socket::new(Domain::for_address(addr), Type::STREAM, Some(Protocol::TCP))?;
    if addr.is_ipv6() {
        // `0.0.0.0:p` and `[::]:p` are bound separately.
        s.set_only_v6(true)?;
    }
    s.set_reuse_address(true)?;
    s.bind(&addr.into())?;
    s.listen(1024)?;
    Ok(s.into())
}

/// Binds every spec. `auto` specs share one port: `first`, else the next free one of the
/// following [`PORT_PROBES`]. Addresses the host cannot bind at all (IPv6 disabled, an IP
/// that is gone) are skipped with a warning; it is an error when nothing binds.
pub fn bind_all(specs: &[BindSpec], first: u16) -> Result<(Vec<TcpListener>, u16)> {
    let mut out = Vec::new();
    let mut port = None;
    // Fixed and ephemeral ports first: an ephemeral port then fixes the port for `auto`.
    for spec in specs.iter().filter(|s| s.port.is_some()) {
        let want = match (spec.port, port) {
            (Some(0), Some(p)) => p,
            (Some(p), _) => p,
            (None, _) => unreachable!(),
        };
        match bind_one(SocketAddr::new(spec.ip, want)) {
            Ok(l) => {
                port = port.or(Some(l.local_addr()?.port()));
                out.push(l);
            }
            Err(e) if skippable(&e) => tracing::warn!("cannot listen on {}: {e}", spec.ip),
            Err(e) => {
                return Err(WebError::Io(io::Error::new(
                    e.kind(),
                    format!("{}:{want}: {e}", spec.ip),
                )));
            }
        }
    }
    let autos: Vec<IpAddr> = specs.iter().filter(|s| s.port.is_none()).map(|s| s.ip).collect();
    if !autos.is_empty() {
        let candidates: Vec<u16> = match port {
            Some(p) => vec![p],
            None => (0..=PORT_PROBES).filter_map(|i| first.checked_add(i)).collect(),
        };
        let mut last_err = None;
        'ports: for p in candidates {
            let mut bound = Vec::new();
            for ip in &autos {
                match bind_one(SocketAddr::new(*ip, p)) {
                    Ok(l) => bound.push(l),
                    Err(e) if e.kind() == io::ErrorKind::AddrInUse => {
                        last_err = Some(e);
                        continue 'ports;
                    }
                    Err(e) if skippable(&e) => tracing::warn!("cannot listen on {ip}: {e}"),
                    Err(e) => return Err(WebError::Io(io::Error::new(e.kind(), format!("{ip}:{p}: {e}")))),
                }
            }
            if !bound.is_empty() {
                port = Some(p);
                out.extend(bound);
                break;
            }
        }
        if port.is_none() {
            let why = last_err.map_or_else(|| "no usable address".to_owned(), |e| e.to_string());
            return Err(WebError::Io(io::Error::other(format!(
                "no free port in {first}..={}: {why}",
                first.saturating_add(PORT_PROBES)
            ))));
        }
    }
    match (out.is_empty(), port) {
        (false, Some(p)) => Ok((out, p)),
        _ => Err(WebError::Io(io::Error::other("server.bind: nothing could be bound"))),
    }
}

fn skippable(e: &io::Error) -> bool {
    matches!(e.raw_os_error(), Some(libc::EADDRNOTAVAIL | libc::EAFNOSUPPORT))
}

// ------------------------------------------------------------------ the server thread

/// A running web server; dropping it without [`Self::shutdown`] leaves it running until
/// the process exits.
pub struct ServerHandle {
    info: ServerInfo,
    runtime_dir: PathBuf,
    handle: axum_server::Handle<SocketAddr>,
    thread: Option<JoinHandle<()>>,
}

impl ServerHandle {
    /// What `server.json` holds.
    pub fn info(&self) -> &ServerInfo {
        &self.info
    }

    /// Graceful shutdown (5 s grace), then removes `server.json` if it is still ours.
    pub fn shutdown(mut self) {
        self.handle.graceful_shutdown(Some(SHUTDOWN_GRACE));
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        if ServerInfo::read_live(&self.runtime_dir).is_some_and(|i| i.pid == self.info.pid) {
            let _ = std::fs::remove_file(ServerInfo::path(&self.runtime_dir));
        }
    }
}

/// Starts the server of `ctx`'s scope; returns once it listens.
pub fn spawn(ctx: WebContext) -> Result<ServerHandle> {
    tls::install_provider();
    let specs = ctx
        .config
        .server
        .bind
        .iter()
        .map(|s| s.parse())
        .collect::<Result<Vec<BindSpec>>>()?;
    let hosts = hosts::HostAllowlist::new(&ctx);
    let material = tls::load_or_create(&tls::TlsOptions::from_context(&ctx, hosts.cert_names()))?;
    let fingerprint = material.info.fingerprint.clone();

    let mut meta = cinelog_index::db::open_meta(&ctx.db)?;
    let persisted = meta.setting(settings_keys::PORT)?.and_then(|p| p.parse::<u16>().ok());
    let first = persisted.unwrap_or_else(|| auto_port(ctx.scope, ctx.uid));
    let (listeners, port) = bind_all(&specs, first)?;
    if specs.iter().any(|s| s.port.is_none()) && persisted != Some(port) {
        meta.set_setting(settings_keys::PORT, Some(&port.to_string()))?;
    }
    if let Some(reason) = material.regenerated {
        tracing::warn!("new self-signed certificate ({reason:?}), fingerprint {fingerprint}");
    }
    let stored = meta.setting(settings_keys::CERT_FINGERPRINT)?;
    if stored.as_deref() != Some(fingerprint.as_str()) {
        if stored.is_some() && material.regenerated.is_none() {
            tracing::warn!("the TLS certificate changed, fingerprint {fingerprint}");
        }
        meta.set_setting(settings_keys::CERT_FINGERPRINT, Some(&fingerprint))?;
    }
    drop(meta);

    let url_host = ctx
        .config
        .server
        .hostnames
        .first()
        .cloned()
        .unwrap_or_else(|| ctx.host.clone());
    let url = format!("https://{}:{port}", bracket_v6(&url_host));
    let runtime_dir = ctx.paths.runtime_dir.clone();
    let info = ServerInfo {
        pid: std::process::id(),
        url: url.clone(),
        port,
        fingerprint: fingerprint.clone(),
        version: VERSION.to_owned(),
    };
    let state = AppState::new(ctx, Bound { port, fingerprint, url }, hosts);
    let app = router(state.clone());
    let handle = axum_server::Handle::new();
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(std::thread::available_parallelism().map_or(2, |n| n.get().clamp(2, 4)))
        .thread_name("cinelog-web")
        .enable_all()
        .build()?;
    let tls_config = RustlsConfig::from_config(material.config);
    let (ready_tx, ready_rx) = mpsc::channel::<io::Result<()>>();
    let thread_handle = handle.clone();
    let thread = std::thread::Builder::new().name("cinelog-web".into()).spawn(move || {
        rt.block_on(async move {
            state.hosts.start_refresh();
            let mut servers = Vec::new();
            for l in listeners {
                match axum_server::from_tcp_rustls(l, tls_config.clone()) {
                    Ok(s) => servers.push(
                        s.handle(thread_handle.clone())
                            .serve(app.clone().into_make_service_with_connect_info::<SocketAddr>()),
                    ),
                    Err(e) => {
                        let _ = ready_tx.send(Err(e));
                        return;
                    }
                }
            }
            let _ = ready_tx.send(Ok(()));
            for r in futures_util::future::join_all(servers).await {
                if let Err(e) = r {
                    tracing::error!("web server: {e}");
                }
            }
        });
    })?;
    match ready_rx.recv() {
        Ok(Ok(())) => {}
        Ok(Err(e)) => return Err(e.into()),
        Err(_) => return Err(WebError::Internal("the web server thread exited".into())),
    }
    if let Err(e) = info.write(&runtime_dir) {
        tracing::warn!("cannot write {}: {e}", ServerInfo::path(&runtime_dir).display());
    }
    tracing::info!("web UI at {} (certificate {})", info.url, info.fingerprint);
    Ok(ServerHandle {
        info,
        runtime_dir,
        handle,
        thread: Some(thread),
    })
}

fn bracket_v6(host: &str) -> String {
    match host.parse::<IpAddr>() {
        Ok(IpAddr::V6(_)) => format!("[{host}]"),
        _ => host.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    fn spec(s: &str) -> BindSpec {
        s.parse().unwrap()
    }

    #[test]
    fn bind_specs() {
        assert_eq!(
            spec("0.0.0.0:auto"),
            BindSpec {
                ip: Ipv4Addr::UNSPECIFIED.into(),
                port: None
            }
        );
        assert_eq!(spec("[::]:auto").ip, "::".parse::<IpAddr>().unwrap());
        assert_eq!(spec("127.0.0.1:10443").port, Some(10443));
        for bad in ["", "auto", "box.lan:auto", "1.2.3.4:x", "1.2.3.4:70000"] {
            assert!(bad.parse::<BindSpec>().is_err(), "{bad}");
        }
    }

    #[test]
    fn ephemeral_port_is_shared_and_auto_probes() {
        let (ls, port) = bind_all(&[spec("127.0.0.1:0")], 1).unwrap();
        assert_ne!(port, 0);
        assert_eq!(ls[0].local_addr().unwrap().port(), port);
        // The port is taken: `auto` starting there moves on to a following port.
        let (ls2, port2) = bind_all(&[spec("127.0.0.1:auto")], port).unwrap();
        assert!(
            port2 > port && port2 <= port.saturating_add(PORT_PROBES),
            "{port} → {port2}"
        );
        drop((ls, ls2));
        // A fixed port that is taken is an error, not a probe.
        let (held, p) = bind_all(&[spec("127.0.0.1:0")], 1).unwrap();
        assert!(
            bind_all(
                &[BindSpec {
                    ip: Ipv4Addr::LOCALHOST.into(),
                    port: Some(p)
                }],
                1
            )
            .is_err()
        );
        drop(held);
    }

    #[test]
    fn ipv4_and_ipv6_share_a_port() {
        let specs = [spec("127.0.0.1:0"), spec("[::1]:0")];
        let (ls, port) = bind_all(&specs, 1).unwrap();
        assert!(!ls.is_empty());
        assert!(ls.iter().all(|l| l.local_addr().unwrap().port() == port));
    }

    #[tokio::test]
    async fn public_handlers() {
        assert_eq!(healthz().await.0.version, VERSION);
        let r = link(Path("0193A1B2-0000-7000-8000-000000000001".into())).await;
        assert_eq!(r.status(), axum::http::StatusCode::SEE_OTHER);
        assert_eq!(r.headers()["location"], "/s/0193a1b2-0000-7000-8000-000000000001");
        let r = link(Path("../x".into())).await;
        assert_eq!(r.status(), axum::http::StatusCode::NOT_FOUND);
    }

    #[test]
    fn router_builds_with_state() {
        let dir = tempfile::tempdir().unwrap();
        let _ = router(AppState::for_tests(dir.path()));
        assert_eq!(bracket_v6("::1"), "[::1]");
        assert_eq!(bracket_v6("box"), "box");
    }
}
