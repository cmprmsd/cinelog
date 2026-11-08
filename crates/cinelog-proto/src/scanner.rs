//! Streaming marker scanner (protocol §1.2, plan §4.4).
//!
//! The scanner sits on the PTY output path. Outside a marker it copies bytes with a
//! `memchr` fast path to the next ESC. From an ESC on it holds bytes back while they can
//! still be part of a marker, validates the header fields as they arrive and releases the
//! held bytes unchanged as soon as the sequence can no longer become an accepted marker.

use std::fmt;

use memchr::memchr;

use crate::types::{
    Encoding, MAX_ENCODED_PAYLOAD, MarkerScanner, Nonce, NonceField, PROTOCOL_VERSION, ScanStats, ScannedMarker,
    WIRE_PREFIX,
};

const ESC: u8 = 0x1b;
const BEL: u8 = 0x07;
const CAN: u8 = 0x18;
const SUB: u8 = 0x1a;
/// Longest accepted `<ver>` field; keeps the decimal value inside `u32`.
const MAX_VERSION_DIGITS: u8 = 9;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    /// Not inside a candidate marker; nothing is held.
    Ground,
    /// `n` bytes of [`WIRE_PREFIX`] matched (`1 ≤ n < len`).
    Prefix(usize),
    /// Reading `<ver>` digits.
    Version { digits: u8, value: u32 },
    /// Reading `<nonce>`: `len` lowercase hex digits so far.
    Nonce { len: u8 },
    /// Read `-` as the nonce, expecting `;`.
    NonceDash,
    /// Expecting the `<enc>` byte.
    Enc,
    /// Read `<enc>`, expecting `;`.
    EncEnd,
    /// Reading `<payload>`.
    Payload,
    /// Saw ESC inside the payload: `\` completes ST, anything else breaks the marker.
    PayloadEsc,
}

/// What a single byte did to the scanner.
enum Step {
    Consumed,
    /// The byte was not consumed; feed it again (the scanner is back in `Ground` or `Prefix`).
    Reprocess,
}

/// The cinelog [`MarkerScanner`] for protocol v1.
///
/// Markers carrying `expected` as their nonce, and markers with the recorder nonce `-`, are
/// accepted. With `expected = None` only `-` markers are accepted. Results are independent
/// of how the input is split into [`feed`](MarkerScanner::feed) calls.
#[derive(Clone)]
pub struct Scanner {
    expected: Option<Nonce>,
    strip: bool,
    state: State,
    /// Bytes of the current candidate marker, starting with its ESC.
    held: Vec<u8>,
    /// Offset of the payload inside `held` (valid in `Payload`/`PayloadEsc`).
    payload_start: usize,
    version: u32,
    nonce: NonceField,
    enc: Encoding,
    stats: ScanStats,
}

impl fmt::Debug for Scanner {
    // `held` can contain the nonce of an in-flight marker header, so only its length is shown.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Scanner")
            .field("expected", &self.expected)
            .field("strip", &self.strip)
            .field("state", &self.state)
            .field("held_len", &self.held.len())
            .field("stats", &self.stats)
            .finish_non_exhaustive()
    }
}

impl Scanner {
    /// `expected`: the session nonce to accept. `strip`: remove accepted markers from the
    /// passthrough output (`record.strip_markers`).
    pub fn new(expected: Option<Nonce>, strip: bool) -> Self {
        Self {
            expected,
            strip,
            state: State::Ground,
            held: Vec::new(),
            payload_start: 0,
            version: 0,
            nonce: NonceField::Recorder,
            enc: Encoding::Base64,
            stats: ScanStats::default(),
        }
    }

    /// Number of bytes currently held back (a partial prefix or an unterminated marker).
    ///
    /// A recorder that wants to bound terminal latency can call [`flush`](Self::flush) when
    /// this stays non-zero while the PTY is idle.
    pub fn held_len(&self) -> usize {
        self.held.len()
    }

    /// Releases held-back bytes to `out` unchanged and returns to the ground state. The
    /// scanner stays usable; a marker that was in flight is then passed through as text.
    pub fn flush(&mut self, out: &mut Vec<u8>) {
        self.release(out);
    }

