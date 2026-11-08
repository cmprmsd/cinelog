//! TLS material (work package M3-A; plan §7.1).
//!
//! - [`install_provider`]: installs rustls' ring provider as the process default
//!   (idempotent; called before any rustls use);
//! - [`load_or_create`]: the user cert/key (`server.tls_cert`/`tls_key`) or the self-signed
//!   pair in `tls_dir` (`cert.pem`, `key.pem`, 0600, created on first start with rcgen:
//!   SANs = `opts.names`, 825 days). With `auto_regen`, a self-signed cert is regenerated
//!   when a name in `opts.names` is not covered or it expires within 30 days; the reason is
//!   returned in [`TlsMaterial::regenerated`]. The `ServerConfig` has ALPN `h2`,
//!   `http/1.1`;
//! - [`regenerate`] (`cinelog tls regen`): new self-signed pair; refuses a user-provided cert;
//! - [`inspect`] (`cinelog tls show`, `doctor`): the current cert's facts, `None` if there
//!   is none; never creates anything.

use std::fs::{self, DirBuilder};
use std::io;
use std::net::IpAddr;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use cinelog_core::registry;
use jiff::ToSpan as _;
use rcgen::{
    CertificateParams, DistinguishedName, DnType, ExtendedKeyUsagePurpose, IsCa, KeyPair, KeyUsagePurpose, SanType,
};
use rustls::pki_types::pem::PemObject as _;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use serde::Serialize;
use sha2::{Digest, Sha256};
use x509_parser::extensions::GeneralName;

use crate::error::{Result, WebError};
use crate::state::WebContext;

/// Validity of a new self-signed certificate, `notAfter - notBefore` (§7.1; Apple
/// platforms reject server certificates valid for longer).
pub const VALIDITY_DAYS: i64 = 825;
/// A self-signed certificate this close to expiry is regenerated.
pub const RENEW_BEFORE_SECS: i64 = 30 * 86_400;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TlsOptions {
    /// Directory of the self-signed pair.
    pub dir: PathBuf,
    /// User-provided certificate chain and key (both or neither).
    pub cert: Option<PathBuf>,
    pub key: Option<PathBuf>,
    pub auto_regen: bool,
    /// Names the certificate must cover (DNS names and IP literals).
    pub names: Vec<String>,
}

impl TlsOptions {
    /// From `[server]` of the config; `names` from `HostAllowlist::cert_names`.
    pub fn from_context(ctx: &WebContext, names: Vec<String>) -> Self {
        let path = |s: &str| (!s.is_empty()).then(|| PathBuf::from(s));
        let s = &ctx.config.server;
        Self {
            dir: ctx.paths.tls_dir.clone(),
            cert: path(&s.tls_cert),
            key: path(&s.tls_key),
            auto_regen: s.tls_auto_regen,
            names,
        }
    }

    pub fn self_signed_paths(&self) -> (PathBuf, PathBuf) {
        (self.dir.join("cert.pem"), self.dir.join("key.pem"))
    }

    /// The user-provided pair, `None` when neither is set; an error when only one is.
    fn user_pair(&self) -> Result<Option<(&Path, &Path)>> {
        match (&self.cert, &self.key) {
            (Some(c), Some(k)) => Ok(Some((c, k))),
            (None, None) => Ok(None),
            _ => Err(WebError::Internal(
                "server.tls_cert and server.tls_key must be set together".into(),
            )),
        }
    }
}

/// Facts about the served certificate (`cinelog tls show --json` prints this).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CertInfo {
    /// [`fingerprint`] of the leaf certificate.
    pub fingerprint: String,
    pub self_signed: bool,
    /// Unix seconds.
    pub not_after: i64,
    /// DNS names and IP literals.
    pub sans: Vec<String>,
    pub cert_path: PathBuf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RegenReason {
    /// No certificate yet (first start).
    Missing,
    /// A current hostname or IP is not in the SANs.
    NamesChanged,
    /// Expires within 30 days.
    Expiring,
}

#[derive(Debug, Clone)]
pub struct TlsMaterial {
    pub config: Arc<rustls::ServerConfig>,
    pub info: CertInfo,
    pub regenerated: Option<RegenReason>,
}

pub fn install_provider() {
    // Err only means a provider is installed already.
    let _ = rustls::crypto::ring::default_provider().install_default();
}

