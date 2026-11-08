//! Tolerant event-line parser: `[<time>, "<code>", "<data>"]`.

use crate::json::{decode_json_string, string_literal_end};
use crate::{EventCode, RawLine};

/// Splits one event line into time, code and the still-escaped data literal.
///
/// Accepts any JSON whitespace (including a trailing `\r`), integer, decimal and exponent
/// times, and any single-character code. Decimal times are converted to µs exactly
/// (rounded at the seventh fractional digit); negative times are clamped to 0. Returns
/// `None` for anything that is not a complete event.
pub fn parse_event_line(line: &[u8]) -> Option<RawLine<'_>> {
    let mut i = expect(line, skip_ws(line, 0), b'[')?;
    let (time_us, n) = parse_time(&line[i..])?;
    i = expect(line, skip_ws(line, i + n), b',')?;

    let code_end = i + string_literal_end(&line[i..])?;
    let code = parse_code(&line[i..code_end])?;
    i = expect(line, skip_ws(line, code_end), b',')?;

    let data_end = i + string_literal_end(&line[i..])?;
    let data_json = &line[i..data_end];
    i = expect(line, skip_ws(line, data_end), b']')?;

    (i == line.len()).then_some(RawLine {
        time_us,
        code,
        data_json,
    })
}

impl RawLine<'_> {
    /// Decodes the data string (see [`decode_json_string`]).
    pub fn decode_data(&self) -> String {
        decode_json_string(self.data_json)
    }
}

fn skip_ws(s: &[u8], mut i: usize) -> usize {
    while matches!(s.get(i), Some(b' ' | b'\t' | b'\r' | b'\n')) {
        i += 1;
    }
    i
}

/// Expects byte `b` at `i` and returns the index after it and any following whitespace.
fn expect(s: &[u8], i: usize, b: u8) -> Option<usize> {
    (s.get(i) == Some(&b)).then(|| skip_ws(s, i + 1))
}

fn parse_code(literal: &[u8]) -> Option<EventCode> {
    if let [b'"', c, b'"'] = literal
        && c.is_ascii()
        && *c != b'\\'
    {
        return Some(EventCode::from_char(char::from(*c)));
    }
    let decoded = decode_json_string(literal);
    let mut chars = decoded.chars();
    match (chars.next(), chars.next()) {
        (Some(c), None) => Some(EventCode::from_char(c)),
        _ => None,
    }
}

fn count_digits(s: &[u8], from: usize) -> usize {
    s[from..].iter().take_while(|b| b.is_ascii_digit()).count()
}

/// Parses a JSON number of seconds into µs. Returns the value and the bytes consumed.
pub(crate) fn parse_time(s: &[u8]) -> Option<(u64, usize)> {
    let neg = s.first() == Some(&b'-');
    let int_start = usize::from(neg);
    let int_len = count_digits(s, int_start);
    let mut i = int_start + int_len;
    let mut frac: &[u8] = &[];
    if s.get(i) == Some(&b'.') {
        let n = count_digits(s, i + 1);
        frac = &s[i + 1..i + 1 + n];
        i += 1 + n;
    }
    if int_len == 0 && frac.is_empty() {
        return None;
    }
    if matches!(s.get(i), Some(b'e' | b'E')) {
        i += 1;
        if matches!(s.get(i), Some(b'+' | b'-')) {
            i += 1;
        }
        let n = count_digits(s, i);
        if n == 0 {
            return None;
        }
        i += n;
        let secs: f64 = std::str::from_utf8(&s[..i]).ok()?.parse().ok()?;
        return Some((secs_to_us(secs), i));
    }
    if neg {
        return Some((0, i));
    }
    let whole = s[int_start..int_start + int_len]
        .iter()
        .try_fold(0u64, |acc, d| acc.checked_mul(10)?.checked_add(u64::from(d - b'0')))?;
    let mut micros = (0..6).fold(0u64, |acc, k| acc * 10 + frac.get(k).map_or(0, |d| u64::from(d - b'0')));
    if frac.get(6).is_some_and(|&d| d >= b'5') {
        micros += 1;
    }
    Some((whole.checked_mul(1_000_000)?.checked_add(micros)?, i))
}

fn secs_to_us(secs: f64) -> u64 {
    if secs.is_finite() && secs > 0.0 {
        // `as` saturates for out-of-range values.
        (secs * 1e6).round() as u64
    } else {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> Option<(u64, char, String)> {
        parse_event_line(s.as_bytes()).map(|r| (r.time_us, r.code.as_char(), r.decode_data()))
    }

    #[test]
    fn standard_lines() {
        assert_eq!(p(r#"[0.123456, "o", "hi\r\n"]"#), Some((123_456, 'o', "hi\r\n".into())));
        assert_eq!(p(r#"[12,"i","a"]"#), Some((12_000_000, 'i', "a".into())));
        assert_eq!(
            p("  [ 1.5 ,\t\"r\" , \"80x24\" ]\r"),
            Some((1_500_000, 'r', "80x24".into()))
        );
        assert_eq!(p(r#"[0.5, "x", "0"]"#), Some((500_000, 'x', "0".into())));
        assert_eq!(p(r#"[0.5, "q", ""]"#), Some((500_000, 'q', String::new())));
        assert_eq!(p(r#"[0.5, "m", "{}"]"#), Some((500_000, 'm', "{}".into())));
    }

    #[test]
    fn times() {
        assert_eq!(parse_time(b"0.1"), Some((100_000, 3)));
        assert_eq!(parse_time(b"1.0000005"), Some((1_000_001, 9)));
        assert_eq!(parse_time(b"1.0000004"), Some((1_000_000, 9)));
        assert_eq!(parse_time(b"1e-05"), Some((10, 5)));
        assert_eq!(parse_time(b"2.5E+1,"), Some((25_000_000, 6)));
        assert_eq!(parse_time(b"-0.4"), Some((0, 4)));
        assert_eq!(parse_time(b"-1e3"), Some((0, 4)));
        assert_eq!(parse_time(b".5"), Some((500_000, 2)));
        assert_eq!(parse_time(b"7."), Some((7_000_000, 2)));
        assert_eq!(parse_time(b"x"), None);
        assert_eq!(parse_time(b"-"), None);
        assert_eq!(parse_time(b"1e"), None);
        assert_eq!(parse_time(b"99999999999999999999"), None);
    }

    #[test]
    fn rejects_garbage() {
        for bad in [
            "",
            "[",
            "[0.1]",
            r#"[0.1, "o"]"#,
            r#"[0.1, "o", "x"] junk"#,
            r#"[0.1, "o", "x""#,
            r#"[0.1, "oo", "x"]"#,
            r#"[0.1, "", "x"]"#,
            r#"[0.1, o, "x"]"#,
            r#"[0.1, "o", 5]"#,
            r#"{"version": 3}"#,
            "\0\0\0",
        ] {
            assert_eq!(parse_event_line(bad.as_bytes()), None, "{bad:?}");
        }
    }

    #[test]
    fn raw_line_keeps_escaped_literal() {
        let line = br#"[0.1, "o", "a\"b"]"#;
        let raw = parse_event_line(line).unwrap();
        assert_eq!(raw.data_json, br#""a\"b""#);
        assert_eq!(raw.decode_data(), "a\"b");
    }
}
