//! Session ids (UUIDv7) and nonces.

use std::fmt;
use std::str::FromStr;

use cinelog_proto::Nonce;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Session uuid. All rotation parts of one session share it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SessionId(Uuid);

impl SessionId {
    pub fn new_v7() -> Self {
        Self(Uuid::now_v7())
    }

    pub const fn from_uuid(u: Uuid) -> Self {
        Self(u)
    }

    pub const fn as_uuid(&self) -> &Uuid {
        &self.0
    }

    /// 8 hex chars used in file names and the MOTD. Taken from the random tail of the
    /// UUIDv7: its leading chars are the millisecond timestamp and would collide for
    /// sessions started within the same minute.
    pub fn short(&self) -> String {
        let s = self.0.simple().to_string();
        s[s.len() - 8..].to_owned()
    }
}

impl fmt::Display for SessionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.hyphenated().fmt(f)
    }
}

impl FromStr for SessionId {
    type Err = uuid::Error;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Uuid::parse_str(s).map(Self)
    }
}

/// A fresh 128-bit nonce from the OS RNG.
pub fn new_nonce() -> Result<Nonce, getrandom::Error> {
    let mut b = [0u8; 16];
    getrandom::fill(&mut b)?;
    Ok(Nonce::from_bytes(b))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_ids_differ_for_back_to_back_sessions() {
        let a = SessionId::new_v7();
        let b = SessionId::new_v7();
        assert_ne!(a.short(), b.short());
        assert_eq!(a.short().len(), 8);
        assert_eq!(a.to_string().parse::<SessionId>().unwrap(), a);
    }

    #[test]
    fn nonces_are_random() {
        assert_ne!(new_nonce().unwrap(), new_nonce().unwrap());
    }
}
