//! Search terms under the §6.5 query rules: Unicode case folding, the `LIKE`/`GLOB`
//! patterns handed to the trigram index, the Rust post-filter and snippets.
//!
//! SQLite rechecks every trigram candidate with its own `LIKE`, which folds ASCII only, so
//! `LIKE '%ärger%'` never finds `ÄRGER`. Cased non-ASCII characters therefore become `_` in
//! the pattern and the candidates are post-filtered here with the per-character folding of
//! the trigram tokenizer (`case_sensitive=0`).

use crate::model::{MIN_TRIGRAM_CHARS, SNIPPET_CLOSE, SNIPPET_OPEN};

/// Characters of context kept before and after the first match in a snippet.
const SNIPPET_BEFORE: usize = 32;
const SNIPPET_AFTER: usize = 48;

/// Simple 1:1 lowercase folding, like FTS5's `case_sensitive=0`: characters whose lowercase
/// form has several characters stay unchanged, so folded text has the same character count.
pub(crate) fn fold_char(c: char) -> char {
    if c.is_ascii() {
        return c.to_ascii_lowercase();
    }
    let mut l = c.to_lowercase();
    match (l.next(), l.next()) {
        (Some(x), None) => x,
        _ => c,
    }
}

fn fold(s: &str) -> String {
    s.chars().map(fold_char).collect()
}

/// Whether `c` has a case variant SQLite's ASCII-only `LIKE` would miss.
fn is_cased_non_ascii(c: char) -> bool {
    !c.is_ascii() && (fold_char(c) != c || c.to_uppercase().ne(std::iter::once(c)))
}

/// A search term with its SQL pattern.
#[derive(Debug, Clone)]
pub(crate) struct Term {
    case_sensitive: bool,
    /// The term as compared by the post-filter (folded unless case-sensitive).
    needle: String,
    /// `LIKE` pattern (case-insensitive) or `GLOB` pattern (case-sensitive), `%…%`/`*…*`.
    pattern: String,
    /// The pattern can over-match: candidates must pass [`Self::matches`].
    post_filter: bool,
    /// The pattern has a literal run of at least three characters, so the trigram index
    /// can serve it; otherwise FTS5 would scan the whole table.
    indexable: bool,
}

impl Term {
    pub(crate) fn new(term: &str, case_sensitive: bool) -> Self {
        let mut pattern = String::with_capacity(term.len() + 8);
        let (open_close, wildcard) = if case_sensitive { ('*', '?') } else { ('%', '_') };
        pattern.push(open_close);
        let mut post_filter = false;
        let mut indexable = false;
        let mut run = String::new();
        let mut flush = |run: &mut String, pattern: &mut String, post_filter: &mut bool| {
            let chars = run.chars().count();
            if chars >= MIN_TRIGRAM_CHARS {
                indexable = true;
                pattern.push_str(run);
            } else if run.len() >= MIN_TRIGRAM_CHARS {
                // FTS5 measures runs in bytes: a short multi-byte run would become a phrase
                // without trigrams. Keep it out of the index lookup.
                pattern.extend(std::iter::repeat_n(wildcard, chars));
                *post_filter = true;
            } else {
                pattern.push_str(run);
            }
            run.clear();
        };
        for c in term.chars() {
            if case_sensitive {
                match c {
                    '*' | '?' | '[' => {
                        flush(&mut run, &mut pattern, &mut post_filter);
                        pattern.push('[');
                        pattern.push(c);
                        pattern.push(']');
                    }
                    c => run.push(c),
                }
            } else if c == '%' || c == '_' {
                flush(&mut run, &mut pattern, &mut post_filter);
                pattern.push(c);
                post_filter = true;
            } else if is_cased_non_ascii(c) {
                flush(&mut run, &mut pattern, &mut post_filter);
                pattern.push('_');
                post_filter = true;
            } else {
                run.push(c);
            }
        }
        flush(&mut run, &mut pattern, &mut post_filter);
        pattern.push(open_close);
        let needle = if case_sensitive { term.to_owned() } else { fold(term) };
        Self {
            case_sensitive,
            needle,
            pattern,
            post_filter,
            indexable,
        }
    }

    pub(crate) fn pattern(&self) -> &str {
        &self.pattern
    }

