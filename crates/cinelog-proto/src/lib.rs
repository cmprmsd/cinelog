//! cinelog marker protocol v1 (`docs/PROTOCOL.md`). No I/O.
//!
//! [`types`] is the frozen contract. The wire codec is owned by the proto work package and
//! must provide, at the crate root:
//!
//! - `pub struct Scanner` implementing [`MarkerScanner`], constructed with
//!   `Scanner::new(expected: Option<Nonce>, strip: bool)`. With `expected = None` only
//!   recorder (`-`) markers are accepted.
//! - `pub fn decode_payload(enc: Encoding, payload: &[u8]) -> Result<WireMessage, PayloadError>`.
//! - `pub fn encode_marker(nonce: NonceField, msg: &WireMessage) -> Vec<u8>`: the complete
//!   OSC sequence (BEL-terminated), choosing the encoding by [`Encoding::for_json_len`].

pub mod types;

pub use types::*;
