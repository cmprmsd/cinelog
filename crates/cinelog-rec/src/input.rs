//! Keystroke (`i`) policy, plan §5.5.
//!
//! Every chunk read from the host terminal is forwarded to the child unchanged; this module
//! only decides which part of it is *recorded*.

use std::borrow::Cow;

use cinelog_core::config::{InputSuppress, RecordConfig};

/// The rules of §5.5 as configured.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct InputRules {
    /// (f) `record.input`.
    pub enabled: bool,
    /// (a) `!ECHO && ICANON`.
    pub password_prompt: bool,
    /// (b) last output line looks like a secret prompt.
    pub prompt_heuristic: bool,
    /// (c) nothing while paused.
    pub paused: bool,
    /// (d) a relay in the foreground pgrp (opt-in).
    pub relay: bool,
    /// (e) terminal replies are not keystrokes.
    pub terminal_responses: bool,
}

impl InputRules {
    pub(crate) fn from_config(cfg: &RecordConfig) -> Self {
        Self {
            enabled: cfg.input,
            password_prompt: cfg.suppresses(InputSuppress::PasswordPrompt),
            prompt_heuristic: cfg.suppresses(InputSuppress::PromptHeuristic),
            paused: cfg.suppresses(InputSuppress::Paused),
            relay: cfg.suppresses(InputSuppress::Relay),
            terminal_responses: cfg.suppresses(InputSuppress::TerminalResponses),
        }
    }
}

/// Slave line discipline flags at the time the chunk was read (`tcgetattr(master)`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TermFlags {
    pub echo: bool,
    pub icanon: bool,
}

impl TermFlags {
    /// Used when the flags cannot be read: assume an echoing canonical line.
    pub(crate) const UNKNOWN: Self = Self {
        echo: true,
        icanon: true,
    };

    pub(crate) fn password_like(self) -> bool {
        !self.echo && self.icanon
    }
}

/// Situation of one input chunk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct InputContext {
    pub flags: TermFlags,
    pub paused: bool,
    pub relay_in_foreground: bool,
}

/// Prompt-heuristic latch: armed by output that looks like a secret prompt, released by the
/// first CR or LF typed afterwards.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PromptGate {
    armed: bool,
}

impl PromptGate {
    pub(crate) fn arm(&mut self) {
        self.armed = true;
    }

    #[cfg(test)]
    pub(crate) fn armed(&self) -> bool {
        self.armed
    }

    /// Splits off the suppressed head of `data` (through the first CR/LF) while armed.
    fn pass<'a>(&mut self, data: &'a [u8]) -> &'a [u8] {
        if !self.armed {
            return data;
        }
        match memchr::memchr2(b'\r', b'\n', data) {
            Some(i) => {
                self.armed = false;
                &data[i + 1..]
            }
            None => &[],
        }
    }
}

/// Applies the rules to one chunk and returns the bytes to record (possibly empty).
pub(crate) fn recordable<'a>(
    rules: &InputRules,
    ctx: InputContext,
    gate: &mut PromptGate,
    data: &'a [u8],
) -> Cow<'a, [u8]> {
    let suppressed = !rules.enabled
        || (rules.paused && ctx.paused)
        || (rules.password_prompt && ctx.flags.password_like())
        || (rules.relay && ctx.relay_in_foreground);
    if suppressed {
        // A line typed now still ends the prompt the gate was armed for.
        if rules.prompt_heuristic {
            gate.pass(data);
        }
        return Cow::Borrowed(&[]);
    }
    let data = if rules.prompt_heuristic { gate.pass(data) } else { data };
    if rules.terminal_responses {
        strip_terminal_responses(data)
    } else {
        Cow::Borrowed(data)
    }
}

