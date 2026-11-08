//! Payload encoding and decoding (protocol §1.1, §4.1).

use std::borrow::Cow;

use base64::Engine as _;
use base64::alphabet;
use base64::engine::{DecodePaddingMode, GeneralPurpose, GeneralPurposeConfig};

use crate::types::{
    Cmd, Encoding, MAX_COMMAND_TEXT, MAX_ENCODED_PAYLOAD, Nonce, NonceField, PROTOCOL_VERSION, PayloadError,
    ScannedMarker, WIRE_PREFIX, WireMessage,
};

/// Standard alphabet, written without padding; padding is tolerated when decoding.
const BASE64: GeneralPurpose = GeneralPurpose::new(
    &alphabet::STANDARD,
    GeneralPurposeConfig::new()
        .with_encode_padding(false)
        .with_decode_padding_mode(DecodePaddingMode::Indifferent),
);

/// Decodes and parses the payload of a marker.
///
/// Invalid UTF-8 (a command line the shell holds as raw bytes) becomes U+FFFD instead of
/// failing the whole marker.
pub fn decode_payload(enc: Encoding, payload: &[u8]) -> Result<WireMessage, PayloadError> {
    match enc {
        Encoding::Base64 => {
            let json = BASE64
                .decode(payload)
                .map_err(|e| PayloadError::Encoding(format!("base64: {e}")))?;
            WireMessage::from_json(String::from_utf8_lossy(&json).as_bytes())
        }
        Encoding::Json => {
            if let Some(b) = payload.iter().find(|&&b| b < 0x20 || b == 0x7f) {
                return Err(PayloadError::Encoding(format!(
                    "raw control byte 0x{b:02x} in JSON payload"
                )));
            }
            WireMessage::from_json(String::from_utf8_lossy(payload).as_bytes())
        }
    }
}

/// Why a message cannot be sent as a marker.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum EncodeError {
    /// The scanner would reject the marker as `oversized` and pass it through as text.
    #[error("encoded payload is {len} bytes, more than {MAX_ENCODED_PAYLOAD}")]
    Oversized { len: usize },
}

/// The complete BEL-terminated marker for `msg`, with the encoding chosen by
/// [`Encoding::for_json_len`].
///
/// Fails when the encoded payload exceeds [`MAX_ENCODED_PAYLOAD`]. For a `cmd`, call
/// [`Cmd::fit_to_wire`] first.
pub fn try_encode_marker(nonce: NonceField, msg: &WireMessage) -> Result<Vec<u8>, EncodeError> {
    let json = msg.to_json();
    let enc = Encoding::for_json_len(json.len());
    encode_json_marker(nonce, enc, &json)
}

/// Like [`try_encode_marker`], with an explicit encoding.
pub fn try_encode_marker_as(nonce: NonceField, enc: Encoding, msg: &WireMessage) -> Result<Vec<u8>, EncodeError> {
    encode_json_marker(nonce, enc, &msg.to_json())
}

/// [`try_encode_marker`] for messages whose size is bounded by construction.
///
/// # Panics
///
/// If the encoded payload exceeds [`MAX_ENCODED_PAYLOAD`]. Messages that carry unbounded
/// text (a command line, a user-supplied reason) must use [`try_encode_marker`].
pub fn encode_marker(nonce: NonceField, msg: &WireMessage) -> Vec<u8> {
    try_encode_marker(nonce, msg).unwrap_or_else(|e| panic!("{} marker: {e}", msg.kind()))
}

/// Like [`encode_marker`], with an explicit encoding.
///
/// # Panics
///
/// If the encoded payload exceeds [`MAX_ENCODED_PAYLOAD`].
pub fn encode_marker_as(nonce: NonceField, enc: Encoding, msg: &WireMessage) -> Vec<u8> {
    try_encode_marker_as(nonce, enc, msg).unwrap_or_else(|e| panic!("{} marker: {e}", msg.kind()))
}

fn encode_payload(enc: Encoding, json: &[u8]) -> Vec<u8> {
    match enc {
        Encoding::Base64 => BASE64.encode(json).into_bytes(),
        Encoding::Json => escape_json_controls(json).into_owned(),
    }
}

