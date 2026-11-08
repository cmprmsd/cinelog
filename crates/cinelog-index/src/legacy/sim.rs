//! Similarity of a commands.log entry to the text seen around a `Begin` banner (§11 step 5).
//!
//! Both sides are normalised first: whitespace collapsed, newlines equal to `;` (the old
//! hook wrote newlines as `;`, and zsh's `$3` re-renders multi-line commands), backslashes
//! dropped (zsh's `echo`/`print` interpreted them on the log and title side only).

/// Highest score of an inexact match.
const FUZZY_MAX: f64 = 0.94;

/// Normalised form used for every comparison.
pub(crate) fn normalize(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        let c = match c {
            '\n' | '\r' => ';',
            '\\' => continue,
            c if c.is_whitespace() => ' ',
            c if c.is_control() => continue,
            c => c,
        };
        match c {
            ' ' if out.is_empty() || out.ends_with([' ', ';']) => {}
            ';' => {
                while out.ends_with(' ') {
                    out.pop();
                }
                if !out.ends_with(';') {
                    out.push(';');
                }
            }
            c => out.push(c),
        }
    }
    out.trim_matches([' ', ';']).to_owned()
}

/// Score of a log entry `cmd` against `context` (a screen line or a window title), both
/// already [`normalize`]d: 1.0 when the context ends with the command at a token
/// boundary, slightly less for a bounded occurrence followed by more text (a right
/// prompt), else the best fuzzy match of a context window of the command's length.
pub(crate) fn score(context: &str, cmd: &str) -> f64 {
    if cmd.is_empty() || context.is_empty() {
        return 0.0;
    }
    let c: Vec<char> = context.chars().collect();
    let k: Vec<char> = cmd.chars().collect();
    if let Some(s) = exact(&c, &k) {
        return s.max(alias(context, cmd));
    }
    // Edits of very short commands say nothing (`pls` is not `ls`).
    if k.len() < 4 || !plausible(context, cmd) {
        return alias(context, cmd);
    }
    // Fuzzy matches stay below every exact form.
    fuzzy(&c, &k).min(FUZZY_MAX).max(alias(context, cmd))
}

/// [`score`] without the fuzzy comparisons (for long multi-line contexts).
pub(crate) fn exact_score(context: &str, cmd: &str) -> f64 {
    if cmd.is_empty() || context.is_empty() {
        return 0.0;
    }
    let c: Vec<char> = context.chars().collect();
    let k: Vec<char> = cmd.chars().collect();
    exact(&c, &k).unwrap_or(0.0)
}

/// Score of a truncated entry (`\c` cut its tail): any occurrence that starts at a token
/// boundary, whatever follows.
pub(crate) fn prefix_score(context: &str, cmd: &str) -> f64 {
    let k = cmd.chars().count();
    if k == 0 {
        return 0.0;
    }
    let c: Vec<char> = context.chars().collect();
    let found = context
        .match_indices(cmd)
        .any(|(i, _)| left_bounded(&c, context[..i].chars().count()));
    match (found, k >= 4) {
        (true, true) => 0.9,
        (true, false) => 0.6,
        (false, _) => 0.0,
    }
}

/// Exact occurrences of `k` in `c` that start at a token boundary.
fn exact(c: &[char], k: &[char]) -> Option<f64> {
    if k.len() > c.len() {
        return None;
    }
    let mut best = None::<f64>;
    for start in (0..=c.len() - k.len()).rev() {
        if c[start..start + k.len()] != *k || !left_bounded(c, start) {
            continue;
        }
        let end = start + k.len();
        let s = if end == c.len() {
            1.0
        } else if is_sep(c[end]) {
            // Followed by more text: a right prompt, or a longer command that merely
            // contains this one. Short commands are cheap to contain by accident.
            if k.len() < 4 { 0.85 } else { 0.95 }
        } else {
            0.8
        };
        best = Some(best.map_or(s, |b| b.max(s)));
        if s >= 1.0 {
            break;
        }
    }
    best
}

fn is_sep(c: char) -> bool {
    c == ' ' || c == ';'
}

/// The occurrence starts the context, or follows a separator or prompt punctuation.
fn left_bounded(c: &[char], start: usize) -> bool {
    start == 0 || {
        let p = c[start - 1];
        is_sep(p) || !(p.is_alphanumeric() || p == '_' || p == '-' || p == '.' || p == '/')
    }
}

/// Cheap prefilter before the quadratic comparisons: some token of the command (or, for
/// one-token commands, the command itself) must occur in the context.
fn plausible(context: &str, cmd: &str) -> bool {
    cmd.split([' ', ';'])
        .filter(|t| t.chars().count() >= 2)
        .any(|t| context.contains(t))
        || cmd.chars().count() < 2
}