pub fn load_or_create(opts: &TlsOptions) -> Result<TlsMaterial> {
    if let Some((cert, key)) = opts.user_pair()? {
        let pair = Pair::read(cert, key)?;
        let config = server_config(&pair)?;
        let info = pair.info(cert, None)?;
        return Ok(TlsMaterial {
            config,
            info,
            regenerated: None,
        });
    }

    let (cert_path, key_path) = opts.self_signed_paths();
    let existing = match Pair::read(&cert_path, &key_path) {
        Ok(pair) => match server_config(&pair) {
            Ok(config) => Some((pair, config)),
            Err(e) => {
                tracing::warn!(
                    "{}: unusable self-signed pair ({e}); creating a new one",
                    cert_path.display()
                );
                None
            }
        },
        Err(WebError::Io(e)) if e.kind() == io::ErrorKind::NotFound => None,
        Err(e) => {
            tracing::warn!(
                "{}: unreadable self-signed pair ({e}); creating a new one",
                cert_path.display()
            );
            None
        }
    };
    let reason = match &existing {
        None => Some(RegenReason::Missing),
        Some(_) if !opts.auto_regen => None,
        Some((pair, _)) => match Parsed::of(&pair.chain[0]) {
            Ok(p) if p.not_after - now_secs() < RENEW_BEFORE_SECS => Some(RegenReason::Expiring),
            Ok(p) if !p.covers(&opts.names) => Some(RegenReason::NamesChanged),
            Ok(_) => None,
            Err(e) => {
                tracing::warn!("{}: {e}; creating a new one", cert_path.display());
                Some(RegenReason::Missing)
            }
        },
    };
    let (pair, config) = match (reason, existing) {
        (None, Some(found)) => found,
        _ => {
            let pair = create_self_signed(opts)?;
            let config = server_config(&pair)?;
            (pair, config)
        }
    };
    let info = pair.info(&cert_path, Some(true))?;
    Ok(TlsMaterial {
        config,
        info,
        regenerated: reason,
    })
}

pub fn regenerate(opts: &TlsOptions) -> Result<CertInfo> {
    if opts.user_pair()?.is_some() {
        return Err(WebError::Conflict(
            "server.tls_cert is set: cinelog does not replace a user-provided certificate".into(),
        ));
    }
    let pair = create_self_signed(opts)?;
    pair.info(&opts.self_signed_paths().0, Some(true))
}

pub fn inspect(opts: &TlsOptions) -> Result<Option<CertInfo>> {
    let (path, ours) = match opts.user_pair()? {
        Some((cert, _)) => (cert.to_owned(), None),
        None => (opts.self_signed_paths().0, Some(true)),
    };
    let pem = match fs::read(&path) {
        Ok(p) => p,
        // Only our own pair may be missing: the server creates it on its first start.
        Err(e) if e.kind() == io::ErrorKind::NotFound && ours.is_some() => return Ok(None),
        Err(e) => return Err(io_at(&path, e)),
    };
    let chain = parse_chain(&pem, &path)?;
    Ok(Some(cert_info(&chain[0], &path, ours)?))
}

/// SHA-256 of a certificate's DER as uppercase hex pairs joined by `:` (as
/// `openssl x509 -fingerprint -sha256` prints it, without the prefix).
pub fn fingerprint(der: &[u8]) -> String {
    let digest = Sha256::digest(der);
    let mut out = String::with_capacity(digest.len() * 3);
    for (i, b) in digest.iter().enumerate() {
        if i > 0 {
            out.push(':');
        }
        out.push_str(&format!("{b:02X}"));
    }
    out
}

// ------------------------------------------------------------------ internals

struct Pair {
    chain: Vec<CertificateDer<'static>>,
    key: PrivateKeyDer<'static>,
}

impl Pair {
    fn read(cert: &Path, key: &Path) -> Result<Self> {
        let pem = fs::read(cert).map_err(|e| io_at(cert, e))?;
        let chain = parse_chain(&pem, cert)?;
        let key_pem = fs::read(key).map_err(|e| io_at(key, e))?;
        let key = PrivateKeyDer::from_pem_slice(&key_pem)
            .map_err(|e| WebError::Internal(format!("{}: no private key ({e})", key.display())))?;
        Ok(Self { chain, key })
    }

    /// `self_signed`: `Some(true)` for our own pair, `None` to derive it from the cert.
    fn info(&self, path: &Path, self_signed: Option<bool>) -> Result<CertInfo> {
        cert_info(&self.chain[0], path, self_signed)
    }
}

