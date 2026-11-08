//! Streaming scanners over the raw `o` text of a legacy cast (§11 step 3): the old plugin's
//! `Begin`/`End` banners and the window title its preexec hook set right after `Begin`.
//!
//! The plugin printed `printf '%*s\n' $COLUMNS "Begin [%Y-%m-%d %H:%M:%S]"` (and `End` padded
//! to the same width with three spaces), then `print -Pn '\e]0;%n@%m: %~ <cmd>\a'`. Both can
//! be split across events; a banner prefix at the end of an event is carried over.

use std::sync::LazyLock;

use regex::Regex;
use serde::{Deserialize, Serialize};

use crate::model::Stamp;

static BANNER: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(Begin|End  ) \[(\d{4}-\d\d-\d\d \d\d:\d\d:\d\d)\]").expect("valid regex"));

/// A whole banner line inside rendered chunk text (right-aligned with padding).
static BANNER_LINE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\s*(?:Begin|End  ) \[(\d{4}-\d\d-\d\d \d\d:\d\d:\d\d)\]\s*$").expect("valid regex"));

/// Shapes of the two banners; `0` stands for a digit.
const TEMPLATES: [&[u8]; 2] = [b"Begin [0000-00-00 00:00:00]", b"End   [0000-00-00 00:00:00]"];
const BANNER_LEN: usize = 27;

/// Longest window title kept (longer ones are cut; the command is at their end, so the
/// cut keeps the head, which is enough for a prefix comparison).
const MAX_TITLE: usize = 4096;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum BannerKind {
    Begin,
    End,
}

/// What [`Scanner::feed`] reports, in stream order.
#[derive(Debug, PartialEq)]
pub(crate) enum Piece<'a> {
    /// Text to render (never contains the start of a banner).
    Text { at: Stamp, text: &'a str },
    /// A banner starts here; its text follows as the next [`Piece::Text`].
    Banner { at: Stamp, kind: BannerKind, wall: i64 },
}

/// Banner scanner with carry-over. Serializable for checkpoints.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct Scanner {
    /// Unfed tail that may be the start of a banner.
    carry: String,
    /// Time of the event the carry came from (bit patterns, for exact resumption).
    carry_at: (u64, u64),
}

