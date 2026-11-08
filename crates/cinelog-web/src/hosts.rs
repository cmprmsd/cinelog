//! Host allowlist and Origin checks (work package M3-A; plan §7.3, docs/API.md §3).
//!
//! - [`HostAllowlist::new`]: `server.hostnames` plus the hostname, FQDN, current local IPs
//!   (getifaddrs) and `localhost`/`127.0.0.1`/`::1`; [`HostAllowlist::refresh`] re-reads the
//!   local names, [`HostAllowlist::start_refresh`] does so every 60 s on the current tokio
//!   runtime;
//! - [`HostAllowlist::cert_names`]: the SAN list for a self-signed certificate (DNS names
//!   and IP literals), used by `tls::TlsOptions::from_context`. It has only the *stable*
//!   local IPs: IPv6 temporary (privacy), deprecated or DAD-failed addresses and addresses
//!   of virtual interfaces (container bridges, veth, VPN tunnels) are left out, because
//!   every new name forces a regeneration with `tls.auto_regen` and so a new fingerprint.
//!   Those addresses are still allowed as Host; to put one in the certificate, list it in
//!   `server.hostnames`;
//! - [`request_host`]: the host of a request from `uri().authority()` first (HTTP/2 has no
//!   Host header), else the `Host` header; port stripped, IPv6 brackets removed, lowercase;
//! - [`check_host`]: middleware, 400 (`bad_request`) for a host outside the list;
//! - [`check_origin`]: middleware for non-GET/HEAD/OPTIONS requests: `Origin` must equal
//!   `https://<authority of this request>` exactly (403 `forbidden`), and the content type
//!   must be `application/json` (415) for requests with a body.

use std::collections::{BTreeSet, HashMap};
use std::ffi::{CStr, CString};
use std::net::{IpAddr, Ipv6Addr};
use std::sync::{Arc, RwLock, Weak};
use std::time::Duration;

use axum::body::HttpBody as _;
use axum::extract::{Request, State};
use axum::http::{Method, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

use crate::error::WebError;
use crate::state::{AppState, WebContext};

/// How often [`HostAllowlist::start_refresh`] re-reads the local names.
pub const REFRESH_EVERY: Duration = Duration::from_secs(60);

/// Always allowed and always in the certificate.
const LOOPBACK: [&str; 3] = ["localhost", "127.0.0.1", "::1"];

/// Interface name prefixes of virtual links whose addresses come and go (containers, VMs,
/// VPNs); their addresses stay out of the certificate.
const VIRTUAL_IFACES: [&str; 21] = [
    "docker",
    "br-",
    "veth",
    "virbr",
    "vnet",
    "vmnet",
    "vboxnet",
    "lxcbr",
    "lxdbr",
    "incusbr",
    "podman",
    "cni",
    "flannel",
    "cali",
    "kube",
    "tun",
    "tap",
    "wg",
    "tailscale",
    "zt",
    "ppp",
];

/// `ifa_flags` (linux/if_addr.h) that mark an IPv6 address as transient.
const IFA_F_TEMPORARY: u32 = 0x01;
const IFA_F_DADFAILED: u32 = 0x08;
const IFA_F_DEPRECATED: u32 = 0x20;
const IFA_F_UNSTABLE: u32 = IFA_F_TEMPORARY | IFA_F_DADFAILED | IFA_F_DEPRECATED;

/// Names that come from the machine and can change while the server runs.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct LocalNames {
    /// Hostname, FQDN and `WebContext::host`, lowercase.
    dns: BTreeSet<String>,
    /// Every address of an interface that is up (Host allowlist).
    ips: BTreeSet<IpAddr>,
    /// The stable subset of `ips` that goes into the certificate.
    cert_ips: BTreeSet<IpAddr>,
}