    /// SQL operator of [`Self::pattern`].
    pub(crate) fn op(&self) -> &'static str {
        if self.case_sensitive { "GLOB" } else { "LIKE" }
    }

    pub(crate) fn needs_post_filter(&self) -> bool {
        self.post_filter
    }

    pub(crate) fn indexable(&self) -> bool {
        self.indexable
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.needle.is_empty()
    }

    /// Character index of the first match in `hay`.
    fn find(&self, hay: &str) -> Option<usize> {
        if self.case_sensitive {
            hay.find(&self.needle).map(|b| hay[..b].chars().count())
        } else {
            let folded = fold(hay);
            folded.find(&self.needle).map(|b| folded[..b].chars().count())
        }
    }

    /// Literal substring match (wildcard characters in the term are plain characters).
    pub(crate) fn matches(&self, hay: &str) -> bool {
        self.find(hay).is_some()
    }

    /// The line around the first match, the match between [`SNIPPET_OPEN`] and
    /// [`SNIPPET_CLOSE`]. Sentinels already in the text become U+FFFD.
    pub(crate) fn snippet(&self, text: &str) -> Option<String> {
        let start = self.find(text)?;
        let chars: Vec<char> = text.chars().collect();
        let end = (start + self.needle.chars().count()).min(chars.len());
        let line_start = chars[..start].iter().rposition(|&c| c == '\n').map_or(0, |i| i + 1);
        let line_end = chars[end..]
            .iter()
            .position(|&c| c == '\n')
            .map_or(chars.len(), |i| end + i);
        let from = start.saturating_sub(SNIPPET_BEFORE).max(line_start);
        let to = (end + SNIPPET_AFTER).min(line_end.max(end));
        let clean = |c: &char| match *c {
            SNIPPET_OPEN | SNIPPET_CLOSE => '\u{FFFD}',
            c => c,
        };
        let mut s = String::with_capacity((to - from) * 2 + 8);
        s.extend(chars[from..start].iter().map(clean));
        s.push(SNIPPET_OPEN);
        s.extend(chars[start..end].iter().map(clean));
        s.push(SNIPPET_CLOSE);
        s.extend(chars[end..to].iter().map(clean));
        Some(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folding_is_one_to_one() {
        assert_eq!(fold("ÄRGER Straße İx"), "ärger straße İx");
        for s in ["ÄÖÜ", "ΣΑΣ", "İ", "ǅ", "ﬃ"] {
            assert_eq!(fold(s).chars().count(), s.chars().count(), "{s}");
        }
    }

    #[test]
    fn like_patterns() {
        let t = Term::new("grep needle", false);
        assert_eq!((t.pattern(), t.op()), ("%grep needle%", "LIKE"));
        assert!(t.indexable() && !t.needs_post_filter());

        let t = Term::new("50%_off", false);
        assert_eq!(t.pattern(), "%50%_off%");
        assert!(t.indexable() && t.needs_post_filter());

        let t = Term::new("Ärger", false);
        assert_eq!(t.pattern(), "%_rger%");
        assert!(t.indexable() && t.needs_post_filter());

        let t = Term::new("äöü", false);
        assert_eq!(t.pattern(), "%___%");
        assert!(!t.indexable());

        // Uncased non-ASCII stays literal; short multi-byte runs never reach the index.
        let t = Term::new("a─b", false);
        assert_eq!(t.pattern(), "%a─b%");
        assert!(t.indexable() && !t.needs_post_filter());
        let t = Term::new("abc%─", false);
        assert_eq!(t.pattern(), "%abc%_%");
        assert!(t.indexable() && t.needs_post_filter());
        let t = Term::new("ab%cd", false);
        assert_eq!(t.pattern(), "%ab%cd%");
        assert!(!t.indexable());
    }

    #[test]
    fn glob_patterns_escape_metacharacters() {
        let t = Term::new("a*b?c[d]", true);
        assert_eq!((t.pattern(), t.op()), ("*a[*]b[?]c[[]d]*", "GLOB"));
        assert!(!t.indexable(), "no literal run of 3");
        let t = Term::new("main[0]", true);
        assert_eq!(t.pattern(), "*main[[]0]*");
        assert!(t.indexable() && !t.needs_post_filter());
        let t = Term::new("Ärger", true);
        assert_eq!(t.pattern(), "*Ärger*");
        assert!(t.indexable() && !t.needs_post_filter());
    }

    #[test]
    fn post_filter_is_literal_and_unicode_aware() {
        let t = Term::new("50%_off", false);
        assert!(t.matches("now 50%_OFF!"));
        assert!(!t.matches("now 50% off"));
        assert!(!t.matches("50xyoff"));
        let t = Term::new("ärger", false);
        assert!(t.matches("viel ÄRGER hier"));
        assert!(!t.matches("viel arger"));
        let t = Term::new("Ärger", true);
        assert!(!t.matches("ärger"));
        assert!(t.matches("Ärger"));
    }

    #[test]
    fn snippets_stay_on_the_matching_line() {
        let t = Term::new("needle", false);
        let text = format!("first line\n{}xx NEEDLE yy\nlast", "p".repeat(40));
        let s = t.snippet(&text).unwrap();
        assert_eq!(
            s,
            format!("{}xx {SNIPPET_OPEN}NEEDLE{SNIPPET_CLOSE} yy", "p".repeat(29))
        );
        assert!(t.snippet("nothing").is_none());

        let t = Term::new("äb", false);
        let s = Term::new("äb", false).snippet("ÄÄB\u{E000}").unwrap();
        assert_eq!(s, format!("Ä{SNIPPET_OPEN}ÄB{SNIPPET_CLOSE}\u{FFFD}"));
        assert!(t.matches("xÄb"));
    }
}
