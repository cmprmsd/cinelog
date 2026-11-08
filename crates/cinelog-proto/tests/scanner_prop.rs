//! Property tests: random streams with embedded markers, fed in random chunks.

use cinelog_proto::{
    Encoding, MarkerScanner, Nonce, NonceField, ScanStats, ScannedMarker, Scanner, Skip, WireMessage, decode_payload,
    encode_marker_as,
};
use proptest::prelude::*;

const NONCE: Nonce = Nonce::from_bytes([0x3e; 16]);
const OTHER: Nonce = Nonce::from_bytes([0x11; 16]);

/// Noise biased towards bytes that interact with the scanner. It contains no `-`, so it can
/// never form an accepted marker on its own (a session nonce would need 32 exact hex digits).
fn noise() -> impl Strategy<Value = Vec<u8>> {
    let alphabet: &[u8] = b"\x1b\x1b\x1b]]777;;;cinelog1bj\x07\x07\\\\\x18\x1a\n\x7fAxZ \xc3\xa9";
    prop::collection::vec(prop::sample::select(alphabet.to_vec()), 0..40)
}

#[derive(Debug, Clone)]
enum Segment {
    Noise(Vec<u8>),
    /// A marker the scanner must accept; carries its wire bytes.
    Good(Vec<u8>),
    /// A marker with a foreign nonce; passes through.
    Foreign(Vec<u8>),
}

fn wire(nonce: NonceField, enc: Encoding, sh: u32, st_esc: bool) -> Vec<u8> {
    let msg = WireMessage::Skip(Skip {
        sh,
        t: 0.5,
        extra: Default::default(),
    });
    let mut w = encode_marker_as(nonce, enc, &msg);
    if st_esc {
        w.pop();
        w.extend_from_slice(b"\x1b\\");
    }
    w
}

fn segment() -> impl Strategy<Value = Segment> {
    let enc = prop_oneof![Just(Encoding::Base64), Just(Encoding::Json)];
    prop_oneof![
        3 => noise().prop_map(Segment::Noise),
        2 => (enc.clone(), any::<u32>(), any::<bool>(), any::<bool>()).prop_map(|(enc, sh, esc, dash)| {
            let nonce = if dash { NonceField::Recorder } else { NonceField::Session(NONCE) };
            Segment::Good(wire(nonce, enc, sh, esc))
        }),
        1 => (enc, any::<u32>(), any::<bool>())
            .prop_map(|(enc, sh, esc)| Segment::Foreign(wire(NonceField::Session(OTHER), enc, sh, esc))),
    ]
}

fn chunked(input: &[u8], cuts: &[usize], strip: bool) -> (Vec<u8>, Vec<ScannedMarker>, ScanStats) {
    let mut cuts: Vec<usize> = cuts.iter().map(|c| c % (input.len() + 1)).collect();
    cuts.sort_unstable();
    let mut s = Scanner::new(Some(NONCE), strip);
    let (mut out, mut markers) = (Vec::new(), Vec::new());
    let mut start = 0;
    for cut in cuts.into_iter().chain([input.len()]) {
        // One `out` across all feeds: `at` is then absolute.
        s.feed(&input[start..cut], &mut out, &mut markers);
        start = cut;
    }
    s.finish(&mut out);
    (out, markers, s.stats())
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(2000))]

    #[test]
    fn embedded_markers_are_found_regardless_of_splits(
        segments in prop::collection::vec(segment(), 0..12),
        cuts in prop::collection::vec(any::<usize>(), 0..8),
    ) {
        let mut input = Vec::new();
        let mut expected_out = Vec::new();
        let mut expected = Vec::new();
        let mut foreign = 0u64;
        for seg in &segments {
            match seg {
                Segment::Noise(b) | Segment::Foreign(b) => {
                    input.extend_from_slice(b);
                    expected_out.extend_from_slice(b);
                    foreign += u64::from(matches!(seg, Segment::Foreign(_)));
                }
                Segment::Good(b) => {
                    input.extend_from_slice(b);
                    expected.push((expected_out.len(), b.clone()));
                }
            }
        }

        let (out, markers, stats) = chunked(&input, &cuts, true);
        prop_assert_eq!(&out, &expected_out);
        prop_assert_eq!(markers.len(), expected.len());
        prop_assert_eq!(stats.accepted, expected.len() as u64);
        prop_assert_eq!(stats.bad_nonce, foreign);
        for (m, (at, _)) in markers.iter().zip(&expected) {
            prop_assert_eq!(m.at, *at);
            // `-` with `skip` is a nonce/kind mismatch, so check the payload alone.
            prop_assert!(matches!(decode_payload(m.enc, &m.payload), Ok(WireMessage::Skip(_))));
        }

        let (whole_out, whole_markers, whole_stats) = chunked(&input, &[], true);
        prop_assert_eq!(whole_out, out);
        prop_assert_eq!(whole_markers, markers);
        prop_assert_eq!(whole_stats, stats);

        // Without stripping the stream is byte-identical and `at` points at each marker.
        let (kept, kept_markers, _) = chunked(&input, &cuts, false);
        prop_assert_eq!(&kept, &input);
        for (m, (_, bytes)) in kept_markers.iter().zip(&expected) {
            prop_assert!(kept[m.at..].starts_with(bytes));
        }
    }

    #[test]
    fn arbitrary_bytes_are_split_invariant_and_lossless(
        input in prop::collection::vec(any::<u8>(), 0..512),
        cuts in prop::collection::vec(any::<usize>(), 0..8),
    ) {
        let whole = chunked(&input, &[], false);
        prop_assert_eq!(&whole.0, &input);
        let split = chunked(&input, &cuts, false);
        prop_assert_eq!(&split, &whole);
        let stripped = chunked(&input, &cuts, true);
        prop_assert_eq!(stripped.2, whole.2);
    }
}