fn parse_chain(pem: &[u8], path: &Path) -> Result<Vec<CertificateDer<'static>>> {
    let chain = CertificateDer::pem_slice_iter(pem)
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(|e| WebError::Internal(format!("{}: bad PEM ({e})", path.display())))?;
    if chain.is_empty() {
        return Err(WebError::Internal(format!("{}: no certificate", path.display())));
    }
    Ok(chain)
}

fn server_config(pair: &Pair) -> Result<Arc<rustls::ServerConfig>> {
    let tls_err = |e: rustls::Error| WebError::Internal(format!("TLS configuration: {e}"));
    let mut config = rustls::ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .with_safe_default_protocol_versions()
        .map_err(tls_err)?
        .with_no_client_auth()
        .with_single_cert(pair.chain.clone(), pair.key.clone_key())
        .map_err(tls_err)?;
    config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    Ok(Arc::new(config))
}

/// The facts of a leaf certificate that load, inspect and the regen checks need.
struct Parsed {
    not_after: i64,
    dns: Vec<String>,
    ips: Vec<IpAddr>,
    issuer_is_subject: bool,
}

impl Parsed {
    fn of(der: &CertificateDer<'_>) -> Result<Self> {
        let bad = |e: String| WebError::Internal(format!("certificate: {e}"));
        let (_, cert) = x509_parser::parse_x509_certificate(der).map_err(|e| bad(e.to_string()))?;
        let mut dns = Vec::new();
        let mut ips = Vec::new();
        if let Some(san) = cert.subject_alternative_name().map_err(|e| bad(e.to_string()))? {
            for name in &san.value.general_names {
                match name {
                    GeneralName::DNSName(d) => dns.push((*d).to_owned()),
                    GeneralName::IPAddress(raw) => {
                        if let Some(ip) = ip_from_bytes(raw) {
                            ips.push(ip);
                        }
                    }
                    _ => {}
                }
            }
        }
        Ok(Self {
            not_after: cert.validity().not_after.timestamp(),
            dns,
            ips,
            issuer_is_subject: cert.issuer().as_raw() == cert.subject().as_raw(),
        })
    }

    fn covers(&self, names: &[String]) -> bool {
        names
            .iter()
            .filter(|n| san_of(n).is_some())
            .all(|n| match n.parse::<IpAddr>() {
                Ok(ip) => self.ips.iter().any(|s| s.to_canonical() == ip.to_canonical()),
                Err(_) => self.dns.iter().any(|d| d.eq_ignore_ascii_case(n)),
            })
    }

    fn sans(&self) -> Vec<String> {
        self.dns
            .iter()
            .cloned()
            .chain(self.ips.iter().map(IpAddr::to_string))
            .collect()
    }
}

fn ip_from_bytes(raw: &[u8]) -> Option<IpAddr> {
    match raw.len() {
        4 => Some(IpAddr::from(<[u8; 4]>::try_from(raw).ok()?)),
        16 => Some(IpAddr::from(<[u8; 16]>::try_from(raw).ok()?)),
        _ => None,
    }
}

fn cert_info(der: &CertificateDer<'_>, path: &Path, self_signed: Option<bool>) -> Result<CertInfo> {
    let parsed = Parsed::of(der)?;
    Ok(CertInfo {
        fingerprint: fingerprint(der),
        self_signed: self_signed.unwrap_or(parsed.issuer_is_subject),
        not_after: parsed.not_after,
        sans: parsed.sans(),
        cert_path: path.to_owned(),
    })
}

/// Generates a pair valid from yesterday (clock skew) for exactly [`VALIDITY_DAYS`] and
/// writes it to `opts.dir`.
fn create_self_signed(opts: &TlsOptions) -> Result<Pair> {
    let today = jiff::Timestamp::now().to_zoned(jiff::tz::TimeZone::UTC).date();
    let from = today.checked_sub(1.day()).map_err(internal)?;
    let until = from.checked_add(VALIDITY_DAYS.days()).map_err(internal)?;
    let (cert_pem, key_pem) = self_signed_pem(&opts.names, from, until)?;
    let (cert_path, key_path) = opts.self_signed_paths();
    DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&opts.dir)
        .map_err(|e| io_at(&opts.dir, e))?;
    // Key first: a crash in between leaves a mismatched pair, which the next start replaces.
    write_private(&key_path, key_pem.as_bytes())?;
    write_private(&cert_path, cert_pem.as_bytes())?;
    Pair::read(&cert_path, &key_path)
}