impl Scanner {
    /// Scans `data` (prefixed by the carry) and reports text and banners through `f`.
    pub(crate) fn feed(&mut self, at: Stamp, data: &str, f: &mut dyn FnMut(Piece<'_>)) {
        let carry_at = Stamp {
            t: f64::from_bits(self.carry_at.0),
            ts: f64::from_bits(self.carry_at.1),
        };
        let carry_len = self.carry.len();
        let joined;
        let buf: &str = if carry_len == 0 {
            data
        } else {
            joined = std::mem::take(&mut self.carry) + data;
            &joined
        };
        let stamp_of = |pos: usize| if pos < carry_len { carry_at } else { at };
        let emit_text = |f: &mut dyn FnMut(Piece<'_>), from: usize, to: usize| {
            if from < carry_len && carry_len < to {
                f(Piece::Text {
                    at: carry_at,
                    text: &buf[from..carry_len],
                });
                f(Piece::Text {
                    at,
                    text: &buf[carry_len..to],
                });
            } else if from < to {
                f(Piece::Text {
                    at: stamp_of(from),
                    text: &buf[from..to],
                });
            }
        };
        let mut from = 0;
        for c in BANNER.captures_iter(buf) {
            let m = c.get(0).expect("whole match");
            let Some(wall) = parse_wall(&c[2]) else {
                continue;
            };
            let kind = if &c[1] == "Begin" {
                BannerKind::Begin
            } else {
                BannerKind::End
            };
            emit_text(f, from, m.start());
            f(Piece::Banner {
                at: stamp_of(m.start()),
                kind,
                wall,
            });
            from = m.start();
        }
        let keep = held_back(&buf.as_bytes()[from..]).map_or(buf.len(), |k| buf.len() - k);
        emit_text(f, from, keep);
        if keep < buf.len() {
            let held = stamp_of(keep);
            self.carry = buf[keep..].to_owned();
            self.carry_at = (held.t.to_bits(), held.ts.to_bits());
        }
    }

    /// Moves a held-back carry to `at` (a later event that is not output, e.g. a resize),
    /// so whatever it turns into is reported after that event, never before it.
    pub(crate) fn retime(&mut self, at: Stamp) {
        if !self.carry.is_empty() {
            self.carry_at = (at.t.to_bits(), at.ts.to_bits());
        }
    }

    /// Releases the carry as plain text (end of data).
    pub(crate) fn flush(&mut self, f: &mut dyn FnMut(Piece<'_>)) {
        if self.carry.is_empty() {
            return;
        }
        let carry = std::mem::take(&mut self.carry);
        f(Piece::Text {
            at: Stamp {
                t: f64::from_bits(self.carry_at.0),
                ts: f64::from_bits(self.carry_at.1),
            },
            text: &carry,
        });
    }
}

/// Length of the tail of `s` that is a proper prefix of a banner, if any.
fn held_back(s: &[u8]) -> Option<usize> {
    let start = s.len().saturating_sub(BANNER_LEN - 1);
    (start..s.len())
        .find(|&p| matches!(s[p], b'B' | b'E') && is_banner_prefix(&s[p..]))
        .map(|p| s.len() - p)
}

fn is_banner_prefix(s: &[u8]) -> bool {
    s.len() < BANNER_LEN
        && TEMPLATES.iter().any(|t| {
            s.iter()
                .zip(t.iter())
                .all(|(c, p)| if *p == b'0' { c.is_ascii_digit() } else { c == p })
        })
}

/// `YYYY-MM-DD HH:MM:SS` as seconds of a naive (zone-less) clock, `None` if not a valid
/// date and time.
pub(crate) fn parse_wall(s: &str) -> Option<i64> {
    let dt: jiff::civil::DateTime = s.replacen(' ', "T", 1).parse().ok()?;
    dt.to_zoned(jiff::tz::TimeZone::UTC)
        .ok()
        .map(|z| z.timestamp().as_second())
}

/// Drops the lines of the banners `is_banner` accepts (by wall clock) from rendered chunk
/// text; `None` when nothing else is left.
pub(crate) fn strip_banner_lines(text: &str, is_banner: impl Fn(i64) -> bool) -> Option<String> {
    if !text.contains("Begin [") && !text.contains("End   [") {
        return Some(text.to_owned());
    }
    let banner = |l: &str| {
        BANNER_LINE
            .captures(l)
            .and_then(|c| parse_wall(&c[1]))
            .is_some_and(&is_banner)
    };
    let kept: Vec<&str> = text.split('\n').filter(|l| !banner(l)).collect();
    let out = kept.join("\n");
    (!out.trim().is_empty()).then_some(out)
}

/// OSC 0/2 (window title) extractor. Serializable for checkpoints.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct TitleScanner {
    state: Osc,
    buf: String,
    /// The current OSC is a title (`0;` or `2;`), decided once its number is complete.
    title: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
enum Osc {
    #[default]
    Ground,
    Esc,
    Number,
    Body,
    /// ESC inside the body: `ESC \` ends it.
    BodyEsc,
}

impl TitleScanner {
    /// Feeds rendered text; returns the titles completed in it.
    pub(crate) fn feed(&mut self, text: &str, out: &mut Vec<String>) {
        let mut rest = text;
        while !rest.is_empty() {
            if self.state == Osc::Ground {
                match rest.find('\x1b') {
                    Some(i) => {
                        rest = &rest[i + 1..];
                        self.state = Osc::Esc;
                    }
                    None => return,
                }
                continue;
            }
            let c = rest.chars().next().expect("non-empty");
            rest = &rest[c.len_utf8()..];
            self.state = match (self.state, c) {
                (Osc::Esc, ']') => {
                    self.buf.clear();
                    Osc::Number
                }
                (Osc::Esc, '\x1b') => Osc::Esc,
                (Osc::Esc, _) => Osc::Ground,
                (Osc::Number, d) if d.is_ascii_digit() => {
                    self.buf.push(d);
                    Osc::Number
                }
                (Osc::Number, ';') => {
                    self.title = self.buf == "0" || self.buf == "2";
                    self.buf.clear();
                    Osc::Body
                }
                (Osc::Number, '\x1b') => Osc::Esc,
                (Osc::Number, _) => Osc::Ground,
                (Osc::Body, '\x07') => {
                    self.finish(out);
                    Osc::Ground
                }
                (Osc::Body, '\x1b') => Osc::BodyEsc,
                (Osc::Body, c) => {
                    if self.title && self.buf.len() < MAX_TITLE {
                        self.buf.push(c);
                    }
                    Osc::Body
                }
                (Osc::BodyEsc, '\\') => {
                    self.finish(out);
                    Osc::Ground
                }
                // Another escape sequence aborts the string (as terminals do).
                (Osc::BodyEsc, ']') => {
                    self.buf.clear();
                    Osc::Number
                }
                (Osc::BodyEsc, _) => {
                    self.buf.clear();
                    Osc::Ground
                }
                (Osc::Ground, _) => unreachable!("handled above"),
            };
        }
    }

    fn finish(&mut self, out: &mut Vec<String>) {
        let body = std::mem::take(&mut self.buf);
        if std::mem::take(&mut self.title) {
            out.push(body);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(t: f64) -> Stamp {
        Stamp { t, ts: 100.0 + t }
    }

    /// Runs the scanner over `events` and renders the pieces as a readable trace.
    fn trace(events: &[(f64, &str)]) -> Vec<String> {
        let mut s = Scanner::default();
        let mut out = Vec::new();
        let mut f = |p: Piece<'_>| {
            out.push(match p {
                Piece::Text { at, text } => format!("{}:{text}", at.t),
                Piece::Banner { at, kind, wall } => format!("{}:{kind:?}@{wall}", at.t),
            })
        };
        for (t, d) in events {
            s.feed(at(*t), d, &mut f);
        }
        s.flush(&mut f);
        out
    }

    const W: i64 = 1_700_000_000; // 2023-11-14 22:13:20 as a naive clock

    #[test]
    fn wall_clock_parsing() {
        assert_eq!(parse_wall("2023-11-14 22:13:20"), Some(W));
        assert_eq!(parse_wall("2023-13-14 22:13:20"), None);
        assert_eq!(parse_wall("2023-02-30 22:13:20"), None);
    }

    #[test]
    fn banners_in_one_event() {
        let data = "$ ls\r\n      Begin [2023-11-14 22:13:20]\r\na b\r\n        End   [2023-11-14 22:13:21]\r\n$ ";
        assert_eq!(
            trace(&[(1.0, data)]),
            [
                "1:$ ls\r\n      ",
                &format!("1:Begin@{W}"),
                "1:Begin [2023-11-14 22:13:20]\r\na b\r\n        ",
                &format!("1:End@{}", W + 1),
                "1:End   [2023-11-14 22:13:21]\r\n$ ",
            ]
        );
    }

    #[test]
    fn banner_split_across_events_is_carried_over() {
        let tr = trace(&[
            (1.0, "x\r\n   Beg"),
            (2.0, "in [2023-11-14 22:1"),
            (3.0, "3:20]\r\nout"),
        ]);
        assert_eq!(
            tr,
            [
                "1:x\r\n   ",
                &format!("1:Begin@{W}"),
                "1:Begin [2023-11-14 22:1",
                "3:3:20]\r\nout",
            ]
        );
    }

    #[test]
    fn retimed_carry_is_reported_at_the_new_time() {
        let mut s = Scanner::default();
        let mut out = Vec::new();
        let mut f = |p: Piece<'_>| {
            out.push(match p {
                Piece::Text { at, text } => format!("{}:{text}", at.t),
                Piece::Banner { at, kind, .. } => format!("{}:{kind:?}", at.t),
            })
        };
        s.feed(at(1.0), "x Begin [2023-11-14 2", &mut f);
        s.retime(at(2.0));
        s.feed(at(3.0), "2:13:20]\r\n", &mut f);
        assert_eq!(out, ["1:x ", "2:Begin", "2:Begin [2023-11-14 2", "3:2:13:20]\r\n"]);
        // Without a carry there is nothing to move.
        let mut s = Scanner::default();
        s.retime(at(5.0));
        assert_eq!(s, Scanner::default());
    }

    #[test]
    fn non_banner_tails_are_not_held_back() {
        assert_eq!(trace(&[(1.0, "Bob"), (2.0, "End")]), ["1:Bob", "2:End"]);
        // A held-back prefix that turns out not to be a banner is released with its time.
        assert_eq!(trace(&[(1.0, "a Begin ["), (2.0, "x")]), ["1:a ", "1:Begin [", "2:x"]);
        assert_eq!(trace(&[(1.0, "Begin [2023")]), ["1:Begin [2023"], "flushed at the end");
        // Invalid dates are text.
        assert_eq!(
            trace(&[(1.0, "Begin [2023-19-14 22:13:20]")]),
            ["1:Begin [2023-19-14 22:13:20]"]
        );
    }

    #[test]
    fn scanner_state_roundtrips() {
        let mut s = Scanner::default();
        s.feed(at(0.5), "x Begin [2023-11", &mut |_| {});
        let back: Scanner = serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
        assert_eq!(back, s);
    }

    #[test]
    fn banner_lines_are_stripped_from_chunks() {
        let any = |_| true;
        let text = "          Begin [2023-11-14 22:13:20]\na\nb\n            End   [2023-11-14 22:13:21]";
        assert_eq!(strip_banner_lines(text, any).as_deref(), Some("a\nb"));
        assert_eq!(
            strip_banner_lines(text, |w| w == W).as_deref(),
            Some("a\nb\n            End   [2023-11-14 22:13:21]")
        );
        assert_eq!(strip_banner_lines("   Begin [2023-11-14 22:13:20]", any), None);
        assert_eq!(
            strip_banner_lines("echo Begin [2023-11-14 22:13:20]", any).as_deref(),
            Some("echo Begin [2023-11-14 22:13:20]")
        );
    }

    #[test]
    fn titles() {
        let mut t = TitleScanner::default();
        let mut out = Vec::new();
        t.feed("a\x1b]0;u@h: ~ ls -la\x07b\x1b]7;file://h/x\x07", &mut out);
        t.feed("\x1b]2;split ", &mut out);
        t.feed("title\x1b", &mut out);
        t.feed("\\\x1b[1m\x1b]0;aborted\x1b[0m", &mut out);
        t.feed("\x1b]0;multi\nline\x07", &mut out);
        assert_eq!(out, ["u@h: ~ ls -la", "split title", "multi\nline"]);
    }
}
