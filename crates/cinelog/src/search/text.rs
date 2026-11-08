//! Terminal-safe rendering of untrusted index text (§7.6): commands and output never reach
//! the terminal with their control characters, and snippet sentinels become highlights.

use cinelog_index::model::{SNIPPET_CLOSE, SNIPPET_OPEN};
use jiff::Timestamp;
use jiff::tz::TimeZone;

pub const RESET: &str = "\x1b[0m";
pub const BOLD: &str = "\x1b[1m";
pub const DIM: &str = "\x1b[2m";
pub const RED: &str = "\x1b[31m";
pub const YELLOW: &str = "\x1b[33m";
pub const HIT: &str = "\x1b[1;33m";

/// Whether to emit ANSI styles.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Style {
    pub color: bool,
}

impl Style {
    #[cfg(test)]
    pub const PLAIN: Self = Self { color: false };
    #[cfg(test)]
    pub const COLOR: Self = Self { color: true };

    /// `text` wrapped in `sgr` … reset, or `text` alone without colour.
    pub fn paint(self, sgr: &str, text: &str) -> String {
        if self.color && !text.is_empty() {
            format!("{sgr}{text}{RESET}")
        } else {
            text.to_owned()
        }
    }
}

/// Local `YYYY-MM-DD HH:MM:SS` of an epoch-seconds value.
pub fn fmt_ts(ts: f64, tz: &TimeZone) -> String {
    let micros = (ts * 1e6).round();
    Timestamp::from_microsecond(micros as i64)
        .map(|t| t.to_zoned(tz.clone()).strftime("%Y-%m-%d %H:%M:%S").to_string())
        .unwrap_or_else(|_| format!("@{ts}"))
}

/// `HH:MM:SS` when `ts` falls on the same local day as `same_day_as`, else the full form.
pub fn fmt_ts_short(ts: f64, same_day_as: f64, tz: &TimeZone) -> String {
    let full = fmt_ts(ts, tz);
    let base = fmt_ts(same_day_as, tz);
    match (full.split_once(' '), base.split_once(' ')) {
        (Some((d, time)), Some((bd, _))) if d == bd => time.to_owned(),
        _ => full,
    }
}

/// Human duration: `850 ms`, `3.2 s`, `4 min 12 s`, `2 h 05 min`, `3 d 4 h`.
pub fn fmt_duration(secs: f64) -> String {
    let secs = secs.max(0.0);
    if secs < 1.0 {
        format!("{} ms", (secs * 1000.0).round() as u64)
    } else if secs < 60.0 {
        format!("{secs:.1} s")
    } else {
        let s = secs.round() as u64;
        if s < 3600 {
            format!("{} min {} s", s / 60, s % 60)
        } else if s < 86_400 {
            format!("{} h {:02} min", s / 3600, s % 3600 / 60)
        } else {
            format!("{} d {} h", s / 86_400, s % 86_400 / 3600)
        }
    }
}

/// `1.2 MiB` style sizes.
pub fn fmt_bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["KiB", "MiB", "GiB", "TiB", "PiB"];
    if n < 1024 {
        return format!("{n} B");
    }
    let mut v = n as f64 / 1024.0;
    let mut unit = 0;
    while v >= 1024.0 && unit + 1 < UNITS.len() {
        v /= 1024.0;
        unit += 1;
    }
    format!("{v:.1} {}", UNITS[unit])
}

/// Pushes `c` with control characters made visible (caret notation; C1 as U+FFFD).
fn push_visible(out: &mut String, c: char) {
    match c {
        '\0'..='\x1f' => {
            out.push('^');
            out.push(char::from(c as u8 + 0x40));
        }
        '\x7f' => out.push_str("^?"),
        '\u{80}'..='\u{9f}' => out.push('\u{FFFD}'),
        SNIPPET_OPEN | SNIPPET_CLOSE => {}
        c => out.push(c),
    }
}

/// A single display line of at most `max_chars` characters: newlines become `↵`, tabs a
/// space, other control characters are made visible, and an overlong value ends in `…`.
pub fn one_line(s: &str, max_chars: usize) -> String {
    let mut out = String::with_capacity(s.len().min(max_chars * 4));
    let mut n = 0;
    let mut chars = s.trim_end_matches(['\n', '\r']).chars().peekable();
    while let Some(c) = chars.next() {
        if n >= max_chars {
            out.push('…');
            break;
        }
        match c {
            '\r' if chars.peek() == Some(&'\n') => continue,
            '\n' | '\r' => out.push_str("↵ "),
            '\t' => out.push(' '),
            c => push_visible(&mut out, c),
        }
        n += 1;
    }
    out
}

/// Multi-line text with every control character except newline and tab made visible.
pub fn block(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\n' | '\t' => out.push(c),
            c => push_visible(&mut out, c),
        }
    }
    out
}