impl LocalNames {
    fn read(ctx_host: &str) -> Self {
        let mut dns = BTreeSet::new();
        let mut add = |n: &str| {
            if let Some(n) = normalize_dns(n) {
                dns.insert(n);
            }
        };
        add(ctx_host);
        if let Ok(h) = nix::unistd::gethostname() {
            let h = h.to_string_lossy();
            add(&h);
            if let Some(fqdn) = canonical_name(&h) {
                add(&fqdn);
            }
        }
        let (ips, cert_ips) = local_ips();
        Self { dns, ips, cert_ips }
    }
}

#[derive(Debug, Clone)]
pub struct HostAllowlist {
    /// `server.hostnames`, normalized; DNS names or IP literals.
    configured: Vec<String>,
    ctx_host: String,
    local: Arc<RwLock<LocalNames>>,
}

impl HostAllowlist {
    pub fn new(ctx: &WebContext) -> Self {
        Self::with_local(&ctx.config.server.hostnames, &ctx.host, LocalNames::read(&ctx.host))
    }

    fn with_local(hostnames: &[String], ctx_host: &str, local: LocalNames) -> Self {
        let mut configured: Vec<String> = Vec::new();
        for h in hostnames.iter().filter_map(|h| normalize_host(h)) {
            if !configured.contains(&h) {
                configured.push(h);
            }
        }
        Self {
            configured,
            ctx_host: ctx_host.to_owned(),
            local: Arc::new(RwLock::new(local)),
        }
    }

    /// `host` as [`request_host`] returns it (any case, a trailing dot is ignored).
    pub fn allows(&self, host: &str) -> bool {
        let Some(host) = normalize_host(host) else {
            return false;
        };
        if self.configured.contains(&host) || LOOPBACK.contains(&host.as_str()) {
            return true;
        }
        let local = self.local.read().unwrap_or_else(|e| e.into_inner());
        match host.parse::<IpAddr>() {
            Ok(ip) => local.ips.contains(&ip),
            Err(_) => local.dns.contains(&host),
        }
    }

    /// Re-reads hostname, FQDN and interface addresses (blocking: the FQDN lookup may ask
    /// the resolver).
    pub fn refresh(&self) {
        let fresh = LocalNames::read(&self.ctx_host);
        let mut local = self.local.write().unwrap_or_else(|e| e.into_inner());
        if *local != fresh {
            tracing::debug!("local names changed: {:?} {:?}", fresh.dns, fresh.ips);
            *local = fresh;
        }
    }

    /// Refreshes every [`REFRESH_EVERY`] on the current tokio runtime until the allowlist
    /// is dropped. Without a runtime it does nothing.
    pub fn start_refresh(&self) {
        let Ok(rt) = tokio::runtime::Handle::try_current() else {
            tracing::warn!("host allowlist: no tokio runtime, local names are not refreshed");
            return;
        };
        let weak: Weak<RwLock<LocalNames>> = Arc::downgrade(&self.local);
        let ctx_host = self.ctx_host.clone();
        rt.spawn(async move {
            let mut tick = tokio::time::interval(REFRESH_EVERY);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            tick.tick().await;
            loop {
                tick.tick().await;
                let host = ctx_host.clone();
                let Ok(fresh) = tokio::task::spawn_blocking(move || LocalNames::read(&host)).await else {
                    continue;
                };
                let Some(local) = weak.upgrade() else { break };
                let mut local = local.write().unwrap_or_else(|e| e.into_inner());
                if *local != fresh {
                    tracing::debug!("local names changed: {:?} {:?}", fresh.dns, fresh.ips);
                    *local = fresh;
                }
            }
        });
    }

    /// Configured names, hostname, FQDN, stable local IPs and the loopback names, without
    /// duplicates (DNS names first, then IPs).
    pub fn cert_names(&self) -> Vec<String> {
        let local = self.local.read().unwrap_or_else(|e| e.into_inner());
        let mut dns: Vec<String> = Vec::new();
        let mut ips: Vec<IpAddr> = Vec::new();
        let configured = self.configured.iter().map(String::as_str);
        let all = configured
            .chain(local.dns.iter().map(String::as_str))
            .chain(LOOPBACK)
            .map(str::to_owned)
            .chain(local.cert_ips.iter().map(IpAddr::to_string));
        for name in all {
            match name.parse::<IpAddr>() {
                Ok(ip) if !ips.contains(&ip) => ips.push(ip),
                Ok(_) => {}
                Err(_) if !dns.contains(&name) => dns.push(name),
                Err(_) => {}
            }
        }
        dns.into_iter().chain(ips.iter().map(IpAddr::to_string)).collect()
    }
}