    fn release(&mut self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.held);
        self.held.clear();
        self.state = State::Ground;
    }

    /// Rejects the sequence at byte `b`, which was not pushed to `held`.
    ///
    /// CAN/SUB abort the OSC and pass through after the held bytes. Any other byte makes the
    /// sequence malformed; it is reprocessed from the ground state because it may start a
    /// new marker (ESC).
    fn reject(&mut self, b: u8, out: &mut Vec<u8>) -> Step {
        self.release(out);
        if b == CAN || b == SUB {
            self.stats.aborted += 1;
            out.push(b);
            Step::Consumed
        } else {
            self.stats.malformed += 1;
            Step::Reprocess
        }
    }

    fn accept(&mut self, terminator: &[u8], out: &mut Vec<u8>, markers: &mut Vec<ScannedMarker>) {
        let at = out.len();
        let payload = self.held[self.payload_start..].to_vec();
        if !self.strip {
            out.extend_from_slice(&self.held);
            out.extend_from_slice(terminator);
        }
        markers.push(ScannedMarker {
            at,
            version: self.version,
            nonce: self.nonce,
            enc: self.enc,
            payload,
        });
        self.stats.accepted += 1;
        self.held.clear();
        self.state = State::Ground;
    }

    fn step(&mut self, b: u8, out: &mut Vec<u8>, markers: &mut Vec<ScannedMarker>) -> Step {
        match self.state {
            State::Ground => {
                if b == ESC {
                    self.held.push(b);
                    self.state = State::Prefix(1);
                } else {
                    out.push(b);
                }
            }
            State::Prefix(n) => {
                if b != WIRE_PREFIX[n] {
                    // Not one of ours: no counter, the bytes are ordinary output.
                    self.release(out);
                    return Step::Reprocess;
                }
                self.held.push(b);
                self.state = if n + 1 == WIRE_PREFIX.len() {
                    State::Version { digits: 0, value: 0 }
                } else {
                    State::Prefix(n + 1)
                };
            }
            State::Version { digits, value } => match b {
                b'0'..=b'9' if digits < MAX_VERSION_DIGITS => {
                    self.held.push(b);
                    self.state = State::Version {
                        digits: digits + 1,
                        value: value * 10 + u32::from(b - b'0'),
                    };
                }
                b';' if digits > 0 => {
                    self.held.push(b);
                    if value == PROTOCOL_VERSION {
                        self.version = value;
                        self.state = State::Nonce { len: 0 };
                    } else {
                        self.stats.unsupported_version += 1;
                        self.release(out);
                    }
                }
                _ => return self.reject(b, out),
            },
            State::Nonce { len } => match b {
                b'-' if len == 0 => {
                    self.held.push(b);
                    self.state = State::NonceDash;
                }
                b'0'..=b'9' | b'a'..=b'f' if usize::from(len) < Nonce::HEX_LEN => {
                    self.held.push(b);
                    self.state = State::Nonce { len: len + 1 };
                }
                b';' if usize::from(len) == Nonce::HEX_LEN => {
                    let hex = &self.held[self.held.len() - Nonce::HEX_LEN..];
                    let nonce = Nonce::parse_hex(hex).expect("validated as lowercase hex");
                    self.held.push(b);
                    if self.expected == Some(nonce) {
                        self.nonce = NonceField::Session(nonce);
                        self.state = State::Enc;
                    } else {
                        self.stats.bad_nonce += 1;
                        self.release(out);
                    }
                }
                _ => return self.reject(b, out),
            },
            State::NonceDash => {
                if b != b';' {
                    return self.reject(b, out);
                }
                self.held.push(b);
                self.nonce = NonceField::Recorder;
                self.state = State::Enc;
            }
            State::Enc => {
                let Some(enc) = Encoding::from_byte(b) else {
                    return self.reject(b, out);
                };
                self.held.push(b);
                self.enc = enc;
                self.state = State::EncEnd;
            }
            State::EncEnd => {
                if b != b';' {
                    return self.reject(b, out);
                }
                self.held.push(b);
                self.payload_start = self.held.len();
                self.state = State::Payload;
            }
            State::Payload => match b {
                BEL => self.accept(&[BEL], out, markers),
                ESC => {
                    self.held.push(b);
                    self.state = State::PayloadEsc;
                }
                0x00..=0x1f | 0x7f => return self.reject(b, out),
                _ => {
                    self.held.push(b);
                    if self.held.len() - self.payload_start > MAX_ENCODED_PAYLOAD {
                        self.stats.oversized += 1;
                        self.release(out);
                    }
                }
            },
            State::PayloadEsc => {
                self.held.pop();
                if b == b'\\' {
                    self.accept(b"\x1b\\", out, markers);
                } else {
                    // The ESC ends the OSC without ST. It may start the next marker, so it
                    // becomes the first held byte of a new candidate.
                    self.stats.malformed += 1;
                    self.release(out);
                    self.held.push(ESC);
                    self.state = State::Prefix(1);
                    return Step::Reprocess;
                }
            }
        }
        Step::Consumed
    }
}

