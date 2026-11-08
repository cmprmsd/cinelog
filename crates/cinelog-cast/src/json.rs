//! Minimal JSON string-literal handling for the event-line fast path.

use memchr::{memchr, memchr2};

/// Returns the index one past the closing quote of the string literal at `s[0] == b'"'`.
pub(crate) fn string_literal_end(s: &[u8]) -> Option<usize> {
    if s.first() != Some(&b'"') {
        return None;
    }
    let mut i = 1;
    loop {
        let j = i + memchr2(b'"', b'\\', s.get(i..)?)?;
        if s[j] == b'"' {
            return Some(j + 1);
        }
        i = j + 2;
    }
}

/// Decodes a JSON string literal (with or without its quotes) into a `String`.
///
/// Tolerant by design, because casts come from many writers: lone surrogates, malformed
/// `\u` escapes and invalid UTF-8 become U+FFFD, unknown escapes keep the escaped byte,
/// and raw control characters are accepted. Never fails.
pub fn decode_json_string(literal: &[u8]) -> String {
    let inner = match literal {
        [b'"', inner @ .., b'"'] => inner,
        other => other,
    };
    if memchr(b'\\', inner).is_none() {
        return String::from_utf8_lossy(inner).into_owned();
    }
    let mut out = Vec::with_capacity(inner.len());
    let mut i = 0;
    while let Some(off) = memchr(b'\\', &inner[i..]) {
        out.extend_from_slice(&inner[i..i + off]);
        i += off + 1;
        let Some(&esc) = inner.get(i) else { break };
        i += 1;
        match esc {
            b'b' => out.push(0x08),
            b'f' => out.push(0x0c),
            b'n' => out.push(b'\n'),
            b'r' => out.push(b'\r'),
            b't' => out.push(b'\t'),
            b'u' => {
                let ch = match hex4(inner, i) {
                    Some(hi @ 0xD800..=0xDBFF) => {
                        i += 4;
                        match (inner.get(i..i + 2), hex4(inner, i + 2)) {
                            (Some(b"\\u"), Some(lo @ 0xDC00..=0xDFFF)) => {
                                i += 6;
                                char::from_u32(0x10000 + ((hi - 0xD800) << 10) + (lo - 0xDC00))
                            }
                            _ => None,
                        }
                    }
                    Some(cu) => {
                        i += 4;
                        char::from_u32(cu)
                    }
                    None => None,
                };
                let mut buf = [0u8; 4];
                out.extend_from_slice(
                    ch.unwrap_or(char::REPLACEMENT_CHARACTER)
                        .encode_utf8(&mut buf)
                        .as_bytes(),
                );
            }
            // `"`, `\`, `/` and any unknown escape keep the escaped byte.
            other => out.push(other),
        }
    }
    out.extend_from_slice(&inner[i..]);
    match String::from_utf8(out) {
        Ok(s) => s,
        Err(e) => String::from_utf8_lossy(e.as_bytes()).into_owned(),
    }
}

fn hex4(s: &[u8], at: usize) -> Option<u32> {
    let digits = s.get(at..at + 4)?;
    digits
        .iter()
        .try_fold(0u32, |acc, &d| Some(acc << 4 | char::from(d).to_digit(16)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn literal_end_handles_escapes() {
        assert_eq!(string_literal_end(br#""abc", x"#), Some(5));
        assert_eq!(string_literal_end(br#""a\"b""#), Some(6));
        assert_eq!(string_literal_end(br#""a\\""#), Some(5));
        assert_eq!(string_literal_end(br#""unterminated"#), None);
        assert_eq!(string_literal_end(br#""ends in \"#), None);
        assert_eq!(string_literal_end(b"noquote"), None);
    }

    #[test]
    fn decodes_standard_escapes() {
        assert_eq!(
            decode_json_string(br#""a\nb\t\"\\\/\r\b\f""#),
            "a\nb\t\"\\/\r\u{8}\u{c}"
        );
        assert_eq!(decode_json_string(br#""\u001b[0m\u00e4""#), "\u{1b}[0m\u{e4}");
        assert_eq!(decode_json_string(br#""\ud83d\ude00""#), "\u{1f600}");
        assert_eq!(decode_json_string("\"raw ü\"".as_bytes()), "raw ü");
    }

    #[test]
    fn tolerates_malformed_input() {
        assert_eq!(decode_json_string(br#""\ud83d x""#), "\u{FFFD} x");
        assert_eq!(decode_json_string(br#""\ude00""#), "\u{FFFD}");
        assert_eq!(decode_json_string(br#""\u12""#), "\u{FFFD}12");
        assert_eq!(decode_json_string(br#""\q""#), "q");
        assert_eq!(decode_json_string(b"\"\xff\\n\""), "\u{FFFD}\n");
    }

    #[test]
    fn matches_serde_on_serde_output() {
        let samples = ["", "plain", "\u{1b}]7777;x\u{7}", "tab\tnl\n\"q\" \\ ü 😀 \u{7f}\u{0}"];
        for s in samples {
            let lit = serde_json::to_vec(s).unwrap();
            assert_eq!(decode_json_string(&lit), s);
        }
    }
}