fn encode_json_marker(nonce: NonceField, enc: Encoding, json: &[u8]) -> Result<Vec<u8>, EncodeError> {
    let payload = encode_payload(enc, json);
    if payload.len() > MAX_ENCODED_PAYLOAD {
        return Err(EncodeError::Oversized { len: payload.len() });
    }
    let mut out = Vec::with_capacity(WIRE_PREFIX.len() + 40 + payload.len());
    out.extend_from_slice(WIRE_PREFIX);
    out.extend_from_slice(PROTOCOL_VERSION.to_string().as_bytes());
    out.push(b';');
    match nonce {
        NonceField::Session(n) => out.extend_from_slice(n.to_hex().as_bytes()),
        NonceField::Recorder => out.push(b'-'),
    }
    out.push(b';');
    out.push(enc.as_byte());
    out.push(b';');
    out.extend_from_slice(&payload);
    out.push(0x07);
    Ok(out)
}

impl Cmd {
    /// Cuts `typed` and `expanded` so the `cmd` marker fits the wire, setting `trunc` if
    /// anything was cut.
    ///
    /// Each field is first capped at [`MAX_COMMAND_TEXT`] bytes. Escaping can still grow the
    /// payload up to sixfold (a C0 byte becomes `\u00XX`), so if the encoded payload then
    /// exceeds [`MAX_ENCODED_PAYLOAD`], both fields are cut further by their escaped length:
    /// a field that fits in half the remaining budget is kept whole and the other one gets
    /// the rest. Cuts fall on character boundaries.
    ///
    /// Fails only if the message is too large with both fields empty.
    pub fn fit_to_wire(&mut self) -> Result<(), EncodeError> {
        self.trunc |= truncate_bytes(&mut self.typed, MAX_COMMAND_TEXT);
        self.trunc |= truncate_bytes(&mut self.expanded, MAX_COMMAND_TEXT);
        let encoded = |c: &Cmd| {
            let json = WireMessage::Cmd(c.clone()).to_json();
            encode_payload(Encoding::for_json_len(json.len()), &json).len()
        };
        if encoded(self) <= MAX_ENCODED_PAYLOAD {
            return Ok(());
        }

        let mut bare = self.clone();
        bare.typed.clear();
        bare.expanded.clear();
        bare.trunc = true;
        let bare_json = WireMessage::Cmd(bare).to_json();
        let overhead = escape_json_controls(&bare_json).len();
        let budget = MAX_ENCODED_PAYLOAD
            .checked_sub(overhead)
            .ok_or(EncodeError::Oversized { len: overhead })?;
        let (typed, expanded) = (escaped_len(&self.typed), escaped_len(&self.expanded));
        let half = budget / 2;
        let (typed_budget, expanded_budget) = if typed <= half {
            (typed, budget - typed)
        } else if expanded <= half {
            (budget - expanded, expanded)
        } else {
            (half, budget - half)
        };
        truncate_escaped(&mut self.typed, typed_budget);
        truncate_escaped(&mut self.expanded, expanded_budget);
        self.trunc = true;

        match encoded(self) {
            len if len <= MAX_ENCODED_PAYLOAD => Ok(()),
            len => Err(EncodeError::Oversized { len }),
        }
    }
}

/// Bytes that `c` occupies inside a `j` payload string.
fn escaped_char_len(c: char) -> usize {
    match c {
        '"' | '\\' => 2,
        '\u{0}'..='\u{1f}' | '\u{7f}' => 6,
        _ => c.len_utf8(),
    }
}

fn escaped_len(s: &str) -> usize {
    s.chars().map(escaped_char_len).sum()
}

/// Cuts `s` to at most `max` bytes on a character boundary; true if anything was cut.
fn truncate_bytes(s: &mut String, max: usize) -> bool {
    if s.len() <= max {
        return false;
    }
    let cut = (0..=max).rev().find(|&i| s.is_char_boundary(i)).unwrap_or(0);
    s.truncate(cut);
    true
}

/// Cuts `s` to the longest prefix whose escaped length is at most `max`.
fn truncate_escaped(s: &mut String, max: usize) {
    let mut used = 0;
    let cut = s
        .char_indices()
        .find(|&(_, c)| {
            used += escaped_char_len(c);
            used > max
        })
        .map(|(i, _)| i);
    if let Some(i) = cut {
        s.truncate(i);
    }
}