impl MarkerScanner for Scanner {
    fn feed(&mut self, input: &[u8], out: &mut Vec<u8>, markers: &mut Vec<ScannedMarker>) {
        let mut i = 0;
        while i < input.len() {
            if self.state == State::Ground {
                let rest = &input[i..];
                match memchr(ESC, rest) {
                    None => {
                        out.extend_from_slice(rest);
                        return;
                    }
                    Some(k) => {
                        out.extend_from_slice(&rest[..k]);
                        i += k;
                    }
                }
            }
            if let Step::Consumed = self.step(input[i], out, markers) {
                i += 1;
            }
        }
    }

    fn finish(&mut self, out: &mut Vec<u8>) {
        self.release(out);
    }

    fn stats(&self) -> ScanStats {
        self.stats
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NONCE: Nonce = Nonce::from_bytes([0x5a; 16]);

    fn marker(nonce: &str, enc: char, payload: &str, st: &str) -> Vec<u8> {
        format!("\x1b]7777;cinelog;1;{nonce};{enc};{payload}{st}").into_bytes()
    }

    fn session(payload: &str) -> Vec<u8> {
        marker(&NONCE.to_hex(), 'b', payload, "\x07")
    }

    struct Run {
        out: Vec<u8>,
        /// Marker positions as absolute offsets into the concatenated output.
        markers: Vec<ScannedMarker>,
        stats: ScanStats,
    }

    fn run_chunks<'a>(chunks: impl IntoIterator<Item = &'a [u8]>, strip: bool) -> Run {
        let mut s = Scanner::new(Some(NONCE), strip);
        let (mut out, mut markers) = (Vec::new(), Vec::new());
        for c in chunks {
            // Fresh buffers per feed, as the recorder does; rebase positions afterwards.
            let (mut o, mut m) = (Vec::new(), Vec::new());
            s.feed(c, &mut o, &mut m);
            for mut mk in m {
                mk.at += out.len();
                markers.push(mk);
            }
            out.extend_from_slice(&o);
        }
        s.finish(&mut out);
        Run {
            out,
            markers,
            stats: s.stats(),
        }
    }

    fn run(input: &[u8], strip: bool) -> Run {
        run_chunks([input], strip)
    }

    /// Asserts identical results for one-shot, every two-way split and byte-by-byte feeding.
    fn assert_split_invariant(input: &[u8], strip: bool) -> Run {
        let whole = run(input, strip);
        for split in 0..=input.len() {
            let (a, b) = input.split_at(split);
            let r = run_chunks([a, b], strip);
            assert_eq!(r.out, whole.out, "split at {split}");
            assert_eq!(r.markers, whole.markers, "split at {split}");
            assert_eq!(r.stats, whole.stats, "split at {split}");
        }
        let bytewise = run_chunks(input.chunks(1), strip);
        assert_eq!(bytewise.out, whole.out);
        assert_eq!(bytewise.markers, whole.markers);
        assert_eq!(bytewise.stats, whole.stats);
        whole
    }

    #[test]
    fn plain_output_passes_through() {
        let input = b"hello\x1b[31mred\x1b[0m\x1b]0;title\x07\x1b]7777;other\x07\x1b\\x";
        for strip in [true, false] {
            let r = assert_split_invariant(input, strip);
            assert_eq!(r.out, input);
            assert!(r.markers.is_empty());
            assert_eq!(r.stats, ScanStats::default());
        }
    }