/// A snippet with its U+E000/U+E001 sentinels as highlights, on one line of at most
/// `max_chars` visible characters. Unbalanced sentinels are tolerated.
pub fn snippet(s: &str, max_chars: usize, style: Style) -> String {
    let mut out = String::new();
    let mut seg = String::new();
    let mut in_hit = false;
    let mut n = 0;
    let flush = |out: &mut String, seg: &mut String, hit: bool| {
        if hit {
            out.push_str(&style.paint(HIT, seg));
        } else {
            out.push_str(seg);
        }
        seg.clear();
    };
    let s = s.trim_matches(|c: char| c.is_whitespace());
    for c in s.chars() {
        match c {
            SNIPPET_OPEN | SNIPPET_CLOSE => {
                flush(&mut out, &mut seg, in_hit);
                in_hit = c == SNIPPET_OPEN;
                continue;
            }
            _ if n >= max_chars => {
                seg.push('…');
                break;
            }
            '\n' | '\r' => seg.push_str("↵ "),
            '\t' => seg.push(' '),
            c => push_visible(&mut seg, c),
        }
        n += 1;
    }
    flush(&mut out, &mut seg, in_hit);
    out
}

/// POSIX shell single-quoting.
pub fn shell_quote(s: &str) -> String {
    if !s.is_empty() && s.bytes().all(|b| b.is_ascii_alphanumeric() || b"/._-+:@%".contains(&b)) {
        return s.to_owned();
    }
    format!("'{}'", s.replace('\'', r"'\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn control_characters_never_reach_the_terminal() {
        let evil = "a\x1b]0;pwned\x07b\x1b[2Jc\u{9b}d\x7f";
        let line = one_line(evil, 100);
        assert!(!line.chars().any(|c| c.is_control()), "{line:?}");
        assert_eq!(line, "a^[]0;pwned^Gb^[[2Jc\u{FFFD}d^?");
        let b = block("x\n\ty\x1b[31m");
        assert_eq!(b, "x\n\ty^[[31m");
    }

    #[test]
    fn one_line_joins_lines_and_truncates() {
        assert_eq!(one_line("printf 'a\nb'\n", 100), "printf 'a↵ b'");
        assert_eq!(one_line("a\r\nb\tc", 100), "a↵ b c");
        assert_eq!(one_line("abcdef", 3), "abc…");
        assert_eq!(one_line("abc", 3), "abc");
        assert_eq!(one_line("ä\u{E000}b", 5), "äb");
    }

    #[test]
    fn snippets_highlight_hits() {
        let s = "  log \u{E000}needle\u{E001} found\n";
        assert_eq!(snippet(s, 100, Style::PLAIN), "log needle found");
        assert_eq!(snippet(s, 100, Style::COLOR), format!("log {HIT}needle{RESET} found"));
        assert_eq!(snippet("\u{E000}open", 100, Style::COLOR), format!("{HIT}open{RESET}"));
        assert_eq!(snippet("ab\u{E001}cd", 100, Style::PLAIN), "abcd");
        assert_eq!(snippet("abcdef", 2, Style::PLAIN), "ab…");
        // escape sequences inside a hit are neutralised too
        let hit = snippet("\u{E000}\x1b[2J\u{E001}", 10, Style::COLOR);
        assert_eq!(hit, format!("{HIT}^[[2J{RESET}"));
    }

    #[test]
    fn times_and_sizes() {
        let tz = TimeZone::UTC;
        assert_eq!(fmt_ts(1_794_139_200.4, &tz), "2026-11-08 12:00:00");
        assert_eq!(fmt_ts_short(1_794_139_260.0, 1_794_139_200.0, &tz), "12:01:00");
        assert_eq!(
            fmt_ts_short(1_794_239_260.0, 1_794_139_200.0, &tz),
            "2026-11-09 15:47:40"
        );
        assert_eq!(fmt_duration(0.25), "250 ms");
        assert_eq!(fmt_duration(3.21), "3.2 s");
        assert_eq!(fmt_duration(252.0), "4 min 12 s");
        assert_eq!(fmt_duration(7500.0), "2 h 05 min");
        assert_eq!(fmt_duration(3.0 * 86_400.0 + 4.0 * 3600.0), "3 d 4 h");
        assert_eq!(fmt_bytes(512), "512 B");
        assert_eq!(fmt_bytes(1536), "1.5 KiB");
        assert_eq!(fmt_bytes(3 << 30), "3.0 GiB");
    }

    #[test]
    fn quoting() {
        assert_eq!(shell_quote("/usr/bin/cinelog"), "/usr/bin/cinelog");
        assert_eq!(shell_quote("/a b/it's"), r"'/a b/it'\''s'");
        assert_eq!(shell_quote(""), "''");
    }
}