/// Rewrites serialized JSON into the `j` form: every C0 byte and DEL inside a string as
/// `\u00XX`.
///
/// `serde_json` already escapes C0 inside strings, partly with short forms (`\n`, `\t`, …),
/// and leaves DEL raw. Both are normalized here. Outside strings, where JSON allows
/// whitespace (`\t`, `\n`, `\r`) between tokens, every C0 byte and DEL becomes a space, so
/// pretty-printed JSON stays valid and equivalent.
pub fn escape_json_controls(json: &[u8]) -> Cow<'_, [u8]> {
    let needs_work = json.iter().any(|&b| b == b'\\' || b < 0x20 || b == 0x7f);
    if !needs_work {
        return Cow::Borrowed(json);
    }
    let mut out = Vec::with_capacity(json.len() + 16);
    let mut in_string = false;
    let mut iter = json.iter().copied();
    while let Some(b) = iter.next() {
        match b {
            b'"' => {
                in_string = !in_string;
                out.push(b);
            }
            0x00..=0x1f | 0x7f if !in_string => out.push(b' '),
            b'\\' if in_string => {
                let Some(next) = iter.next() else {
                    out.push(b);
                    break;
                };
                let control = match next {
                    b'b' => Some(0x08),
                    b't' => Some(0x09),
                    b'n' => Some(0x0a),
                    b'f' => Some(0x0c),
                    b'r' => Some(0x0d),
                    _ => None,
                };
                match control {
                    Some(c) => push_u_escape(&mut out, c),
                    None => out.extend_from_slice(&[b'\\', next]),
                }
            }
            0x00..=0x1f | 0x7f => push_u_escape(&mut out, b),
            _ => out.push(b),
        }
    }
    Cow::Owned(out)
}

fn push_u_escape(out: &mut Vec<u8>, b: u8) {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    out.extend_from_slice(b"\\u00");
    out.push(HEX[usize::from(b >> 4)]);
    out.push(HEX[usize::from(b & 0xf)]);
}

/// Why an accepted [`ScannedMarker`] is not processed. Every case counts as a bad marker.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum MarkerError {
    #[error(transparent)]
    Payload(#[from] PayloadError),
    /// `nested-*` with a session nonce, or any other kind with the recorder nonce `-`
    /// (protocol §4.1 step 4).
    #[error("kind {0:?} is not allowed with this nonce")]
    NonceKind(String),
}

impl ScannedMarker {
    /// Decodes the payload and checks that its kind matches the nonce field.
    pub fn decode(&self) -> Result<WireMessage, MarkerError> {
        let msg = decode_payload(self.enc, &self.payload)?;
        let recorder = matches!(self.nonce, NonceField::Recorder);
        if msg.requires_recorder_nonce() != recorder {
            return Err(MarkerError::NonceKind(msg.kind().to_owned()));
        }
        Ok(msg)
    }
}

/// Placeholder that [`scrub_nonce`] writes instead of the nonce.
pub const NONCE_PLACEHOLDER: &str = "<cinelog-nonce>";

/// Replaces every occurrence of the nonce's hex form in `text`.
///
/// The wire header never reaches an `m` event, but payload text can still quote the nonce
/// (a command line that reads the registry). The recorder passes each `m` data string
/// through this so the nonce is never written to a cast (protocol §5.1).
pub fn scrub_nonce<'a>(text: &'a str, nonce: &Nonce) -> Cow<'a, str> {
    let hex = nonce.to_hex();
    if text.contains(&hex) {
        Cow::Owned(text.replace(&hex, NONCE_PLACEHOLDER))
    } else {
        Cow::Borrowed(text)
    }
}

#[cfg(test)]
mod tests {
    use serde_json::Value;

    use super::*;
    use crate::scanner::Scanner;
    use crate::types::{
        BASE64_MAX_JSON, CastMarker, CmdRecord, Done, Extra, MarkerScanner, NestedInfo, PauseRequest, ScanStats, Scope,
    };

    const NONCE: Nonce = Nonce::from_bytes([0xc3; 16]);