/// The authority of a request as the client sent it (`uri` first, else `Host`).
fn request_authority(req: &Request) -> Option<String> {
    if let Some(a) = req.uri().authority() {
        return Some(a.as_str().to_owned());
    }
    let host = req.headers().get(header::HOST)?.to_str().ok()?;
    let host = host.trim();
    (!host.is_empty()).then(|| host.to_owned())
}

pub fn request_host(req: &Request) -> Option<String> {
    host_of_authority(&request_authority(req)?)
}

/// `box.lan:443` → `box.lan`, `[::1]:443` → `::1`; lowercase. `None` for userinfo, a bad
/// port or an empty host.
fn host_of_authority(authority: &str) -> Option<String> {
    if authority.contains('@') {
        return None;
    }
    let (host, port) = if let Some(rest) = authority.strip_prefix('[') {
        let (h, after) = rest.split_once(']')?;
        h.parse::<std::net::Ipv6Addr>().ok()?;
        let port = match after {
            "" => None,
            p => Some(p.strip_prefix(':')?),
        };
        (h, port)
    } else {
        match authority.rsplit_once(':') {
            Some((h, p)) => (h, Some(p)),
            None => (authority, None),
        }
    };
    if let Some(p) = port
        && !p.is_empty()
        && p.parse::<u16>().is_err()
    {
        return None;
    }
    if host.is_empty() || (host.contains(':') && !authority.starts_with('[')) {
        return None;
    }
    Some(host.to_ascii_lowercase())
}

/// Lowercase, without a trailing dot; IP literals in canonical form.
fn normalize_host(h: &str) -> Option<String> {
    let h = h.trim().trim_end_matches('.');
    let h = h.strip_prefix('[').and_then(|x| x.strip_suffix(']')).unwrap_or(h);
    if h.is_empty() {
        return None;
    }
    match h.parse::<IpAddr>() {
        Ok(ip) => Some(ip.to_canonical().to_string()),
        Err(_) => Some(h.to_ascii_lowercase()),
    }
}

/// A usable DNS name (no IP literal, no whitespace).
fn normalize_dns(h: &str) -> Option<String> {
    let n = normalize_host(h)?;
    (n.parse::<IpAddr>().is_err() && !n.chars().any(|c| c.is_whitespace() || c == '/')).then_some(n)
}

pub async fn check_host(State(state): State<AppState>, req: Request, next: Next) -> Response {
    match request_host(&req) {
        Some(h) if state.hosts.allows(&h) => next.run(req).await,
        _ => WebError::BadRequest("this host name is not served here".into()).into_response(),
    }
}

pub async fn check_origin(State(_): State<AppState>, req: Request, next: Next) -> Response {
    match origin_problem(&req) {
        None => next.run(req).await,
        Some(e) => e.into_response(),
    }
}

/// Why a request fails the Origin / Content-Type rule, if it does.
fn origin_problem(req: &Request) -> Option<WebError> {
    if matches!(*req.method(), Method::GET | Method::HEAD | Method::OPTIONS) {
        return None;
    }
    let origin = req.headers().get(header::ORIGIN).and_then(|o| o.to_str().ok());
    let expected = request_authority(req).map(|a| format!("https://{a}"));
    match (origin, expected) {
        (Some(o), Some(e)) if o.eq_ignore_ascii_case(&e) => {}
        _ => return Some(WebError::Forbidden("cross-origin request refused".into())),
    }
    if has_body(req) && !is_json(req) {
        return Some(WebError::UnsupportedMediaType);
    }
    None
}