/// Best normalised Levenshtein similarity between `k` and a window of `c` of about the
/// same length that starts at a token boundary near the end of `c` (the command ends the
/// context, at most a right prompt follows).
fn fuzzy(c: &[char], k: &[char]) -> f64 {
    const MAX_CHARS: usize = 256;
    const RIGHT_SLACK: usize = 64;
    let k = &k[..k.len().min(MAX_CHARS)];
    let mut best: f64 = 0.0;
    let first = c.len().saturating_sub(k.len() + k.len() / 8 + 1 + RIGHT_SLACK);
    for start in (first..c.len()).filter(|&i| left_bounded(c, i)) {
        for len in [k.len(), k.len() + k.len() / 8 + 1] {
            let end = (start + len).min(c.len());
            if end <= start {
                continue;
            }
            best = best.max(levenshtein_sim(&c[start..end], k));
        }
    }
    best
}

fn levenshtein_sim(a: &[char], b: &[char]) -> f64 {
    let n = a.len().max(b.len());
    if n == 0 {
        return 1.0;
    }
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0; b.len() + 1];
    for (i, ca) in a.iter().enumerate() {
        cur[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let sub = prev[j] + usize::from(ca != cb);
            cur[j + 1] = sub.min(prev[j + 1] + 1).min(cur[j] + 1);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    1.0 - prev[b.len()] as f64 / n as f64
}

/// Alias expansion tolerance (`$3` is alias-expanded, the screen shows what was typed): the
/// command's trailing tokens equal the context's, and the remaining head of the command
/// stands in for exactly one context token (`ll /tmp` vs `ls -l /tmp`).
fn alias(context: &str, cmd: &str) -> f64 {
    let ctx: Vec<&str> = context.split(' ').collect();
    let cmdt: Vec<&str> = cmd.split(' ').collect();
    let common = ctx
        .iter()
        .rev()
        .zip(cmdt.iter().rev())
        .take_while(|(a, b)| a == b)
        .count();
    if common == 0 || common >= cmdt.len() || common >= ctx.len() {
        return 0.0;
    }
    let alias_word = ctx[ctx.len() - common - 1];
    let head = &cmdt[..cmdt.len() - common];
    let rest_chars: usize = cmdt[cmdt.len() - common..].iter().map(|t| t.chars().count()).sum();
    if alias_word.is_empty() || head.contains(&alias_word) || rest_chars < 2 {
        return 0.0;
    }
    if common >= 2 { 0.9 } else { 0.8 }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(ctx: &str, cmd: &str) -> f64 {
        score(&normalize(ctx), &normalize(cmd))
    }

    #[test]
    fn normalisation() {
        assert_eq!(
            normalize("  for i in 1 2; do\n\techo $i\ndone  "),
            "for i in 1 2;do;echo $i;done"
        );
        assert_eq!(
            normalize("for i in 1 2;do;\techo $i;done"),
            "for i in 1 2;do;echo $i;done"
        );
        assert_eq!(normalize("a  \\\\b\r\n"), "a b");
        assert_eq!(normalize(";; x ;; "), "x");
    }

    #[test]
    fn exact_suffix_and_right_prompt() {
        assert_eq!(s("user@host:~/src$ git status", "git status"), 1.0);
        assert_eq!(s("❯ git status          ✔  10:32:01", "git status"), 0.95);
        assert_eq!(s("$ ls -la", "ls"), 0.85);
        assert_eq!(s("$ pls", "ls"), 0.0);
        assert!(s("$ cat /var/log/syslog", "syslog") < 0.5);
        assert_eq!(s("❯ftp host", "ftp host"), 1.0);
    }

    #[test]
    fn fuzzy_and_alias() {
        // `$3` re-renders a one-line loop as several lines.
        let f = s("$ for i in 1 2; do echo $i; done", "for i in 1 2;do;\techo $i;done");
        assert!(f > 0.9 && f < 1.0, "{f}");
        assert_eq!(s("~ ❯ ll /tmp", "ls -l /tmp"), 0.8);
        assert_eq!(s("~ ❯ gco feature/x main", "git checkout feature/x main"), 0.9);
        assert!(s("$ vim notes.txt", "htop") < 0.5);
        assert_eq!(s("", "ls"), 0.0);
    }

    #[test]
    fn exact_and_prefix_only() {
        assert_eq!(exact_score("$ git status", "git status"), 1.0);
        assert_eq!(
            exact_score("$ for i in 1 2; do echo $i; done", "for i in 1 2;do;echo $i;done"),
            0.0
        );
        assert_eq!(prefix_score("$ printf 'acb'", "printf 'a"), 0.9);
        assert_eq!(prefix_score("$ xprintf 'acb'", "printf 'a"), 0.0);
        assert_eq!(prefix_score("$ lsblk", "ls"), 0.6);
        assert_eq!(prefix_score("", "ls"), 0.0);
    }

    #[test]
    fn fuzzy_looks_at_the_end_only() {
        let long = format!("$ git sttaus {}", "x ".repeat(100));
        assert!(s(&long, "git status") < 0.5);
        assert!(s("$ git sttaus", "git status") > 0.7);
    }

    #[test]
    fn levenshtein() {
        let c = |x: &str| x.chars().collect::<Vec<_>>();
        assert_eq!(levenshtein_sim(&c("kitten"), &c("sitting")), 1.0 - 3.0 / 7.0);
        assert_eq!(levenshtein_sim(&c(""), &c("")), 1.0);
    }
}
