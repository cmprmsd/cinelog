//! Token files (§7.2): PERSONAL `auth.toml` (one token, named after the user) and TEAM
//! `tokens.toml` (one named token per member). Shared by the server's login (M3-A) and
//! `cinelog token new|list|revoke` / `install` (M3-D).
//!
//! A token is 32 random bytes shown once as unpadded base32 (52 characters). Only
//! `sha256(raw bytes)` is stored, so the presented text is case-insensitive. Files are
//! written atomically with mode 0600.
//!
//! ```toml
//! [[token]]
//! name = "alice"
//! sha256 = "9f86d0…"   # 64 lowercase hex digits
//! created_at = 1731070000.5
//! ```

use std::fs;
use std::io;
use std::path::Path;

use cinelog_core::registry::write_private;
use data_encoding::{BASE32_NOPAD, HEXLOWER};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

const TOKEN_BYTES: usize = 32;
const NAME_MAX: usize = 64;

#[derive(Debug, thiserror::Error)]
pub enum TokenError {
    #[error("invalid token name {0:?} (1–64 characters of A–Z, a–z, 0–9, '.', '_', '-')")]
    InvalidName(String),
    #[error("a token named {0:?} already exists")]
    Duplicate(String),
    #[error("{path}: {source}")]
    Parse { path: String, source: toml::de::Error },
    #[error(transparent)]
    Io(#[from] io::Error),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TokenEntry {
    /// The principal: author of annotations and forgets, key of `auth_sessions.principal`.
    pub name: String,
    /// Lowercase hex sha256 of the raw token bytes.
    pub sha256: String,
    pub created_at: f64,
}

/// A freshly generated token; `secret` is shown once and never stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewToken {
    pub name: String,
    pub secret: String,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct TokenFile {
    #[serde(default, rename = "token")]
    pub tokens: Vec<TokenEntry>,
}

impl TokenFile {
    /// A missing file is an empty token list.
    pub fn load(path: &Path) -> Result<Self, TokenError> {
        match fs::read_to_string(path) {
            Ok(s) => toml::from_str(&s).map_err(|source| TokenError::Parse {
                path: path.display().to_string(),
                source,
            }),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e.into()),
        }
    }

    /// Atomic write (temp file + rename, [`write_private`]), mode 0600. The parent
    /// directory must exist.
    pub fn save(&self, path: &Path) -> Result<(), TokenError> {
        let body = format!(
            "# cinelog tokens: sha256 of each token; the tokens themselves are never stored.\n{}",
            toml::to_string(self).map_err(|e| io::Error::other(e.to_string()))?
        );
        write_private(path, body.as_bytes())?;
        Ok(())
    }

    pub fn get(&self, name: &str) -> Option<&TokenEntry> {
        self.tokens.iter().find(|t| t.name == name)
    }

    /// Generates and adds a token named `name`.
    pub fn add(&mut self, name: &str, now: f64) -> Result<NewToken, TokenError> {
        if !valid_name(name) {
            return Err(TokenError::InvalidName(name.to_owned()));
        }
        if self.get(name).is_some() {
            return Err(TokenError::Duplicate(name.to_owned()));
        }
        let (secret, digest) = generate();
        self.tokens.push(TokenEntry {
            name: name.to_owned(),
            sha256: HEXLOWER.encode(&digest),
            created_at: now,
        });
        Ok(NewToken {
            name: name.to_owned(),
            secret,
        })
    }

    /// Removes the token named `name`; `false` if there was none. Callers also revoke the
    /// principal's auth sessions (`auth::revoke_principal`).
    pub fn remove(&mut self, name: &str) -> bool {
        let before = self.tokens.len();
        self.tokens.retain(|t| t.name != name);
        self.tokens.len() != before
    }

    /// The entry whose hash matches `presented`. Compares against every entry in constant
    /// time per entry, so timing does not reveal which (or whether a) prefix matched.
    pub fn verify(&self, presented: &str) -> Option<&TokenEntry> {
        let digest = hash(presented)?;
        let mut found = None;
        for t in &self.tokens {
            let Ok(stored) = HEXLOWER.decode(t.sha256.as_bytes()) else {
                continue;
            };
            if bool::from(stored.ct_eq(&digest)) && found.is_none() {
                found = Some(t);
            }
        }
        found
    }
}

