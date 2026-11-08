//! TLS material (work package M3-A; plan §7.1).
//!
//! Provides, with exactly these signatures:
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
//!
//! [`fingerprint`] and [`TlsOptions::from_context`] are part of the contract and complete.
//! The other bodies are the pre-integration stand-ins of the contracts commit.

use std::path::PathBuf;
use std::sync::Arc;

use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::error::{Result, WebError};
use crate::state::WebContext;

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

pub fn install_provider() {}

pub fn load_or_create(opts: &TlsOptions) -> Result<TlsMaterial> {
    let _ = opts;
    Err(WebError::Unavailable("tls"))
}

pub fn regenerate(opts: &TlsOptions) -> Result<CertInfo> {
    let _ = opts;
    Err(WebError::Unavailable("tls"))
}

pub fn inspect(opts: &TlsOptions) -> Result<Option<CertInfo>> {
    let _ = opts;
    Err(WebError::Unavailable("tls"))
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