/// A self-signed ECDSA P-256 server certificate for `names` (invalid DNS names are
/// skipped; `localhost` when nothing is left) as PEM `(cert, key)`.
fn self_signed_pem(names: &[String], from: jiff::civil::Date, until: jiff::civil::Date) -> Result<(String, String)> {
    let mut sans = Vec::new();
    for name in names {
        let Some(san) = san_of(name) else {
            tracing::warn!("{name:?} cannot be a certificate name; skipped");
            continue;
        };
        if !sans.contains(&san) {
            sans.push(san);
        }
    }
    if sans.is_empty() {
        sans.push(SanType::DnsName("localhost".try_into().map_err(internal)?));
    }
    let cn = names
        .iter()
        .find(|n| n.parse::<IpAddr>().is_err())
        .map_or("cinelog", String::as_str);
    let mut params = CertificateParams::default();
    params.subject_alt_names = sans;
    params.distinguished_name = DistinguishedName::new();
    params.distinguished_name.push(DnType::CommonName, cn);
    params.distinguished_name.push(DnType::OrganizationName, "cinelog");
    params.is_ca = IsCa::ExplicitNoCa;
    params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
    params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
    // `rcgen::date_time_ymd` keeps the `time` crate out of our dependencies.
    let ymd = |d: jiff::civil::Date| (i32::from(d.year()), d.month().unsigned_abs(), d.day().unsigned_abs());
    let (y, m, d) = ymd(from);
    params.not_before = rcgen::date_time_ymd(y, m, d);
    let (y, m, d) = ymd(until);
    params.not_after = rcgen::date_time_ymd(y, m, d);
    let key = KeyPair::generate().map_err(internal)?;
    let cert = params.self_signed(&key).map_err(internal)?;
    Ok((cert.pem(), key.serialize_pem()))
}

/// The SAN entry for a name, `None` if it cannot be one (such names are skipped when
/// generating, so they must not count as "not covered" either).
fn san_of(name: &str) -> Option<SanType> {
    match name.parse::<IpAddr>() {
        Ok(ip) => Some(SanType::IpAddress(ip)),
        Err(_) => name.to_owned().try_into().ok().map(SanType::DnsName),
    }
}

/// Atomic write (temp file + rename) with mode 0600; never follows a symlink at `path` or
/// at the temp path ([`registry::write_private`]).
fn write_private(path: &Path, data: &[u8]) -> Result<()> {
    registry::write_private(path, data).map_err(|e| io_at(path, e))
}

fn io_at(path: &Path, e: io::Error) -> WebError {
    WebError::Io(io::Error::new(e.kind(), format!("{}: {e}", path.display())))
}

fn internal(e: impl std::fmt::Display) -> WebError {
    WebError::Internal(format!("certificate generation: {e}"))
}