fn has_body(req: &Request) -> bool {
    let length = req
        .headers()
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<u64>().ok());
    match length {
        Some(n) => n > 0,
        None => req.headers().contains_key(header::TRANSFER_ENCODING) || !req.body().is_end_stream(),
    }
}

fn is_json(req: &Request) -> bool {
    req.headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(';').next())
        .is_some_and(|m| m.trim().eq_ignore_ascii_case("application/json"))
}

/// The canonical name of `host` (`AI_CANONNAME`), as `hostname -f` prints it.
fn canonical_name(host: &str) -> Option<String> {
    let c_host = CString::new(host).ok()?;
    // SAFETY: a zeroed addrinfo is a valid "no hints" value; only the fields below are set.
    let mut hints: libc::addrinfo = unsafe { std::mem::zeroed() };
    hints.ai_flags = libc::AI_CANONNAME;
    hints.ai_family = libc::AF_UNSPEC;
    hints.ai_socktype = libc::SOCK_STREAM;
    let mut res: *mut libc::addrinfo = std::ptr::null_mut();
    // SAFETY: valid NUL-terminated node, null service, valid hints and out pointer.
    let rc = unsafe { libc::getaddrinfo(c_host.as_ptr(), std::ptr::null(), &hints, &mut res) };
    if rc != 0 || res.is_null() {
        return None;
    }
    // SAFETY: on success `res` points to a list owned by libc until freeaddrinfo; the first
    // entry carries ai_canonname (a NUL-terminated string) when AI_CANONNAME was requested.
    let name = unsafe {
        let canon = (*res).ai_canonname;
        let name = (!canon.is_null()).then(|| CStr::from_ptr(canon).to_string_lossy().into_owned());
        libc::freeaddrinfo(res);
        name
    };
    name.filter(|n| !n.is_empty() && !n.eq_ignore_ascii_case(host))
}

/// Addresses of every interface that is up, except IPv6 link-local ones (a URL cannot
/// carry them without a zone), and the subset of them that is stable ([`cert_worthy`]).
fn local_ips() -> (BTreeSet<IpAddr>, BTreeSet<IpAddr>) {
    let mut all = BTreeSet::new();
    let mut stable = BTreeSet::new();
    let Ok(addrs) = nix::ifaddrs::getifaddrs() else {
        return (all, stable);
    };
    // getifaddrs has no IPv6 address flags; the kernel lists them here.
    let v6_flags = std::fs::read_to_string("/proc/net/if_inet6")
        .ok()
        .map(|t| parse_if_inet6(&t));
    for a in addrs {
        if !a.flags.contains(nix::net::if_::InterfaceFlags::IFF_UP) {
            continue;
        }
        let Some(addr) = a.address else { continue };
        let ip = if let Some(v4) = addr.as_sockaddr_in() {
            IpAddr::V4(v4.ip())
        } else if let Some(v6) = addr.as_sockaddr_in6() {
            let ip = v6.ip();
            if ip.segments()[0] & 0xffc0 == 0xfe80 {
                continue;
            }
            IpAddr::V6(ip)
        } else {
            continue;
        };
        let ip = ip.to_canonical();
        all.insert(ip);
        if cert_worthy(&a.interface_name, ip, v6_flags.as_ref()) {
            stable.insert(ip);
        }
    }
    (all, stable)
}

/// Whether an address of `iface` belongs in the certificate: not on a virtual interface
/// and, for IPv6, known to be neither temporary, deprecated nor DAD-failed (without the
/// flags, e.g. no /proc, IPv6 addresses are left out).
fn cert_worthy(iface: &str, ip: IpAddr, v6_flags: Option<&HashMap<Ipv6Addr, u32>>) -> bool {
    if VIRTUAL_IFACES.iter().any(|p| iface.starts_with(p)) {
        return false;
    }
    match ip {
        IpAddr::V4(_) => true,
        IpAddr::V6(v6) => v6_flags
            .and_then(|m| m.get(&v6))
            .is_some_and(|f| f & IFA_F_UNSTABLE == 0),
    }
}

