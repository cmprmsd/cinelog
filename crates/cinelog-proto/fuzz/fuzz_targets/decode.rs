//! Payload decoding never panics, and every decoded message that fits the wire re-encodes
//! to a marker that decodes to the same message.
#![no_main]

use cinelog_proto::{Encoding, NonceField, decode_payload, try_encode_marker};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Some((&sel, payload)) = data.split_first() else { return };
    let enc = if sel & 1 == 0 { Encoding::Base64 } else { Encoding::Json };
    let Ok(msg) = decode_payload(enc, payload) else { return };
    // Re-escaping can grow a payload past the cap; the encoder must then refuse it.
    let Ok(wire) = try_encode_marker(NonceField::Recorder, &msg) else { return };
    let header = cinelog_proto::WIRE_PREFIX.len() + "1;-;".len();
    let enc = Encoding::from_byte(wire[header]).expect("encoder writes b or j");
    let again = decode_payload(enc, &wire[header + 2..wire.len() - 1]).expect("re-encoded payload decodes");
    // Float fields may not survive a text roundtrip bit-exactly; compare canonical JSON.
    assert_eq!(again.to_json(), msg.to_json());
});
