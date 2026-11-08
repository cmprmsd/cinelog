//! Scanner invariants on arbitrary input and an arbitrary split: no panic, output identical
//! to one-shot feeding, byte-identical passthrough without stripping.
#![no_main]

use cinelog_proto::{MarkerScanner, Nonce, Scanner};
use libfuzzer_sys::fuzz_target;

const NONCE: Nonce = Nonce::from_bytes([0x5a; 16]);

fn scan(chunks: &[&[u8]], strip: bool) -> (Vec<u8>, usize) {
    let mut s = Scanner::new(Some(NONCE), strip);
    let (mut out, mut markers) = (Vec::new(), Vec::new());
    for c in chunks {
        s.feed(c, &mut out, &mut markers);
    }
    s.finish(&mut out);
    for m in &markers {
        let _ = m.decode();
    }
    (out, markers.len())
}

fuzz_target!(|data: &[u8]| {
    let Some((&cut, input)) = data.split_first() else { return };
    let split = usize::from(cut) * input.len() / 256;
    let (a, b) = input.split_at(split);
    let whole = scan(&[input], true);
    assert_eq!(scan(&[a, b], true), whole);
    let kept = scan(&[a, b], false);
    assert_eq!(kept.0, input);
    assert_eq!(kept.1, whole.1);
});