fn now_secs() -> i64 {
    jiff::Timestamp::now().as_second()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn opts(dir: &Path, names: &[&str]) -> TlsOptions {
        TlsOptions {
            dir: dir.join("tls"),
            cert: None,
            key: None,
            auto_regen: true,
            names: names.iter().map(|s| (*s).to_owned()).collect(),
        }
    }

    #[test]
    fn fingerprint_format() {
        let fp = fingerprint(b"");
        assert_eq!(fp.len(), 32 * 3 - 1);
        assert!(fp.starts_with("E3:B0:C4:42:98:FC:1C:14"));
    }

    #[test]
    fn options_from_config() {
        let dir = tempfile::tempdir().unwrap();
        let mut ctx = WebContext::for_tests(dir.path());
        let o = TlsOptions::from_context(&ctx, vec!["box".into()]);
        assert_eq!((&o.cert, &o.key), (&None, &None));
        assert_eq!(o.self_signed_paths().0, ctx.paths.tls_dir.join("cert.pem"));
        ctx.config.server.tls_cert = "/etc/c.pem".into();
        ctx.config.server.tls_key = "/etc/k.pem".into();
        let o = TlsOptions::from_context(&ctx, vec![]);
        assert_eq!(o.cert, Some(PathBuf::from("/etc/c.pem")));
    }

    #[test]
    fn first_start_creates_then_reuses() {
        let dir = tempfile::tempdir().unwrap();
        let o = opts(
            dir.path(),
            &["box", "box.lan", "localhost", "127.0.0.1", "::1", "10.0.0.7"],
        );
        assert_eq!(inspect(&o).unwrap(), None, "inspect never creates");
        assert!(!o.dir.exists());

        let m = load_or_create(&o).unwrap();
        assert_eq!(m.regenerated, Some(RegenReason::Missing));
        assert!(m.info.self_signed);
        assert_eq!(m.config.alpn_protocols, vec![b"h2".to_vec(), b"http/1.1".to_vec()]);
        for n in ["box", "box.lan", "localhost", "127.0.0.1", "::1", "10.0.0.7"] {
            assert!(m.info.sans.iter().any(|s| s == n), "{n} in {:?}", m.info.sans);
        }
        let days = (m.info.not_after - now_secs()) / 86_400;
        assert!((VALIDITY_DAYS - 2..VALIDITY_DAYS).contains(&days), "{days}");
        let (c, k) = o.self_signed_paths();
        for p in [&c, &k] {
            assert_eq!(fs::metadata(p).unwrap().permissions().mode() & 0o777, 0o600);
        }
        assert_eq!(fs::metadata(&o.dir).unwrap().permissions().mode() & 0o777, 0o700);

        let again = load_or_create(&o).unwrap();
        assert_eq!(again.regenerated, None);
        assert_eq!(again.info.fingerprint, m.info.fingerprint);
        assert_eq!(inspect(&o).unwrap().unwrap(), m.info);

        // A name that went away is still covered; only a new one triggers a regeneration.
        let fewer = opts(dir.path(), &["box", "127.0.0.1"]);
        assert_eq!(load_or_create(&fewer).unwrap().regenerated, None);
    }

    #[test]
    fn writes_do_not_follow_planted_symlinks() {
        let dir = tempfile::tempdir().unwrap();
        let o = opts(dir.path(), &["box"]);
        fs::create_dir(&o.dir).unwrap();
        let victim = dir.path().join("victim");
        fs::write(&victim, "keep").unwrap();
        let (c, k) = o.self_signed_paths();
        let tmp = o.dir.join(format!(".key.pem.{}.tmp", std::process::id()));
        for link in [&c, &k, &tmp] {
            std::os::unix::fs::symlink(&victim, link).unwrap();
        }
        load_or_create(&o).unwrap();
        assert_eq!(fs::read_to_string(&victim).unwrap(), "keep");
        for p in [&c, &k] {
            let meta = fs::symlink_metadata(p).unwrap();
            assert!(meta.is_file(), "{}", p.display());
            assert_eq!(meta.permissions().mode() & 0o777, 0o600);
        }
        assert!(fs::symlink_metadata(&tmp).unwrap().file_type().is_symlink());
    }

    /// Regression: the validity was 826 days (yesterday until today + 825), which Apple
    /// platforms reject for TLS server certificates.
    #[test]
    fn validity_is_at_most_825_days() {
        let dir = tempfile::tempdir().unwrap();
        let o = opts(dir.path(), &["box"]);
        for info in [load_or_create(&o).unwrap().info, regenerate(&o).unwrap()] {
            let pem = fs::read(&info.cert_path).unwrap();
            let der = CertificateDer::from_pem_slice(&pem).unwrap();
            let (_, cert) = x509_parser::parse_x509_certificate(&der).unwrap();
            let v = cert.validity();
            let (from, until) = (v.not_before.timestamp(), v.not_after.timestamp());
            assert_eq!(until - from, VALIDITY_DAYS * 86_400);
            assert!(from <= now_secs(), "valid now");
        }
    }

    #[test]
    fn regenerates_on_new_name_and_expiry() {
        let dir = tempfile::tempdir().unwrap();
        let o = opts(dir.path(), &["box", "127.0.0.1"]);
        let first = load_or_create(&o).unwrap().info;

        let moved = opts(dir.path(), &["box", "127.0.0.1", "192.168.1.20"]);
        let m = load_or_create(&moved).unwrap();
        assert_eq!(m.regenerated, Some(RegenReason::NamesChanged));
        assert_ne!(m.info.fingerprint, first.fingerprint);
        assert!(m.info.sans.contains(&"192.168.1.20".to_owned()));

        let mut manual = moved.clone();
        manual.auto_regen = false;
        manual.names.push("new.lan".into());
        assert_eq!(
            load_or_create(&manual).unwrap().regenerated,
            None,
            "auto_regen=false keeps it"
        );

        // A pair expiring in 10 days.
        let today = jiff::Timestamp::now().to_zoned(jiff::tz::TimeZone::UTC).date();
        let (cert, key) = self_signed_pem(
            &moved.names,
            today.checked_sub(800.days()).unwrap(),
            today.checked_add(10.days()).unwrap(),
        )
        .unwrap();
        let (cp, kp) = moved.self_signed_paths();
        write_private(&cp, cert.as_bytes()).unwrap();
        write_private(&kp, key.as_bytes()).unwrap();
        let m = load_or_create(&moved).unwrap();
        assert_eq!(m.regenerated, Some(RegenReason::Expiring));
        assert!(m.info.not_after - now_secs() > 800 * 86_400);
    }

    #[test]
    fn broken_pair_is_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let o = opts(dir.path(), &["box"]);
        load_or_create(&o).unwrap();
        let (c, _) = o.self_signed_paths();
        fs::write(&c, "garbage").unwrap();
        let m = load_or_create(&o).unwrap();
        assert_eq!(m.regenerated, Some(RegenReason::Missing));
        // A key that does not match the certificate.
        let (cert, _) =
            self_signed_pem(&o.names, jiff::civil::date(2024, 1, 1), jiff::civil::date(2099, 1, 1)).unwrap();
        fs::write(&c, cert).unwrap();
        assert_eq!(load_or_create(&o).unwrap().regenerated, Some(RegenReason::Missing));
    }

    #[test]
    fn regenerate_and_user_certificates() {
        let dir = tempfile::tempdir().unwrap();
        let o = opts(dir.path(), &["box"]);
        let a = regenerate(&o).unwrap();
        let b = regenerate(&o).unwrap();
        assert_ne!(a.fingerprint, b.fingerprint);
        assert_eq!(inspect(&o).unwrap().unwrap().fingerprint, b.fingerprint);

        // A user pair is used as is, never regenerated, and `regenerate` refuses it.
        let (cert, key) = self_signed_pem(
            &["other.example".into()],
            jiff::civil::date(2024, 1, 1),
            jiff::civil::date(2099, 1, 1),
        )
        .unwrap();
        let cp = dir.path().join("user.crt");
        let kp = dir.path().join("user.key");
        fs::write(&cp, cert).unwrap();
        fs::write(&kp, key).unwrap();
        let user = TlsOptions {
            cert: Some(cp.clone()),
            key: Some(kp),
            ..opts(dir.path(), &["box", "10.1.1.1"])
        };
        let m = load_or_create(&user).unwrap();
        assert_eq!(m.regenerated, None);
        assert_eq!(m.info.cert_path, cp);
        assert!(m.info.self_signed, "issuer == subject");
        assert_eq!(m.info.sans, vec!["other.example".to_owned()]);
        assert!(matches!(regenerate(&user), Err(WebError::Conflict(_))));
        assert_eq!(inspect(&user).unwrap().unwrap().fingerprint, m.info.fingerprint);

        let half = TlsOptions {
            key: None,
            ..user.clone()
        };
        assert!(load_or_create(&half).is_err());
        let missing = TlsOptions {
            cert: Some(dir.path().join("nope.crt")),
            ..user
        };
        assert!(matches!(load_or_create(&missing), Err(WebError::Io(_))));
        assert!(
            matches!(inspect(&missing), Err(WebError::Io(_))),
            "a configured cert must exist"
        );
    }

    #[test]
    fn invalid_names_are_skipped() {
        let (cert, _) = self_signed_pem(
            &["bad name\u{e4}".into(), "ok.lan".into()],
            jiff::civil::date(2024, 1, 1),
            jiff::civil::date(2030, 1, 1),
        )
        .unwrap();
        let der = CertificateDer::from_pem_slice(cert.as_bytes()).unwrap();
        let p = Parsed::of(&der).unwrap();
        assert_eq!(p.sans(), vec!["ok.lan".to_owned()]);
        assert!(p.covers(&["OK.LAN".into()]));
        assert!(
            p.covers(&["bad name\u{e4}".into(), "ok.lan".into()]),
            "an impossible name never forces a regeneration"
        );
        assert!(!p.covers(&["127.0.0.1".into()]));
        let (cert, _) = self_signed_pem(&[], jiff::civil::date(2024, 1, 1), jiff::civil::date(2030, 1, 1)).unwrap();
        let der = CertificateDer::from_pem_slice(cert.as_bytes()).unwrap();
        assert_eq!(Parsed::of(&der).unwrap().sans(), vec!["localhost".to_owned()]);
    }

    #[test]
    fn provider_install_is_idempotent() {
        install_provider();
        install_provider();
        assert!(rustls::crypto::CryptoProvider::get_default().is_some());
    }
}