/// `/proc/net/if_inet6` lines: address (32 hex digits), ifindex, prefix length, scope,
/// flags (hex), interface name.
fn parse_if_inet6(text: &str) -> HashMap<Ipv6Addr, u32> {
    text.lines()
        .filter_map(|line| {
            let mut f = line.split_whitespace();
            let addr = u128::from_str_radix(f.next()?, 16).ok()?;
            let flags = u32::from_str_radix(f.nth(3)?, 16).ok()?;
            Some((Ipv6Addr::from(addr), flags))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::Router;
    use axum::body::Body;
    use axum::http::StatusCode;
    use axum::routing::{get, post};
    use tower::ServiceExt as _;

    fn list(hostnames: &[&str]) -> HostAllowlist {
        let local = LocalNames {
            dns: ["box".to_owned(), "box.example.org".to_owned()].into(),
            ips: [
                "192.168.1.20".parse().unwrap(),
                "2001:db8::7".parse().unwrap(),
                "2001:db8::abcd:1234".parse().unwrap(),
                "172.17.0.1".parse().unwrap(),
            ]
            .into(),
            cert_ips: ["192.168.1.20".parse().unwrap(), "2001:db8::7".parse().unwrap()].into(),
        };
        let hostnames: Vec<String> = hostnames.iter().map(|s| (*s).to_owned()).collect();
        HostAllowlist::with_local(&hostnames, "box", local)
    }

    #[test]
    fn allowlist() {
        let l = list(&["Cinelog-Team.LAN.", "10.9.9.9"]);
        for ok in [
            "cinelog-team.lan",
            "CINELOG-TEAM.lan.",
            "10.9.9.9",
            "box",
            "BOX.example.org",
            "192.168.1.20",
            "2001:db8::7",
            "2001:DB8:0::7",
            "2001:db8::abcd:1234",
            "172.17.0.1",
            "localhost",
            "127.0.0.1",
            "::1",
            "::ffff:127.0.0.1",
        ] {
            assert!(l.allows(ok), "{ok}");
        }
        for bad in [
            "",
            "evil.example",
            "box.evil",
            "192.168.1.21",
            "2001:db8::8",
            "0.0.0.0",
            "127.0.0.2",
        ] {
            assert!(!l.allows(bad), "{bad}");
        }
    }

    #[test]
    fn cert_names_cover_every_allowed_name() {
        let l = list(&["team.lan", "10.9.9.9", "team.lan"]);
        let names = l.cert_names();
        assert_eq!(
            names,
            [
                "team.lan",
                "box",
                "box.example.org",
                "localhost",
                "10.9.9.9",
                "127.0.0.1",
                "::1",
                "192.168.1.20",
                "2001:db8::7"
            ]
        );
    }

    /// Regression: privacy addresses and container bridges went into the certificate, so
    /// with `tls.auto_regen` nearly every start regenerated it (new fingerprint).
    #[test]
    fn transient_addresses_stay_out_of_the_certificate() {
        let l = list(&[]);
        let names = l.cert_names();
        assert!(!names.contains(&"2001:db8::abcd:1234".to_owned()), "{names:?}");
        assert!(!names.contains(&"172.17.0.1".to_owned()));
        let proc = "\
20010db8000000000000000000000007 02 40 00 80 enp39s0
20010db800000000000000000000abcd 02 40 00 01 enp39s0
20010db800000000000000000000dead 02 40 00 20 enp39s0
20010db800000000000000000000beef 02 40 00 08 enp39s0
fddaab53d2e700000000000000000001 11 40 00 82 br-946248f3946f
00000000000000000000000000000001 01 80 10 80       lo
bad line
";
        let flags = parse_if_inet6(proc);
        assert_eq!(flags.len(), 6);
        assert_eq!(flags[&"2001:db8::abcd".parse::<Ipv6Addr>().unwrap()], 0x01);
        let ok = |iface: &str, ip: &str| cert_worthy(iface, ip.parse().unwrap(), Some(&flags));
        assert!(ok("enp39s0", "2001:db8::7"), "stable SLAAC/static");
        assert!(ok("lo", "::1"));
        assert!(!ok("enp39s0", "2001:db8::abcd"), "temporary");
        assert!(!ok("enp39s0", "2001:db8::dead"), "deprecated");
        assert!(!ok("enp39s0", "2001:db8::beef"), "DAD failed");
        assert!(!ok("enp39s0", "2001:db8::1:2"), "unknown flags");
        assert!(!ok("br-946248f3946f", "fdda:ab53:d2e7::1"), "bridge");
        assert!(ok("enp39s0", "192.168.1.20"));
        assert!(ok("wlan0", "10.0.0.5"));
        for v in [
            "docker0",
            "veth12ab",
            "virbr0",
            "vmnet8",
            "incusbr0",
            "tun0",
            "wg0",
            "tailscale0",
        ] {
            assert!(!ok(v, "172.17.0.1"), "{v}");
        }
        assert!(
            !cert_worthy("eth0", "2001:db8::7".parse().unwrap(), None),
            "no flags known"
        );
    }

    #[test]
    fn real_local_names() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = WebContext::for_tests(dir.path());
        let l = HostAllowlist::new(&ctx);
        assert!(l.allows("testhost"));
        let host = nix::unistd::gethostname().unwrap().to_string_lossy().into_owned();
        assert!(l.allows(&host), "{host}");
        let names = l.cert_names();
        assert!(names.contains(&"127.0.0.1".to_owned()), "{names:?}");
        assert!(names.contains(&"localhost".to_owned()));
        let local = l.local.read().unwrap();
        assert!(local.cert_ips.is_subset(&local.ips));
        drop(local);
        l.refresh();
        assert!(l.allows(&host));
    }

    #[tokio::test]
    async fn refresh_task_stops_with_the_list() {
        let l = list(&[]);
        l.start_refresh();
        let weak = Arc::downgrade(&l.local);
        drop(l);
        assert!(weak.upgrade().is_none(), "the task holds no strong reference");
    }

    fn req(method: Method, uri: &str, headers: &[(&str, &str)]) -> Request {
        let mut b = Request::builder().method(method).uri(uri);
        for (k, v) in headers {
            b = b.header(*k, *v);
        }
        b.body(Body::empty()).unwrap()
    }

    #[test]
    fn hosts_of_requests() {
        let h = |uri: &str, host: Option<&str>| {
            let hs: Vec<(&str, &str)> = host.map(|h| ("host", h)).into_iter().collect();
            request_host(&req(Method::GET, uri, &hs))
        };
        assert_eq!(h("/x", Some("Box.LAN:10443")).as_deref(), Some("box.lan"));
        assert_eq!(h("/x", Some("[::1]:10443")).as_deref(), Some("::1"));
        assert_eq!(h("/x", Some("[::1]")).as_deref(), Some("::1"));
        assert_eq!(h("/x", Some("10.0.0.1")).as_deref(), Some("10.0.0.1"));
        // HTTP/2 (and absolute-form h1): the URI authority wins over Host.
        assert_eq!(h("https://box:1/x", Some("evil")).as_deref(), Some("box"));
        assert_eq!(h("https://[2001:db8::7]:1/x", None).as_deref(), Some("2001:db8::7"));
        assert_eq!(h("/x", None), None);
        for bad in ["", "box:x", "box:99999", "::1", "[::1", "[box]:1", "[::1]x", "u@box"] {
            assert_eq!(h("/x", Some(bad)), None, "{bad}");
        }
    }

    #[test]
    fn origin_rules() {
        let p = |m: Method, headers: &[(&str, &str)]| origin_problem(&req(m, "/api/forget", headers));
        let json = ("content-type", "application/json; charset=utf-8");
        let host = ("host", "box:10443");
        let len = ("content-length", "2");
        assert!(p(Method::GET, &[host]).is_none());
        assert!(p(Method::HEAD, &[host, ("origin", "https://evil")]).is_none());
        assert!(p(Method::OPTIONS, &[host]).is_none());
        assert!(p(Method::POST, &[host, ("origin", "https://box:10443"), json, len]).is_none());
        assert!(p(Method::POST, &[host, ("origin", "https://BOX:10443"), json, len]).is_none());
        // No body: no Content-Type needed.
        assert!(p(Method::POST, &[host, ("origin", "https://box:10443")]).is_none());
        let forbidden = |e: Option<WebError>| matches!(e, Some(WebError::Forbidden(_)));
        assert!(forbidden(p(Method::POST, &[host, json, len])));
        for o in [
            "https://box:10444",
            "https://box",
            "http://box:10443",
            "https://box.lan:10443",
            "https://box:10443/",
            "null",
        ] {
            assert!(forbidden(p(Method::POST, &[host, ("origin", o), json, len])), "{o}");
        }
        assert!(forbidden(p(Method::DELETE, &[host, ("origin", "https://evil")])));
        let media = |e: Option<WebError>| matches!(e, Some(WebError::UnsupportedMediaType));
        let ok_origin = ("origin", "https://box:10443");
        assert!(media(p(Method::POST, &[host, ok_origin, len])));
        assert!(media(p(
            Method::POST,
            &[host, ok_origin, len, ("content-type", "text/plain")]
        )));
        assert!(media(p(
            Method::PATCH,
            &[
                host,
                ok_origin,
                ("transfer-encoding", "chunked"),
                ("content-type", "application/jsonx")
            ]
        )));
        // h2: the authority comes from the URI.
        let h2 = origin_problem(&req(Method::POST, "https://box:10443/api/logout", &[ok_origin]));
        assert!(h2.is_none());
    }

    #[tokio::test]
    async fn body_without_length_needs_json() {
        let r = Request::post("/x")
            .header("host", "box:1")
            .header("origin", "https://box:1")
            .body(Body::from("{}"))
            .unwrap();
        assert!(matches!(origin_problem(&r), Some(WebError::UnsupportedMediaType)));
    }

    #[tokio::test]
    async fn middlewares_in_a_router() {
        let dir = tempfile::tempdir().unwrap();
        let state = AppState::for_tests(dir.path());
        let app = Router::new()
            .route("/r", get(|| async { "ok" }))
            .route("/w", post(|| async { "ok" }))
            .layer(axum::middleware::from_fn_with_state(state.clone(), check_origin))
            .layer(axum::middleware::from_fn_with_state(state.clone(), check_host))
            .with_state(state);
        let call = |r: Request| {
            let app = app.clone();
            async move { app.oneshot(r).await.unwrap().status() }
        };
        assert_eq!(
            call(req(Method::GET, "/r", &[("host", "localhost:1")])).await,
            StatusCode::OK
        );
        assert_eq!(
            call(req(Method::GET, "/r", &[("host", "testhost")])).await,
            StatusCode::OK
        );
        assert_eq!(
            call(req(Method::GET, "/r", &[("host", "evil.example")])).await,
            StatusCode::BAD_REQUEST
        );
        assert_eq!(call(req(Method::GET, "/r", &[])).await, StatusCode::BAD_REQUEST);
        assert_eq!(
            call(req(
                Method::POST,
                "/w",
                &[("host", "localhost:1"), ("origin", "https://localhost:2")]
            ))
            .await,
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            call(req(
                Method::POST,
                "/w",
                &[("host", "localhost:1"), ("origin", "https://localhost:1")]
            ))
            .await,
            StatusCode::OK
        );
        // Bad host first: 400 even with a bad Origin.
        assert_eq!(
            call(req(Method::POST, "/w", &[("host", "evil"), ("origin", "https://x")])).await,
            StatusCode::BAD_REQUEST
        );
    }
}