/// Removes terminal replies from an input chunk (rule e): device attributes (`CSI ? … c`,
/// `CSI > … c`), cursor and status reports (`CSI … R`, `CSI … n`), mode reports
/// (`CSI ? … $ y`), window reports (`CSI … t`), kitty keyboard flags (`CSI ? … u`), focus
/// events (`CSI I`, `CSI O`), and every complete OSC, DCS (DECRQSS, XTVERSION) and APC
/// string. Keys, mouse reports and incomplete sequences are kept.
pub(crate) fn strip_terminal_responses(data: &[u8]) -> Cow<'_, [u8]> {
    if memchr::memchr(0x1b, data).is_none() {
        return Cow::Borrowed(data);
    }
    let mut out = Vec::with_capacity(data.len());
    let mut i = 0;
    while i < data.len() {
        let Some(off) = memchr::memchr(0x1b, &data[i..]) else {
            out.extend_from_slice(&data[i..]);
            break;
        };
        out.extend_from_slice(&data[i..i + off]);
        i += off;
        match response_len(&data[i..]) {
            Some(n) => i += n,
            None => {
                out.push(0x1b);
                i += 1;
            }
        }
    }
    Cow::Owned(out)
}

/// Length of the terminal reply at the start of `s` (which starts with ESC), if it is one.
fn response_len(s: &[u8]) -> Option<usize> {
    match *s.get(1)? {
        b'[' => csi_response_len(s),
        b']' | b'P' | b'_' => string_len(s),
        _ => None,
    }
}

fn csi_response_len(s: &[u8]) -> Option<usize> {
    let body = &s[2..];
    let params_end = body.iter().position(|b| !(0x30..=0x3f).contains(b))?;
    let inter_end = params_end + body[params_end..].iter().position(|b| !(0x20..=0x2f).contains(b))?;
    let fin = *body.get(inter_end)?;
    if !(0x40..=0x7e).contains(&fin) {
        return None;
    }
    let params = &body[..params_end];
    let inter = &body[params_end..inter_end];
    let prefix = params
        .first()
        .copied()
        .filter(|b| matches!(b, b'?' | b'>' | b'=' | b'<'));
    let numeric = |p: &[u8]| !p.is_empty() && p.iter().all(|b| b.is_ascii_digit() || *b == b';');
    let rest = if prefix.is_some() { &params[1..] } else { params };
    let reply = match (fin, inter) {
        (b'c', b"") => matches!(prefix, Some(b'?' | b'>' | b'=')),
        (b'R', b"") => matches!(prefix, None | Some(b'?')) && numeric(rest) && rest.contains(&b';'),
        (b'n', b"") => numeric(rest),
        (b'y', b"$") => numeric(rest),
        (b't', b"") => prefix.is_none() && numeric(rest),
        (b'u', b"") => prefix == Some(b'?'),
        (b'I' | b'O', b"") => params.is_empty(),
        _ => false,
    };
    reply.then_some(2 + inter_end + 1)
}

