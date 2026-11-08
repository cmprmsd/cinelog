//! Output-side observers: the alternate-screen tracker (§5.3 resume fix-up) and the
//! prompt-heuristic tail (§5.5 rule b). Both are streaming and split-safe.

use std::sync::LazyLock;

use regex::bytes::Regex;

/// Tracks `CSI ? Pm h|l` for the private modes 47, 1047 and 1049 across arbitrary splits.
#[derive(Debug, Default, Clone)]
pub(crate) struct AltScreen {
    active: bool,
    state: CsiState,
    params: Vec<u8>,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
enum CsiState {
    #[default]
    Ground,
    Esc,
    /// `ESC [` seen; waiting for `?`.
    Csi,
    /// `ESC [ ?` seen; collecting parameters.
    Private,
}

/// Parameters longer than this are not alt-screen switches; the sequence is ignored.
const MAX_PARAMS: usize = 32;

impl AltScreen {
    pub(crate) fn active(&self) -> bool {
        self.active
    }

    pub(crate) fn feed(&mut self, bytes: &[u8]) {
        let mut rest = bytes;
        while !rest.is_empty() {
            if self.state == CsiState::Ground {
                match memchr::memchr(0x1b, rest) {
                    Some(i) => {
                        self.state = CsiState::Esc;
                        rest = &rest[i + 1..];
                    }
                    None => return,
                }
                continue;
            }
            let b = rest[0];
            rest = &rest[1..];
            self.state = match (self.state, b) {
                (_, 0x1b) => CsiState::Esc,
                (CsiState::Esc, b'[') => CsiState::Csi,
                (CsiState::Csi, b'?') => {
                    self.params.clear();
                    CsiState::Private
                }
                (CsiState::Private, b'0'..=b'9' | b';') if self.params.len() < MAX_PARAMS => {
                    self.params.push(b);
                    CsiState::Private
                }
                (CsiState::Private, b'h' | b'l') => {
                    let set = b == b'h';
                    if self
                        .params
                        .split(|&c| c == b';')
                        .any(|p| matches!(p, b"47" | b"1047" | b"1049"))
                    {
                        self.active = set;
                    }
                    CsiState::Ground
                }
                _ => CsiState::Ground,
            };
        }
    }
}

/// Bytes of recent output kept for the prompt heuristic.
const TAIL_CAP: usize = 256;

/// `(?i)(password|passphrase|passcode|pin|token)[^\n]{0,40}:\s*$` from §5.5, with a word
/// boundary in front (so `Spinning up:` does not match) and the German `Passwort`/`Kennwort`
/// that localized sudo and PAM print.
static PROMPT_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(?:password|passwort|kennwort|passphrase|passcode|pin|token)[^\n]{0,40}:\s*$")
        .expect("valid prompt regex")
});

/// Last [`TAIL_CAP`] bytes of terminal output, tested against [`PROMPT_RE`].
#[derive(Debug, Default, Clone)]
pub(crate) struct PromptTail {
    tail: Vec<u8>,
}

impl PromptTail {
    pub(crate) fn push(&mut self, bytes: &[u8]) {
        if bytes.len() >= TAIL_CAP {
            self.tail.clear();
            self.tail.extend_from_slice(&bytes[bytes.len() - TAIL_CAP..]);
        } else {
            let excess = (self.tail.len() + bytes.len()).saturating_sub(TAIL_CAP);
            self.tail.drain(..excess);
            self.tail.extend_from_slice(bytes);
        }
    }

    /// Whether the last output line looks like a secret prompt.
    pub(crate) fn looks_like_secret_prompt(&self) -> bool {
        let line = last_line(&self.tail);
        !line.is_empty() && PROMPT_RE.is_match(&strip_escapes(line))
    }
}

/// Text after the last line break. A bare CR also starts a new line (the line is redrawn).
fn last_line(tail: &[u8]) -> &[u8] {
    match memchr::memrchr2(b'\n', b'\r', tail) {
        Some(i) => &tail[i + 1..],
        None => tail,
    }
}

/// Removes CSI, OSC and two-byte escape sequences so colours and cursor moves around a
/// prompt do not hide it from the regex.
fn strip_escapes(s: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(s.len());
    let mut i = 0;
    while i < s.len() {
        if s[i] != 0x1b {
            if s[i] >= 0x20 || s[i] == b'\t' {
                out.push(s[i]);
            }
            i += 1;
            continue;
        }
        i += 1;
        match s.get(i) {
            Some(b'[') => {
                i += 1;
                while i < s.len() && !(0x40..=0x7e).contains(&s[i]) {
                    i += 1;
                }
                i += 1;
            }
            Some(b']') => {
                i += 1;
                while i < s.len() && s[i] != 0x07 && !(s[i] == 0x1b && s.get(i + 1) == Some(&b'\\')) {
                    i += 1;
                }
                i += if s.get(i) == Some(&0x07) { 1 } else { 2 };
            }
            Some(_) => i += 1,
            None => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alt_screen_any_split() {
        let input = b"a\x1b[?1049hvim\x1b[?25l\x1b[?1;1049l done \x1b[?47h";
        let expect_after = [true, false, true];
        for split in 1..input.len() {
            let mut t = AltScreen::default();
            for c in input.chunks(split) {
                t.feed(c);
            }
            assert!(t.active(), "split {split}");
        }
        let mut t = AltScreen::default();
        for (seq, want) in [&b"\x1b[?1049h"[..], b"\x1b[?1049l", b"\x1b[?1047h"]
            .iter()
            .zip(expect_after)
        {
            t.feed(seq);
            assert_eq!(t.active(), want);
        }
    }

    #[test]
    fn alt_screen_ignores_other_modes_and_garbage() {
        let mut t = AltScreen::default();
        t.feed(b"\x1b[?25h\x1b[1049h\x1b[?10490h\x1b[?x1049h");
        assert!(!t.active());
        t.feed(b"\x1b[?");
        t.feed(b"1049");
        t.feed(b"h");
        assert!(t.active());
    }

    fn prompt(s: &[u8]) -> bool {
        let mut p = PromptTail::default();
        p.push(s);
        p.looks_like_secret_prompt()
    }

    #[test]
    fn prompt_heuristic_matches_secret_prompts() {
        for s in [
            &b"[sudo] password for alice: "[..],
            b"alice@remote's password:",
            b"Enter passphrase for key '/home/alice/.ssh/id_ed25519': ",
            b"Enter PIN for 'PIV Card Holder pin': ",
            b"\x1b[1mPassword:\x1b[0m ",
            b"[sudo] Passwort f\xc3\xbcr alice: ",
            b"some output\r\nAPI token: ",
            b"Login incorrect\nPassword: ",
        ] {
            assert!(prompt(s), "{}", String::from_utf8_lossy(s));
        }
    }

    #[test]
    fn prompt_heuristic_ignores_normal_output() {
        for s in [
            &b"Spinning up: "[..],
            b"Username for 'https://github.com': ",
            b"password: ok\n",
            b"alice@box ~ % ",
            b"Password: hunter2 was rejected, see logs for details please",
            b"",
        ] {
            assert!(!prompt(s), "{}", String::from_utf8_lossy(s));
        }
    }

    #[test]
    fn prompt_tail_is_bounded_and_split_safe() {
        let mut p = PromptTail::default();
        p.push(&[b'x'; 1000]);
        p.push(b"\nPass");
        p.push(b"word: ");
        assert!(p.looks_like_secret_prompt());
        assert!(p.tail.len() <= TAIL_CAP);
        p.push(b"\n");
        assert!(!p.looks_like_secret_prompt());
    }
}
