//! Alignment of `Begin` banners with commands.log entries (§11 step 5) and reconstruction of
//! the command text from the screen (§11 step 3).
//!
//! Every banner has up to three kinds of context: the window title the old preexec hook set
//! right after it (`%n@%m: %~ $3`, alias-expanded like the log), the logical line above the
//! cursor (what was typed), and that line joined with up to 8 earlier ones (multi-line
//! commands). A log entry's score against a banner is its best [`sim::score`] over them.
//! Needleman–Wunsch then picks the order-preserving matching with the highest total score;
//! both kinds of gap are cheap: banners without a log line (`/hist` was never logged) and
//! log lines without a banner (`SHOW_START_END_TIME=0`).

use std::collections::HashMap;
use std::sync::LazyLock;

use regex::Regex;

use super::log::LogEntry;
use super::sim;

/// Lowest score that counts as a match.
pub(crate) const MIN_MATCH: f64 = 0.5;
/// Gap costs (relative to a match's `score - MIN_MATCH`).
const GAP_BANNER: f64 = 0.02;
const GAP_LOG: f64 = 0.05;
/// Score factor for raw (unparsable) log lines.
const RAW_PRIOR: f64 = 0.7;
/// Largest alignment matrix computed in one piece.
const MAX_CELLS: usize = 4_000_000;
/// Logical lines read above the cursor for one command (the last one plus 8 earlier).
pub(crate) const SCREEN_LINES: usize = 9;

/// zsh's default `PS2` (`%_> `: `for> `, `dquote> `, `then else> `, …).
static PS2: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\s*(?:[a-z]+ )*[a-z]+> ").expect("valid regex"));

/// Prompt terminators assumed before any were learned from a cast.
const DEFAULT_TERMINATORS: &[&str] = &["$", "#", "%", "❯", "➜", "»", "λ", ">"];

/// What the pipeline captured at one `Begin`.
#[derive(Debug, Clone, Copy)]
pub(crate) struct BannerView<'a> {
    /// Logical lines above the cursor, oldest first; the last one ends right above it.
    pub screen: &'a [String],
    pub title: Option<&'a str>,
}

/// Normalised contexts of one banner.
struct Contexts {
    title: Option<String>,
    /// `screens[k - 1]`: the last `k` lines joined (continuation prompts removed).
    screens: Vec<String>,
}

impl Contexts {
    fn of(b: BannerView<'_>) -> Self {
        let lines = b.screen;
        let mut screens = Vec::with_capacity(lines.len());
        for k in 1..=lines.len() {
            let window = &lines[lines.len() - k..];
            let mut joined = String::new();
            for (i, l) in window.iter().enumerate() {
                if i > 0 {
                    joined.push('\n');
                    joined.push_str(strip_ps2(l));
                } else {
                    joined.push_str(l);
                }
            }
            screens.push(sim::normalize(&joined));
        }
        Self {
            title: b.title.map(sim::normalize).filter(|t| !t.is_empty()),
            screens,
        }
    }

    fn score(&self, entry: &LogEntry, norm: &str) -> f64 {
        if norm.is_empty() {
            return 0.0;
        }
        // Fuzzy matching only for windows that can hold the entry's lines.
        let fuzzy_k = 1 + norm.matches(';').count();
        let mut best: f64 = 0.0;
        let contexts = self
            .title
            .iter()
            .map(|t| (t, true))
            .chain(self.screens.iter().enumerate().map(|(i, s)| (s, i < fuzzy_k)));
        for (ctx, fuzzy) in contexts {
            let mut s = if fuzzy {
                sim::score(ctx, norm)
            } else {
                sim::exact_score(ctx, norm)
            };
            if entry.truncated {
                s = s.max(sim::prefix_score(ctx, norm));
            }
            best = best.max(s);
            if best >= 1.0 {
                break;
            }
        }
        if entry.parsed { best } else { best * RAW_PRIOR }
    }
}