/// `[A-Za-z0-9._-]{1,64}`, not starting with `.` or `-`.
pub fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= NAME_MAX
        && !name.starts_with(['.', '-'])
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

/// A new random token: its base32 text and the sha256 of its raw bytes.
pub fn generate() -> (String, [u8; 32]) {
    let mut raw = [0u8; TOKEN_BYTES];
    getrandom::fill(&mut raw).expect("the OS random source works");
    let digest = Sha256::digest(raw).into();
    (BASE32_NOPAD.encode(&raw), digest)
}

/// sha256 of a presented token's raw bytes; `None` if it is not a well-formed token.
/// Surrounding whitespace is ignored and case does not matter.
pub fn hash(presented: &str) -> Option<[u8; 32]> {
    let norm = presented.trim().to_ascii_uppercase();
    let raw = BASE32_NOPAD.decode(norm.as_bytes()).ok()?;
    (raw.len() == TOKEN_BYTES).then(|| Sha256::digest(&raw).into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn add_verify_remove_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tokens.toml");
        let mut f = TokenFile::load(&path).unwrap();
        assert!(f.tokens.is_empty());
        let a = f.add("alice", 1.5).unwrap();
        let b = f.add("bob", 2.0).unwrap();
        assert_eq!(a.secret.len(), 52);
        assert_ne!(a.secret, b.secret);
        assert!(matches!(f.add("alice", 3.0), Err(TokenError::Duplicate(_))));
        f.save(&path).unwrap();
        assert_eq!(fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        let text = fs::read_to_string(&path).unwrap();
        assert!(!text.contains(&a.secret), "the token itself is never stored");

        let f = TokenFile::load(&path).unwrap();
        assert_eq!(f.verify(&a.secret).map(|t| t.name.as_str()), Some("alice"));
        let sloppy = format!("  {}\n", b.secret.to_lowercase());
        assert_eq!(f.verify(&sloppy).map(|t| t.name.as_str()), Some("bob"));
        assert_eq!(f.verify(""), None);
        assert_eq!(f.verify(&a.secret[..50]), None);
        let (other, _) = generate();
        assert_eq!(f.verify(&other), None);

        let mut f = f;
        assert!(f.remove("alice"));
        assert!(!f.remove("alice"));
        assert_eq!(f.verify(&a.secret), None);
    }

    #[test]
    fn save_does_not_follow_planted_symlinks() {
        let dir = tempfile::tempdir().unwrap();
        let victim = dir.path().join("victim");
        fs::write(&victim, "keep").unwrap();
        let path = dir.path().join("tokens.toml");
        let tmp = dir.path().join(format!(".tokens.toml.{}.tmp", std::process::id()));
        std::os::unix::fs::symlink(&victim, &tmp).unwrap();
        std::os::unix::fs::symlink(&victim, &path).unwrap();
        let mut f = TokenFile::default();
        f.add("alice", 1.0).unwrap();
        f.save(&path).unwrap();
        assert_eq!(fs::read_to_string(&victim).unwrap(), "keep");
        let meta = fs::symlink_metadata(&path).unwrap();
        assert!(meta.is_file());
        assert_eq!(meta.permissions().mode() & 0o777, 0o600);
        assert!(fs::symlink_metadata(&tmp).unwrap().file_type().is_symlink());
        assert_eq!(TokenFile::load(&path).unwrap(), f);
    }

    #[test]
    fn names() {
        for ok in ["alice", "a.b-c_d", "X1"] {
            assert!(valid_name(ok), "{ok}");
        }
        for bad in ["", "-x", ".x", "a b", "ä", "a/b", &"x".repeat(65)] {
            assert!(!valid_name(bad), "{bad}");
        }
        assert!(matches!(
            TokenFile::default().add("a b", 0.0),
            Err(TokenError::InvalidName(_))
        ));
    }

    #[test]
    fn malformed_file_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.toml");
        fs::write(&path, "[[token]]\nname = 1\n").unwrap();
        assert!(matches!(TokenFile::load(&path), Err(TokenError::Parse { .. })));
    }
}
