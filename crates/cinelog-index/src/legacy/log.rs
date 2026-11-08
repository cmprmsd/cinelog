//! Tolerant parser for the old plugin's `<cast>.commands.log` (plan §11 step 4).
//!
//! The old preexec hook wrote `echo "[<t>, \"m\", \"<cmd>\"]"` with newlines in `<cmd>`
//! replaced by `;` and `"` escaped as `\"`. zsh's `echo` then interpreted backslash escapes
//! in the command itself, which produced two kinds of damage this parser repairs:
//! - `\c` stops the output, so the entry loses its tail and the next entry is appended to
//!   the same physical line (`[1, "m", "foo[2, "m", "bar"]`): split at entry starts;
//! - `\n` emits a real newline, so one entry spans several physical lines: join them.
//!
//! Only the order of the entries is used; their times are garbage [R3].

use std::sync::LazyLock;

use regex::Regex;

/// Physical lines joined into one entry at most (a command with embedded `\n` escapes).
const MAX_JOINED_LINES: usize = 16;

/// Start of an entry, anywhere in a line.
static ENTRY_START: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"\[(-?[\d.]+), "m", ""#).expect("valid regex"));

/// One command of the log, in log order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LogEntry {
    /// The command text (`\"` unescaped; old newlines are `;`).
    pub text: String,
    /// Parsed through the entry pattern; raw lines are kept with a low prior.
    pub parsed: bool,
    /// The entry lost its tail (`\c` in the command): the text is a prefix.
    pub truncated: bool,
}

/// Parses the whole log. Never fails: unparsable lines become raw entries.
pub(crate) fn parse(log: &str) -> Vec<LogEntry> {
    let mut fragments = Vec::new();
    for line in log.split('\n') {
        let line = line.strip_suffix('\r').unwrap_or(line);
        split_merged(line, &mut fragments);
    }
    let mut out = Vec::new();
    let mut i = 0;
    while i < fragments.len() {
        let frag = fragments[i];
        i += 1;
        let Some(body) = entry_body(frag) else {
            if !frag.trim().is_empty() {
                out.push(LogEntry {
                    text: frag.trim().to_owned(),
                    parsed: false,
                    truncated: false,
                });
            }
            continue;
        };
        if let Some(text) = body.strip_suffix("\"]") {
            out.push(entry(text, false));
            continue;
        }
        // An entry without its closing `"]`: either an embedded newline (the rest follows on
        // the next lines, which do not start a new entry) or a `\c` truncation.
        let mut joined = body.to_owned();
        let mut j = i;
        let mut closed = None;
        while j < fragments.len() && j - i < MAX_JOINED_LINES && entry_body(fragments[j]).is_none() {
            joined.push('\n');
            joined.push_str(fragments[j]);
            j += 1;
            if let Some(text) = joined.strip_suffix("\"]") {
                closed = Some(text.to_owned());
                break;
            }
        }
        match closed {
            Some(text) => {
                out.push(entry(&text, false));
                i = j;
            }
            None => out.push(entry(body, true)),
        }
    }
    out
}

/// Splits a physical line at every entry start after its beginning (the `\c` merge).
fn split_merged<'a>(line: &'a str, out: &mut Vec<&'a str>) {
    let mut from = 0;
    for m in ENTRY_START.find_iter(line) {
        if m.start() > from {
            out.push(&line[from..m.start()]);
        }
        from = m.start();
    }
    out.push(&line[from..]);
}

/// The text after `[<t>, "m", "` when `frag` starts with an entry.
fn entry_body(frag: &str) -> Option<&str> {
    ENTRY_START
        .find(frag)
        .filter(|m| m.start() == 0)
        .map(|m| &frag[m.end()..])
}

fn entry(text: &str, truncated: bool) -> LogEntry {
    LogEntry {
        text: text.replace("\\\"", "\""),
        parsed: true,
        truncated,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(v: &[LogEntry]) -> Vec<(&str, bool, bool)> {
        v.iter().map(|e| (e.text.as_str(), e.parsed, e.truncated)).collect()
    }

    #[test]
    fn plain_entries_and_negative_times() {
        let log = "[-0.100000, \"m\", \"ls -la\"]\n[12.5, \"m\", \"echo \\\"hi\\\"\"]\n\n";
        assert_eq!(
            texts(&parse(log)),
            [("ls -la", true, false), ("echo \"hi\"", true, false)]
        );
    }

    #[test]
    fn backslash_c_merge_is_split() {
        let log = "[1.0, \"m\", \"printf 'a[2.0, \"m\", \"pwd\"]\n[3.0, \"m\", \"ls\"]\n";
        assert_eq!(
            texts(&parse(log)),
            [("printf 'a", true, true), ("pwd", true, false), ("ls", true, false)]
        );
    }

    #[test]
    fn embedded_newlines_are_joined() {
        let log = "[1.0, \"m\", \"printf 'a\nb\nc'\"]\n[2.0, \"m\", \"ls\"]\n";
        assert_eq!(
            texts(&parse(log)),
            [("printf 'a\nb\nc'", true, false), ("ls", true, false)]
        );
    }

    #[test]
    fn truncation_at_end_of_file_and_raw_lines() {
        let log = "garbage line\n[1.0, \"m\", \"for i in 1 2;do;\techo $i;done\"]\r\n[2.0, \"m\", \"cut";
        assert_eq!(
            texts(&parse(log)),
            [
                ("garbage line", false, false),
                ("for i in 1 2;do;\techo $i;done", true, false),
                ("cut", true, true)
            ]
        );
    }

    #[test]
    fn unclosed_entry_followed_by_entry_is_truncated_not_joined() {
        let log = "[1.0, \"m\", \"a\\\n[2.0, \"m\", \"b\"]\n";
        assert_eq!(texts(&parse(log)), [("a\\", true, true), ("b", true, false)]);
    }

    #[test]
    fn join_is_bounded() {
        let mut log = String::from("[1.0, \"m\", \"x");
        for _ in 0..40 {
            log.push_str("\nmore");
        }
        let v = parse(&log);
        assert_eq!(v[0].text, "x");
        assert!(v[0].truncated);
        assert_eq!(v.len(), 41);
        assert!(!v[1].parsed);
    }
}