    #[test]
    fn invalid_utf8_in_a_payload_becomes_replacement_characters() {
        let mut raw = br#"{"v":1,"k":"cmd","sh":1,"n":1,"typed":"ls "#.to_vec();
        raw.extend_from_slice(&[0xff, 0xfe]);
        raw.extend_from_slice(br#"","cwd":"/"}"#);
        for (enc, payload) in [
            (Encoding::Json, raw.clone()),
            (Encoding::Base64, BASE64.encode(&raw).into_bytes()),
        ] {
            let WireMessage::Cmd(c) = decode_payload(enc, &payload).unwrap() else {
                panic!("not a cmd")
            };
            assert_eq!(c.typed, "ls \u{fffd}\u{fffd}");
        }
    }

    fn cmd(typed: &str) -> Cmd {
        Cmd {
            sh: 4242,
            n: 3,
            typed: typed.into(),
            expanded: typed.into(),
            cwd: "/tmp".into(),
            user: "alice".into(),
            euid: Some(1000),
            host: "box".into(),
            pane: Some("%3".into()),
            t: 1_731_070_000.123_456,
            trunc: false,
            extra: Extra::new(),
        }
    }

    fn nested() -> NestedInfo {
        NestedInfo {
            uuid: "0190a1b2-c3d4-7e5f-8a9b-0c1d2e3f4a5b".into(),
            pid: 77,
            starttime: 123_456,
            scope: Scope::Team,
            user: "alice".into(),
            sudo_user: Some("alice".into()),
            url: None,
            exit: None,
            extra: Extra::new(),
        }
    }

    fn scan_one(wire: &[u8], expected: Option<Nonce>) -> ScannedMarker {
        let mut s = Scanner::new(expected, true);
        let (mut out, mut markers) = (Vec::new(), Vec::new());
        s.feed(wire, &mut out, &mut markers);
        assert!(
            out.is_empty(),
            "marker not stripped: {:?}",
            String::from_utf8_lossy(&out)
        );
        assert_eq!(markers.len(), 1);
        markers.pop().unwrap()
    }

    #[test]
    fn small_payload_is_base64_without_padding() {
        let msg = WireMessage::Cmd(cmd("ls -la"));
        let wire = encode_marker(NonceField::Session(NONCE), &msg);
        let prefix = format!("\x1b]7777;cinelog;1;{};b;", NONCE.to_hex());
        assert!(wire.starts_with(prefix.as_bytes()));
        assert_eq!(wire.last(), Some(&0x07));
        assert!(!wire.contains(&b'='));
        let m = scan_one(&wire, Some(NONCE));
        assert_eq!(m.enc, Encoding::Base64);
        assert_eq!(m.decode().unwrap(), msg);
    }

    #[test]
    fn large_payload_is_escaped_json() {
        let typed: String = "x\u{7f}\n\t\"\\é\u{1}".repeat(BASE64_MAX_JSON / 4);
        let msg = WireMessage::Cmd(cmd(&typed));
        let wire = encode_marker(NonceField::Session(NONCE), &msg);
        let payload = &wire[WIRE_PREFIX.len() + 2 + Nonce::HEX_LEN + 3..wire.len() - 1];
        assert!(payload.iter().all(|&b| b >= 0x20 && b != 0x7f));
        let text = std::str::from_utf8(payload).unwrap();
        assert!(text.contains(r"\u007f") && text.contains(r"\u000a") && text.contains(r"\u0001"));
        assert!(!text.contains(r"\n") && !text.contains(r"\t"));
        let m = scan_one(&wire, Some(NONCE));
        assert_eq!(m.enc, Encoding::Json);
        assert_eq!(m.decode().unwrap(), msg);
    }

    #[test]
    fn both_encodings_decode_for_any_length() {
        for msg in [
            WireMessage::Cmd(cmd("echo hi")),
            WireMessage::Cmd(cmd(&"y".repeat(4000))),
        ] {
            for enc in [Encoding::Base64, Encoding::Json] {
                let wire = encode_marker_as(NonceField::Session(NONCE), enc, &msg);
                assert_eq!(scan_one(&wire, Some(NONCE)).decode().unwrap(), msg);
            }
        }
    }

    #[test]
    fn decode_tolerates_padding_and_rejects_garbage() {
        let json = br#"{"v":1,"k":"skip","sh":5}"#;
        let padded = base64::engine::general_purpose::STANDARD.encode(json);
        assert!(padded.ends_with('='));
        assert!(matches!(
            decode_payload(Encoding::Base64, padded.as_bytes()),
            Ok(WireMessage::Skip(_))
        ));
        assert!(matches!(
            decode_payload(Encoding::Base64, b"!!!"),
            Err(PayloadError::Encoding(_))
        ));
        assert!(matches!(
            decode_payload(Encoding::Json, b"{\"v\":1,\n\"k\":\"skip\",\"sh\":5}"),
            Err(PayloadError::Encoding(_))
        ));
        assert!(matches!(
            decode_payload(Encoding::Json, b"{"),
            Err(PayloadError::Json(_))
        ));
        assert!(matches!(
            decode_payload(Encoding::Json, br#"{"v":2,"k":"skip","sh":5}"#),
            Err(PayloadError::UnsupportedVersion(2))
        ));
    }

    #[test]
    fn decode_enforces_nonce_kind_pairing() {
        let start = WireMessage::NestedStart(nested());
        let ok = scan_one(&encode_marker(NonceField::Recorder, &start), None);
        assert_eq!(ok.decode().unwrap(), start);

        let forged = scan_one(&encode_marker(NonceField::Session(NONCE), &start), Some(NONCE));
        assert_eq!(forged.decode(), Err(MarkerError::NonceKind("nested-start".into())));

        let pause = WireMessage::UserPause(PauseRequest::default());
        let via_dash = scan_one(&encode_marker(NonceField::Recorder, &pause), None);
        assert_eq!(via_dash.decode(), Err(MarkerError::NonceKind("user-pause".into())));

        let unknown = WireMessage::from_json(br#"{"v":1,"k":"future","a":1}"#).unwrap();
        let m = scan_one(&encode_marker(NonceField::Session(NONCE), &unknown), Some(NONCE));
        assert_eq!(m.decode().unwrap(), unknown);
    }

    #[test]
    fn escape_is_identity_without_controls_and_keeps_other_escapes() {
        assert!(matches!(escape_json_controls(br#"{"a":"b"}"#), Cow::Borrowed(_)));
        let src = serde_json::to_vec(&Value::from("\\n\"/\u{8}\u{c}\r")).unwrap();
        let escaped = escape_json_controls(&src);
        assert_eq!(&*escaped, br#""\\n\"/\u0008\u000c\u000d""#.as_slice());
        let back: Value = serde_json::from_slice(&escaped).unwrap();
        assert_eq!(back, Value::from("\\n\"/\u{8}\u{c}\r"));
    }

    fn scan_stats(wire: &[u8]) -> (usize, ScanStats) {
        let mut s = Scanner::new(Some(NONCE), true);
        let (mut out, mut markers) = (Vec::new(), Vec::new());
        s.feed(wire, &mut out, &mut markers);
        s.finish(&mut out);
        (markers.len(), s.stats())
    }

    fn payload_len(wire: &[u8]) -> usize {
        wire.len() - (WIRE_PREFIX.len() + 2 + Nonce::HEX_LEN + 3) - 1
    }

    #[test]
    fn escape_heavy_max_length_cmd_is_refused_not_emitted() {
        for c in ['"', '\t', '\u{7f}'] {
            let text: String = std::iter::repeat_n(c, MAX_COMMAND_TEXT).collect();
            let msg = WireMessage::Cmd(cmd(&text));
            let err = try_encode_marker(NonceField::Session(NONCE), &msg).unwrap_err();
            let EncodeError::Oversized { len } = err;
            assert!(len > MAX_ENCODED_PAYLOAD, "{c:?}: {len}");
            assert!(try_encode_marker_as(NonceField::Session(NONCE), Encoding::Json, &msg).is_err());
        }
    }

    #[test]
    #[should_panic(expected = "cmd marker: encoded payload is")]
    fn unchecked_encoder_panics_instead_of_emitting_an_oversized_marker() {
        let text = "\t".repeat(MAX_COMMAND_TEXT);
        encode_marker(NonceField::Session(NONCE), &WireMessage::Cmd(cmd(&text)));
    }

    #[test]
    fn largest_accepted_payload_still_encodes() {
        let probe = |n: usize| {
            let msg = WireMessage::Cmd(cmd(&"a".repeat(n)));
            encode_json_marker(NonceField::Session(NONCE), Encoding::Json, &msg.to_json())
        };
        let base = payload_len(&probe(0).unwrap());
        let fits = (MAX_ENCODED_PAYLOAD - base) / 2;
        let wire = probe(fits).unwrap();
        assert!(payload_len(&wire) >= MAX_ENCODED_PAYLOAD - 1);
        assert_eq!(scan_stats(&wire).0, 1);
        assert!(probe(fits + 1).is_err());
    }

    #[test]
    fn fit_to_wire_makes_escape_heavy_cmds_scannable() {
        for c in ['"', '\\', '\t', '\u{1}', '\u{7f}'] {
            let text: String = std::iter::repeat_n(c, MAX_COMMAND_TEXT).collect();
            let mut fitted = cmd(&text);
            fitted.fit_to_wire().unwrap();
            assert!(fitted.trunc);
            assert!(!fitted.typed.is_empty() && !fitted.expanded.is_empty());
            let msg = WireMessage::Cmd(fitted.clone());
            let wire = try_encode_marker(NonceField::Session(NONCE), &msg).unwrap();
            let len = payload_len(&wire);
            assert!(
                len <= MAX_ENCODED_PAYLOAD && len > MAX_ENCODED_PAYLOAD - 16,
                "{c:?}: {len}"
            );
            let (markers, stats) = scan_stats(&wire);
            assert_eq!((markers, stats.oversized), (1, 0), "{c:?}");
            assert_eq!(scan_one(&wire, Some(NONCE)).decode().unwrap(), msg);
            assert!(text.starts_with(&fitted.typed));
        }
    }

    #[test]
    fn fit_to_wire_gives_the_budget_to_the_long_field() {
        let mut c = cmd("ls");
        c.expanded = "\t".repeat(MAX_COMMAND_TEXT);
        c.fit_to_wire().unwrap();
        assert_eq!(c.typed, "ls");
        assert!(c.trunc && c.expanded.len() > 10_000, "{}", c.expanded.len());
        let wire = try_encode_marker(NonceField::Session(NONCE), &WireMessage::Cmd(c)).unwrap();
        assert!(payload_len(&wire) > MAX_ENCODED_PAYLOAD - 16);
    }

    #[test]
    fn fit_to_wire_caps_raw_text_on_char_boundaries() {
        let mut small = cmd("echo hi");
        small.fit_to_wire().unwrap();
        assert_eq!(small, cmd("echo hi"));

        let mut c = cmd(&format!("a{}", "é".repeat(MAX_COMMAND_TEXT / 2)));
        c.fit_to_wire().unwrap();
        assert!(c.trunc);
        assert_eq!(c.typed.len(), MAX_COMMAND_TEXT - 1);
        assert_eq!(c.expanded, c.typed);

        let mut mixed = cmd(&"\u{1}é".repeat(MAX_COMMAND_TEXT / 3));
        mixed.fit_to_wire().unwrap();
        assert!(mixed.typed.ends_with('é') || mixed.typed.ends_with('\u{1}'));
        try_encode_marker(NonceField::Session(NONCE), &WireMessage::Cmd(mixed)).unwrap();
    }

    #[test]
    fn fit_to_wire_fails_when_other_fields_are_too_large() {
        let mut c = cmd("ls");
        c.cwd = "\n".repeat(MAX_ENCODED_PAYLOAD / 4);
        assert!(matches!(c.fit_to_wire(), Err(EncodeError::Oversized { .. })));
    }

    #[test]
    fn escape_keeps_pretty_printed_json_valid() {
        let pretty = b"{\n\t\"a\" :\r\n \"x\\ty\\n\",\n  \"b\": [1,\t2]\x7f}\n";
        let escaped = escape_json_controls(pretty);
        assert!(escaped.iter().all(|&b| b >= 0x20 && b != 0x7f));
        let back: Value = serde_json::from_slice(&escaped).unwrap();
        assert_eq!(back, serde_json::json!({"a": "x\ty\n", "b": [1, 2]}));
        let quoted = escape_json_controls(br#"{"q":"a\"\n"}"#);
        assert_eq!(&*quoted, br#"{"q":"a\"\u000a"}"#.as_slice());
    }

    #[test]
    fn nonce_never_appears_in_the_m_event() {
        let hex = NONCE.to_hex();
        let mut c = cmd(&format!("cat registry # {hex}"));
        c.extra.insert("echo".into(), Value::from(hex.clone()));
        let wire = encode_marker(NonceField::Session(NONCE), &WireMessage::Cmd(c));
        let marker = scan_one(&wire, Some(NONCE));
        let WireMessage::Cmd(c) = marker.decode().unwrap() else {
            panic!("not a cmd")
        };
        let m = CastMarker::Cmd(CmdRecord { seq: 1, cmd: c }).to_json();
        let scrubbed = scrub_nonce(&m, &NONCE);
        assert!(!scrubbed.contains(&hex), "{scrubbed}");
        assert!(scrubbed.contains(NONCE_PLACEHOLDER));
        assert!(matches!(CastMarker::from_json(&scrubbed), Ok(Some(CastMarker::Cmd(_)))));

        let done = WireMessage::Done(Done {
            sh: 1,
            n: 1,
            exit: 0,
            pipestatus: vec![0],
            cwd: "/".into(),
            t: 0.0,
            extra: Extra::new(),
        });
        let m = scan_one(&encode_marker(NonceField::Session(NONCE), &done), Some(NONCE));
        let json = String::from_utf8(m.decode().unwrap().to_json()).unwrap();
        assert!(!json.contains(&hex));
        assert!(matches!(scrub_nonce(&json, &NONCE), Cow::Borrowed(_)));
    }
}