/// One aligned pair.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Match {
    pub banner: usize,
    pub entry: usize,
    pub score: f64,
}

/// Aligns `banners` with `entries`; returns the matched pairs in order.
pub(crate) fn align(banners: &[BannerView<'_>], entries: &[LogEntry]) -> Vec<Match> {
    let (n, m) = (banners.len(), entries.len());
    let cells = (n + 1).saturating_mul(m + 1);
    // Larger inputs (far beyond the real corpus) are cut into proportional blocks.
    let blocks = cells.div_ceil(MAX_CELLS).max(1);
    let mut out = Vec::new();
    for k in 0..blocks {
        let (b0, b1) = (k * n / blocks, (k + 1) * n / blocks);
        let (e0, e1) = (k * m / blocks, (k + 1) * m / blocks);
        out.extend(
            align_block(&banners[b0..b1], &entries[e0..e1])
                .into_iter()
                .map(|x| Match {
                    banner: x.banner + b0,
                    entry: x.entry + e0,
                    score: x.score,
                }),
        );
    }
    out
}

fn align_block(banners: &[BannerView<'_>], entries: &[LogEntry]) -> Vec<Match> {
    #[derive(Clone, Copy)]
    enum Step {
        Match,
        SkipBanner,
        SkipEntry,
    }
    let (n, m) = (banners.len(), entries.len());
    if n == 0 || m == 0 {
        return Vec::new();
    }
    let ctx: Vec<Contexts> = banners.iter().map(|b| Contexts::of(*b)).collect();
    let norm: Vec<String> = entries.iter().map(|e| sim::normalize(&e.text)).collect();
    let w = m + 1;
    let mut score = vec![0.0f64; (n + 1) * w];
    let mut step = vec![Step::SkipEntry; (n + 1) * w];
    let mut pair = vec![0.0f64; (n + 1) * w];
    for j in 1..=m {
        score[j] = score[j - 1] - GAP_LOG;
    }
    for i in 1..=n {
        score[i * w] = score[(i - 1) * w] - GAP_BANNER;
        step[i * w] = Step::SkipBanner;
        for j in 1..=m {
            let at = i * w + j;
            let mut best = (score[(i - 1) * w + j] - GAP_BANNER, Step::SkipBanner);
            let v = score[at - 1] - GAP_LOG;
            if v > best.0 {
                best = (v, Step::SkipEntry);
            }
            let s = ctx[i - 1].score(&entries[j - 1], &norm[j - 1]);
            if s >= MIN_MATCH {
                let v = score[(i - 1) * w + j - 1] + s - MIN_MATCH;
                if v > best.0 {
                    pair[at] = s;
                    best = (v, Step::Match);
                }
            }
            score[at] = best.0;
            step[at] = best.1;
        }
    }
    let mut out = Vec::new();
    let (mut i, mut j) = (n, m);
    while i > 0 || j > 0 {
        let at = i * w + j;
        match step[at] {
            Step::Match => {
                out.push(Match {
                    banner: i - 1,
                    entry: j - 1,
                    score: pair[at],
                });
                i -= 1;
                j -= 1;
            }
            Step::SkipBanner => i -= 1,
            Step::SkipEntry => j -= 1,
        }
    }
    out.reverse();
    out
}

/// Prompt terminators of a cast, most frequent first.
#[derive(Debug, Clone, Default)]
pub(crate) struct Prompts {
    terminators: Vec<String>,
}

impl Prompts {
    /// Learns from aligned single-line commands: what precedes the command on its screen
    /// line is the prompt, and its trailing punctuation the terminator.
    pub(crate) fn learn(banners: &[BannerView<'_>], entries: &[LogEntry], matches: &[Match]) -> Self {
        let mut counts: HashMap<String, usize> = HashMap::new();
        for m in matches {
            let Some(line) = banners[m.banner].screen.last() else {
                continue;
            };
            let line = sim::normalize(line);
            let cmd = sim::normalize(&entries[m.entry].text);
            let Some(prompt) = line.strip_suffix(cmd.as_str()) else {
                continue;
            };
            if let Some(t) = terminator(prompt) {
                *counts.entry(t).or_default() += 1;
            }
        }
        let mut terms: Vec<(String, usize)> = counts.into_iter().collect();
        terms.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        Self {
            terminators: terms.into_iter().map(|(t, _)| t).collect(),
        }
    }

    /// The command typed at a banner, read from its screen lines: from the nearest line
    /// with a prompt down to the line above the cursor.
    pub(crate) fn reconstruct(&self, screen: &[String]) -> String {
        let learned: Vec<&str> = self.terminators.iter().map(String::as_str).collect();
        let terms: &[&str] = if learned.is_empty() {
            DEFAULT_TERMINATORS
        } else {
            &learned
        };
        for i in (0..screen.len()).rev() {
            let line = &screen[i];
            if PS2.is_match(line) {
                continue;
            }
            if let Some(pos) = prompt_end(line, terms) {
                let mut out = cut_right_prompt(&line[pos..]).to_owned();
                for l in &screen[i + 1..] {
                    out.push('\n');
                    out.push_str(cut_right_prompt(strip_ps2(l)));
                }
                return out.trim().to_owned();
            }
        }
        screen
            .last()
            .map(|l| cut_right_prompt(l).trim().to_owned())
            .unwrap_or_default()
    }
}

/// Trailing punctuation of a prompt (`user@host:~$` → `$`, `~/src on main ❯` → `❯`).
fn terminator(prompt: &str) -> Option<String> {
    let p = prompt.trim_end();
    let tail: Vec<char> = p
        .chars()
        .rev()
        .take_while(|c| !c.is_alphanumeric() && !c.is_whitespace() && !matches!(c, '/' | '~' | '.' | '-' | '_'))
        .collect();
    if tail.is_empty() || tail.len() > 3 {
        return None;
    }
    Some(tail.into_iter().rev().collect())
}

/// Byte index right after the first `<terminator> ` that has text after it.
fn prompt_end(line: &str, terms: &[&str]) -> Option<usize> {
    terms
        .iter()
        .filter_map(|t| {
            let pat = format!("{t} ");
            line.match_indices(&pat)
                .map(|(i, _)| i + pat.len())
                .find(|&end| !line[end..].trim().is_empty())
        })
        .min()
}

fn strip_ps2(line: &str) -> &str {
    PS2.find(line).map_or(line, |m| &line[m.end()..])
}

/// A right prompt follows the command after a long run of spaces.
fn cut_right_prompt(s: &str) -> &str {
    s.find("      ").map_or(s, |i| &s[..i])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(text: &str) -> LogEntry {
        LogEntry {
            text: text.into(),
            parsed: true,
            truncated: false,
        }
    }

    fn lines(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn aligns_with_gaps_on_both_sides() {
        let screens = [
            lines(&["$ ls"]),
            lines(&["$ hist foo"]), // never logged
            lines(&["$ git status"]),
            lines(&["$ make"]),
        ];
        let titles = [Some("u@h: ~ ls --color=tty"), None, None, Some("u@h: ~ make")];
        let banners: Vec<BannerView<'_>> = screens
            .iter()
            .zip(titles)
            .map(|(s, t)| BannerView { screen: s, title: t })
            .collect();
        // `pwd` ran while banners were switched off.
        let entries = [
            entry("ls --color=tty"),
            entry("git status"),
            entry("pwd"),
            entry("make"),
        ];
        let m = align(&banners, &entries);
        let pairs: Vec<(usize, usize)> = m.iter().map(|m| (m.banner, m.entry)).collect();
        assert_eq!(pairs, [(0, 0), (2, 1), (3, 3)]);
        assert!(m.iter().all(|m| m.score >= 0.95), "{m:?}");
    }

    #[test]
    fn repeated_commands_keep_their_order() {
        let screens: Vec<Vec<String>> = ["$ ls", "$ cd x", "$ ls", "$ ls"].iter().map(|l| lines(&[l])).collect();
        let banners: Vec<_> = screens.iter().map(|s| BannerView { screen: s, title: None }).collect();
        let entries = [entry("ls"), entry("cd x"), entry("ls"), entry("ls")];
        let pairs: Vec<_> = align(&banners, &entries).iter().map(|m| (m.banner, m.entry)).collect();
        assert_eq!(pairs, [(0, 0), (1, 1), (2, 2), (3, 3)]);
    }

    #[test]
    fn multi_line_and_truncated_entries() {
        let multi = lines(&["out", "❯ for i in 1 2; do", "for> echo $i", "for> done"]);
        let trunc = lines(&["❯ printf 'a\\cb'"]);
        let banners = [
            BannerView {
                screen: &multi,
                title: None,
            },
            BannerView {
                screen: &trunc,
                title: None,
            },
        ];
        let mut t = entry("printf 'a");
        t.truncated = true;
        let entries = [entry("for i in 1 2;do;echo $i;done"), t];
        let m = align(&banners, &entries);
        assert_eq!(m.len(), 2);
        assert_eq!(m[0].score, 1.0);
        assert!(m[1].score >= 0.8, "{m:?}");
    }

    #[test]
    fn unrelated_entries_stay_unmatched() {
        let s = lines(&["$ vim notes.txt"]);
        let banners = [BannerView {
            screen: &s,
            title: None,
        }];
        assert!(align(&banners, &[entry("htop")]).is_empty());
        let mut raw = entry("vim notes.txt");
        raw.parsed = false;
        let m = align(&banners, &[raw]);
        assert!((m[0].score - RAW_PRIOR).abs() < 1e-9);
    }

    #[test]
    fn sparse_log_aligns_completely() {
        let screens: Vec<Vec<String>> = (0..40).map(|i| lines(&[&format!("$ cmd{i} arg")])).collect();
        let banners: Vec<_> = screens.iter().map(|s| BannerView { screen: s, title: None }).collect();
        let entries: Vec<_> = (0..40)
            .filter(|i| i % 7 != 3)
            .map(|i| entry(&format!("cmd{i} arg")))
            .collect();
        let full = align(&banners, &entries);
        assert_eq!(full.len(), entries.len());
        assert!(full.iter().all(|m| m.score == 1.0));
    }

    #[test]
    fn prompts_are_learned_and_used_for_reconstruction() {
        let a = lines(&["user@host:~/src$ make all"]);
        let b = lines(&["user@host:~/src$ ls"]);
        let banners = [
            BannerView {
                screen: &a,
                title: None,
            },
            BannerView {
                screen: &b,
                title: None,
            },
        ];
        let entries = [entry("make all"), entry("ls")];
        let m = align(&banners, &entries);
        let p = Prompts::learn(&banners, &entries, &m);
        assert_eq!(p.terminators, ["$"]);
        assert_eq!(
            p.reconstruct(&lines(&["prev output", "user@host:~/x$ hist foo"])),
            "hist foo"
        );
        assert_eq!(
            p.reconstruct(&lines(&[
                "x",
                "user@host:~$ for i in 1 2; do",
                "for> echo $i",
                "for> done"
            ])),
            "for i in 1 2; do\necho $i\ndone"
        );
        let d = Prompts::default();
        assert_eq!(
            d.reconstruct(&lines(&["~/src ❯ git log          ✔  10:32:01"])),
            "git log"
        );
        assert_eq!(d.reconstruct(&lines(&["no prompt here"])), "no prompt here");
        assert_eq!(d.reconstruct(&[]), "");
        assert_eq!(terminator("[user@host src]$"), Some("]$".into()));
        assert_eq!(terminator("~/src"), None);
    }
}
