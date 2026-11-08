//! Streaming UTF-8 decoding of PTY chunks that may split a character.

/// Decodes a byte stream into UTF-8 text across chunk boundaries.
///
/// An incomplete sequence at the end of a chunk is carried over to the next one. Invalid
/// bytes become U+FFFD using the same "maximal subpart" rule as
/// [`String::from_utf8_lossy`], so the output does not depend on where the stream was split.
#[derive(Debug, Default, Clone)]
pub struct Utf8Accumulator {
    /// Incomplete trailing sequence (at most 3 bytes).
    tail: Vec<u8>,
    scratch: Vec<u8>,
}

impl Utf8Accumulator {
    pub fn new() -> Self {
        Self::default()
    }

    /// Appends the decodable part of `bytes` to `out`. Returns the number of invalid bytes
    /// that were replaced by U+FFFD (counted as `lossy_bytes`).
    pub fn push(&mut self, bytes: &[u8], out: &mut String) -> u64 {
        if self.tail.is_empty() {
            return self.decode(bytes, out);
        }
        let mut joined = std::mem::take(&mut self.scratch);
        joined.clear();
        joined.append(&mut self.tail);
        joined.extend_from_slice(bytes);
        let lossy = self.decode(&joined, out);
        self.scratch = joined;
        lossy
    }

    /// Flushes a pending incomplete sequence as one U+FFFD. Returns the bytes replaced.
    pub fn finish(&mut self, out: &mut String) -> u64 {
        if self.tail.is_empty() {
            return 0;
        }
        out.push(char::REPLACEMENT_CHARACTER);
        let n = self.tail.len() as u64;
        self.tail.clear();
        n
    }

    /// Bytes of an incomplete sequence held back from the last `push`.
    pub fn pending(&self) -> usize {
        self.tail.len()
    }

    fn decode(&mut self, mut rest: &[u8], out: &mut String) -> u64 {
        let mut lossy = 0;
        out.reserve(rest.len());
        loop {
            match std::str::from_utf8(rest) {
                Ok(s) => {
                    out.push_str(s);
                    return lossy;
                }
                Err(e) => {
                    let (ok, bad) = rest.split_at(e.valid_up_to());
                    // `valid_up_to` guarantees that `ok` is valid UTF-8.
                    out.push_str(std::str::from_utf8(ok).unwrap_or_default());
                    match e.error_len() {
                        Some(n) => {
                            out.push(char::REPLACEMENT_CHARACTER);
                            lossy += n as u64;
                            rest = &bad[n..];
                        }
                        None => {
                            self.tail.extend_from_slice(bad);
                            return lossy;
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn run(chunks: &[&[u8]]) -> (String, u64) {
        let mut acc = Utf8Accumulator::new();
        let mut out = String::new();
        let mut lossy = 0;
        for c in chunks {
            lossy += acc.push(c, &mut out);
        }
        lossy += acc.finish(&mut out);
        (out, lossy)
    }

    #[test]
    fn split_multibyte_char() {
        let s = "aä€😀z".as_bytes();
        for split in 0..=s.len() {
            let (a, b) = s.split_at(split);
            assert_eq!(run(&[a, b]), ("aä€😀z".to_owned(), 0), "split at {split}");
        }
    }

    #[test]
    fn every_byte_alone() {
        let s = "€😀".as_bytes();
        let chunks: Vec<&[u8]> = s.chunks(1).collect();
        assert_eq!(run(&chunks), ("€😀".to_owned(), 0));
    }

    #[test]
    fn invalid_bytes_are_replaced_and_counted() {
        assert_eq!(run(&[b"a\xffb"]), ("a\u{FFFD}b".to_owned(), 1));
        // Truncated 3-byte sequence followed by ASCII: one replacement for the 2-byte prefix.
        assert_eq!(run(&[b"\xe2\x82", b"x"]), ("\u{FFFD}x".to_owned(), 2));
        // Incomplete sequence at the very end is flushed by `finish`.
        assert_eq!(run(&[b"ok\xf0\x9f"]), ("ok\u{FFFD}".to_owned(), 2));
    }

    #[test]
    fn pending_is_reported() {
        let mut acc = Utf8Accumulator::new();
        let mut out = String::new();
        acc.push(b"\xe2\x82", &mut out);
        assert_eq!(acc.pending(), 2);
        acc.push(b"\xac", &mut out);
        assert_eq!((out.as_str(), acc.pending()), ("€", 0));
    }

    proptest! {
        #[test]
        fn split_invariant(bytes in proptest::collection::vec(any::<u8>(), 0..64), cuts in proptest::collection::vec(0usize..64, 0..6)) {
            let whole = run(&[&bytes]);
            prop_assert_eq!(&whole.0, &String::from_utf8_lossy(&bytes).into_owned());
            let mut cuts: Vec<usize> = cuts.into_iter().map(|c| c.min(bytes.len())).collect();
            cuts.sort_unstable();
            let mut chunks = Vec::new();
            let mut prev = 0;
            for c in cuts {
                chunks.push(&bytes[prev..c]);
                prev = c;
            }
            chunks.push(&bytes[prev..]);
            prop_assert_eq!(run(&chunks), whole);
        }
    }
}