    #[test]
    fn markers_at_every_split_with_and_without_strip() {
        let mut input = b"ab".to_vec();
        input.extend(session("eyJ4IjoxfQ"));
        input.extend(b"cd\x1b]0;t\x07\x1b]7777;cinelo");
        input.extend(marker("-", 'j', r#"{"v":1,"k":"x"}"#, "\x1b\\"));
        input.extend(b"ef\x1b");
        input.extend(marker(&NONCE.to_hex(), 'b', "P3", "\x07"));

        let r = assert_split_invariant(&input, true);
        assert_eq!(r.out, b"abcd\x1b]0;t\x07\x1b]7777;cineloef\x1b");
        let got: Vec<_> = r
            .markers
            .iter()
            .map(|m| (m.at, m.nonce, m.enc, m.payload.clone()))
            .collect();
        assert_eq!(
            got,
            vec![
                (2, NonceField::Session(NONCE), Encoding::Base64, b"eyJ4IjoxfQ".to_vec()),
                (23, NonceField::Recorder, Encoding::Json, br#"{"v":1,"k":"x"}"#.to_vec()),
                (26, NonceField::Session(NONCE), Encoding::Base64, b"P3".to_vec()),
            ]
        );
        assert_eq!(r.stats.accepted, 3);
        assert_eq!(r.stats.bad_markers(), 0);

        let kept = assert_split_invariant(&input, false);
        assert_eq!(kept.out, input);
        assert_eq!(kept.markers.len(), 3);
        // Without stripping `at` points at the marker's ESC in the output.
        for m in &kept.markers {
            assert!(kept.out[m.at..].starts_with(WIRE_PREFIX));
        }
    }

    #[test]
    fn bad_nonce_passes_through_and_is_counted() {
        let other = Nonce::from_bytes([1; 16]).to_hex();
        let input = [b"x".as_slice(), &marker(&other, 'b', "AAAA", "\x07"), b"y"].concat();
        let r = assert_split_invariant(&input, true);
        assert_eq!(r.out, input);
        assert!(r.markers.is_empty());
        assert_eq!(r.stats.bad_nonce, 1);
        assert_eq!(r.stats.bad_markers(), 1);
    }

    #[test]
    fn without_expected_nonce_only_recorder_markers_are_accepted() {
        let input = [session("AAAA"), marker("-", 'b', "BBBB", "\x07")].concat();
        let mut s = Scanner::new(None, true);
        let (mut out, mut markers) = (Vec::new(), Vec::new());
        s.feed(&input, &mut out, &mut markers);
        assert_eq!(out, session("AAAA"));
        assert_eq!(markers.len(), 1);
        assert_eq!(markers[0].nonce, NonceField::Recorder);
        assert_eq!(s.stats().bad_nonce, 1);
    }

    #[test]
    fn can_and_sub_abort() {
        for ctl in [CAN, SUB] {
            let mut input = session("AA");
            input.pop(); // drop BEL
            input.push(ctl);
            input.extend(b"rest\x07");
            let r = assert_split_invariant(&input, true);
            assert_eq!(r.out, input);
            assert!(r.markers.is_empty());
            assert_eq!(r.stats.aborted, 1);

            // Inside the header as well.
            let input = [b"\x1b]7777;cinelog;1;".as_slice(), &[ctl], b"z"].concat();
            let r = assert_split_invariant(&input, true);
            assert_eq!(r.out, input);
            assert_eq!(r.stats.aborted, 1);
        }
    }

    #[test]
    fn payload_cap_is_64_kib() {
        let max = "A".repeat(MAX_ENCODED_PAYLOAD);
        let r = run(&session(&max), true);
        assert_eq!(r.markers.len(), 1);
        assert_eq!(r.markers[0].payload.len(), MAX_ENCODED_PAYLOAD);
        assert!(r.out.is_empty());

        let over = session(&format!("{max}A"));
        for strip in [true, false] {
            let r = run(&over, strip);
            assert_eq!(r.out, over);
            assert!(r.markers.is_empty());
            assert_eq!(r.stats.oversized, 1);
        }
        // Released bytes are rescanned, so a marker right behind an oversized one is found.
        let input = [over.clone(), session("AAAA")].concat();
        let r = run_chunks(input.chunks(4096), true);
        assert_eq!(r.out, over);
        assert_eq!(r.markers.len(), 1);
    }

    #[test]
    fn malformed_headers_pass_through() {
        let hex = NONCE.to_hex();
        let cases: Vec<Vec<u8>> = vec![
            b"\x1b]7777;cinelog;;x\x07".to_vec(),
            b"\x1b]7777;cinelog;x;\x07".to_vec(),
            b"\x1b]7777;cinelog;1234567890;\x07".to_vec(),
            format!("\x1b]7777;cinelog;1;{};b;AA\x07", hex.to_uppercase()).into_bytes(),
            format!("\x1b]7777;cinelog;1;{};b;AA\x07", &hex[..31]).into_bytes(),
            format!("\x1b]7777;cinelog;1;{hex}0;b;AA\x07").into_bytes(),
            b"\x1b]7777;cinelog;1;--;b;AA\x07".to_vec(),
            format!("\x1b]7777;cinelog;1;{hex};x;AA\x07").into_bytes(),
            format!("\x1b]7777;cinelog;1;{hex};bb;AA\x07").into_bytes(),
            format!("\x1b]7777;cinelog;1;{hex};b;A\nA\x07").into_bytes(),
            format!("\x1b]7777;cinelog;1;{hex};b;A\x7fA\x07").into_bytes(),
            format!("\x1b]7777;cinelog;1;{hex};b;AA\x1bxyz").into_bytes(),
        ];
        for input in cases {
            let r = assert_split_invariant(&input, true);
            assert_eq!(r.out, input, "{:?}", String::from_utf8_lossy(&input));
            assert!(r.markers.is_empty());
            assert_eq!(r.stats.malformed, 1, "{:?}", String::from_utf8_lossy(&input));
        }
    }

    #[test]
    fn unsupported_version_passes_through() {
        let input = format!("\x1b]7777;cinelog;2;{};b;AA\x07", NONCE.to_hex()).into_bytes();
        let r = assert_split_invariant(&input, true);
        assert_eq!(r.out, input);
        assert_eq!(r.stats.unsupported_version, 1);
        assert_eq!(r.stats.bad_markers(), 1);
    }

    #[test]
    fn broken_marker_does_not_hide_the_next_one() {
        let hex = NONCE.to_hex();
        let broken_payload = format!("\x1b]7777;cinelog;1;{hex};b;AA");
        let broken_header = "\x1b]7777;cinelog;1;5a5a";
        for broken in [broken_payload, broken_header.to_owned(), "\x1b]7777;ci".to_owned()] {
            let input = [broken.as_bytes(), &session("QQ")].concat();
            let r = assert_split_invariant(&input, true);
            assert_eq!(r.out, broken.as_bytes());
            assert_eq!(r.markers.len(), 1);
            assert_eq!(r.markers[0].at, broken.len());
        }
    }

    #[test]
    fn finish_and_flush_release_held_bytes() {
        let partial = &session("AAAA")[..30];
        let mut s = Scanner::new(Some(NONCE), true);
        let (mut out, mut markers) = (Vec::new(), Vec::new());
        s.feed(partial, &mut out, &mut markers);
        assert!(out.is_empty());
        assert_eq!(s.held_len(), partial.len());
        s.flush(&mut out);
        assert_eq!(out, partial);
        assert_eq!(s.held_len(), 0);
        s.feed(&session("AAAA"), &mut out, &mut markers);
        assert_eq!(markers.len(), 1);
        s.feed(b"\x1b]77", &mut out, &mut markers);
        s.finish(&mut out);
        assert_eq!(out, [partial, b"\x1b]77"].concat());
    }

    #[test]
    fn marker_position_counts_existing_out_bytes() {
        let mut s = Scanner::new(Some(NONCE), true);
        let mut out = b"previous".to_vec();
        let mut markers = Vec::new();
        s.feed(&[b"ab".as_slice(), &session("AA")].concat(), &mut out, &mut markers);
        assert_eq!(markers[0].at, b"previousab".len());
    }

    #[test]
    fn debug_never_prints_held_nonce_bytes() {
        let wire = session("AAAA");
        let header_with_nonce = &wire[..WIRE_PREFIX.len() + 2 + Nonce::HEX_LEN + 1];
        let mut s = Scanner::new(Some(NONCE), true);
        let (mut out, mut markers) = (Vec::new(), Vec::new());
        s.feed(header_with_nonce, &mut out, &mut markers);
        assert_eq!(s.held_len(), header_with_nonce.len());
        let debug = format!("{s:?} {s:#?}");
        let hex = NONCE.to_hex();
        let held_as_list = format!("{:?}", hex.as_bytes());
        assert!(!debug.contains(&hex), "{debug}");
        assert!(!debug.contains(&held_as_list[1..held_as_list.len() - 1]), "{debug}");
        assert!(
            debug.contains(&format!("held_len: {}", header_with_nonce.len())),
            "{debug}"
        );
    }
}