/// OSC/DCS/APC string terminated by BEL or ST; `None` if unterminated in this chunk.
fn string_len(s: &[u8]) -> Option<usize> {
    let mut i = 2;
    while i < s.len() {
        match s[i] {
            0x07 => return Some(i + 1),
            0x1b if s.get(i + 1) == Some(&b'\\') => return Some(i + 2),
            0x18 | 0x1a => return None,
            _ => i += 1,
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    const RULES: InputRules = InputRules {
        enabled: true,
        password_prompt: true,
        prompt_heuristic: true,
        paused: true,
        relay: false,
        terminal_responses: true,
    };
    const ZLE: TermFlags = TermFlags {
        echo: false,
        icanon: false,
    };
    const COOKED: TermFlags = TermFlags {
        echo: true,
        icanon: true,
    };
    const GETPASS: TermFlags = TermFlags {
        echo: false,
        icanon: true,
    };

    fn ctx(flags: TermFlags, paused: bool, relay: bool) -> InputContext {
        InputContext {
            flags,
            paused,
            relay_in_foreground: relay,
        }
    }

    fn rec(rules: InputRules, c: InputContext, gate: &mut PromptGate, data: &[u8]) -> Vec<u8> {
        recordable(&rules, c, gate, data).into_owned()
    }

    #[test]
    fn truth_table() {
        let relay_on = InputRules { relay: true, ..RULES };
        let off = InputRules {
            enabled: false,
            ..RULES
        };
        // (rules, flags, paused, relay, recorded?)
        let table = [
            (RULES, ZLE, false, false, true),      // zle prompt: recorded
            (RULES, COOKED, false, false, true),   // cat, read: recorded
            (RULES, GETPASS, false, false, false), // (a) read -s, getpass, sudo
            (RULES, ZLE, true, false, false),      // (c) paused
            (RULES, ZLE, false, true, true),       // relay rule off by default
            (relay_on, ZLE, false, true, false),   // (d) opt-in
            (relay_on, ZLE, false, false, true),
            (off, ZLE, false, false, false), // (f)
            (
                InputRules {
                    password_prompt: false,
                    ..RULES
                },
                GETPASS,
                false,
                false,
                true,
            ),
        ];
        for (i, (rules, flags, paused, relay, want)) in table.into_iter().enumerate() {
            let got = rec(rules, ctx(flags, paused, relay), &mut PromptGate::default(), b"ls\r");
            assert_eq!(!got.is_empty(), want, "row {i}");
        }
    }

    #[test]
    fn prompt_gate_suppresses_through_the_first_line_end() {
        let mut gate = PromptGate::default();
        gate.arm();
        let c = ctx(ZLE, false, false);
        assert_eq!(rec(RULES, c, &mut gate, b"hunt"), b"");
        assert_eq!(rec(RULES, c, &mut gate, b"er2\rls"), b"ls");
        assert!(!gate.armed());
        assert_eq!(rec(RULES, c, &mut gate, b"x"), b"x");
        // A suppressed chunk (getpass) still releases the gate at its line end.
        gate.arm();
        assert_eq!(rec(RULES, ctx(GETPASS, false, false), &mut gate, b"pw\n"), b"");
        assert!(!gate.armed());
        // Disabled heuristic ignores the gate.
        gate.arm();
        let no_heur = InputRules {
            prompt_heuristic: false,
            ..RULES
        };
        assert_eq!(rec(no_heur, c, &mut gate, b"pw"), b"pw");
    }

    #[test]
    fn terminal_responses_are_filtered() {
        let cases: [(&[u8], &[u8]); 14] = [
            (b"\x1b[?62;22c", b""),                       // DA1
            (b"\x1b[>41;380;0c", b""),                    // DA2
            (b"\x1b[12;40R", b""),                        // CPR
            (b"\x1b[?12;40;1R", b""),                     // DECXCPR
            (b"\x1b[0n", b""),                            // DSR
            (b"\x1b[?2004;2$y", b""),                     // DECRPM
            (b"\x1b[8;24;80t", b""),                      // window size report
            (b"\x1b[?1u", b""),                           // kitty keyboard flags
            (b"a\x1b[Ib\x1b[Oc", b"abc"),                 // focus
            (b"\x1b]11;rgb:0000/0000/0000\x1b\\x", b"x"), // OSC 11 reply
            (b"\x1b]4;1;rgb:cd/00/00\x07", b""),
            (b"\x1bP1$r0;1m\x1b\\", b""),       // DECRQSS
            (b"\x1bP>|kitty(0.36)\x1b\\", b""), // XTVERSION
            (b"\x1b_Gi=1;OK\x1b\\", b""),       // kitty graphics
        ];
        for (input, want) in cases {
            assert_eq!(&*strip_terminal_responses(input), want, "{input:?}");
        }
    }

    #[test]
    fn keys_are_kept() {
        for input in [
            &b"\x1b[A\x1b[B\x1b[1;5C"[..], // arrows, ctrl-right
            b"\x1b[3~\x1b[15~",            // delete, F5
            b"\x1b[Z",                     // shift-tab
            b"\x1bOP",                     // F1 (SS3)
            b"\x1b",                       // lone ESC (vi mode)
            b"\x1b[",                      // alt-[
            b"\x1b]",                      // alt-]
            b"\x1b[<0;10;5M\x1b[<0;10;5m", // SGR mouse
            b"\x1b[200~pasted\x1b[201~",   // bracketed paste
            b"\x1b]unterminated",
            "üñí".as_bytes(),
        ] {
            assert_eq!(&*strip_terminal_responses(input), input, "{input:?}");
        }
    }

    #[test]
    fn responses_mixed_with_typing() {
        let got = rec(
            RULES,
            ctx(ZLE, false, false),
            &mut PromptGate::default(),
            b"l\x1b[?62cs\r",
        );
        assert_eq!(got, b"ls\r");
        let unfiltered = InputRules {
            terminal_responses: false,
            ..RULES
        };
        let got = rec(
            unfiltered,
            ctx(ZLE, false, false),
            &mut PromptGate::default(),
            b"\x1b[I",
        );
        assert_eq!(got, b"\x1b[I");
    }
}
