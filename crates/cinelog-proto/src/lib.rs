//! cinelog marker protocol v1 (`docs/PROTOCOL.md`). No I/O.
//!
//! - [`types`]: the frozen contract (payloads, cast `m` events, `CINELOG_OPTS`).
//! - [`Scanner`]: the streaming [`MarkerScanner`] the recorder runs over PTY output.
//! - [`decode_payload`] / [`ScannedMarker::decode`]: payload decoding and the nonce/kind check.
//! - [`try_encode_marker`]: the complete BEL-terminated OSC for a message, encoding chosen by
//!   [`Encoding::for_json_len`]; refuses payloads over [`MAX_ENCODED_PAYLOAD`].
//!   [`Cmd::fit_to_wire`] cuts command text so a `cmd` always fits.

mod codec;
mod scanner;
pub mod types;

pub use codec::{
    EncodeError, MarkerError, NONCE_PLACEHOLDER, decode_payload, encode_marker, encode_marker_as, escape_json_controls,
    scrub_nonce, try_encode_marker, try_encode_marker_as,
};
pub use scanner::Scanner;
pub use types::*;
