//! Secret leak checks (I6). A leak need not be the whole secret in one run of bytes: a
//! secret split across reads, or only partly suppressed by the keystroke gate, leaves
//! fragments. So every window of `n` characters of the secret is searched for.

use std::path::Path;

/// A fresh secret none of whose 4-character windows can occur by chance in a sandbox file:
/// a cycle of a letter that is not a hex digit, a digit and `@` (`q7@X2@…`). Hex ids,
/// base64 keys, JSON and `user@host` never contain such a window.
pub fn random_secret() -> String {
    const LETTERS: &[u8] = b"qxzjQXZJ";
    let mut raw = [0u8; 18];
    getrandom::fill(&mut raw).expect("getrandom");
    raw.iter()
        .enumerate()
        .map(|(i, b)| match i % 3 {
            0 => LETTERS[usize::from(*b) % LETTERS.len()] as char,
            1 => char::from(b'0' + b % 10),
            _ => '@',
        })
        .collect()
}

/// Every window of `n` characters of `secret` (the whole secret if it is shorter).
pub fn fragments(secret: &str, n: usize) -> Vec<String> {
    let chars: Vec<char> = secret.chars().collect();
    if chars.len() <= n {
        return vec![secret.to_owned()];
    }
    let mut v: Vec<String> = chars.windows(n).map(|w| w.iter().collect()).collect();
    v.dedup();
    v
}

/// Fragments of `secret` (windows of `n` chars) found in `haystack`, except those that also
/// occur in `known`: text that is legitimately there, such as what the test itself typed
/// and the visible screen (pass `""` when the secret cannot occur by chance).
pub fn leaked(secret: &str, n: usize, haystack: &str, known: &str) -> Vec<String> {
    fragments(secret, n)
        .into_iter()
        .filter(|f| haystack.contains(f.as_str()) && !known.contains(f.as_str()))
        .collect()
}

/// Every regular file below `dir` (symlinks are not followed) with its contents.
pub fn files_below(dir: &Path) -> Vec<(std::path::PathBuf, Vec<u8>)> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else { continue };
        for e in rd.flatten() {
            match e.file_type() {
                Ok(t) if t.is_dir() => stack.push(e.path()),
                Ok(t) if t.is_file() => {
                    if let Ok(b) = std::fs::read(e.path()) {
                        out.push((e.path(), b));
                    }
                }
                _ => {}
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn random_secrets_have_the_rare_shape() {
        let s = random_secret();
        assert_eq!(s.len(), 18);
        for (i, c) in s.chars().enumerate() {
            match i % 3 {
                0 => assert!("qxzjQXZJ".contains(c), "{s}"),
                1 => assert!(c.is_ascii_digit(), "{s}"),
                _ => assert_eq!(c, '@', "{s}"),
            }
        }
        for f in fragments(&s, 4) {
            assert!(f.contains('@') && f.chars().any(|c| c.is_ascii_digit()), "{f}");
        }
        assert_ne!(random_secret(), s);
    }

    #[test]
    fn fragments_are_all_windows() {
        assert_eq!(fragments("abcdef", 4), ["abcd", "bcde", "cdef"]);
        assert_eq!(fragments("abc", 4), ["abc"]);
    }

    #[test]
    fn a_secret_split_or_partly_suppressed_is_still_found() {
        let s = "q1@x2@z3@j4@Q5@X6@";
        // The gate dropped the first read; the rest was recorded.
        assert!(!leaked(s, 4, "echo hi\rz3@j4@Q5@X6@\r", "").is_empty());
        // Recorded in two reads with other input between them.
        let split = format!("{}\u{1b}[A{}", &s[..7], &s[7..]);
        assert!(!split.contains(s));
        assert!(!leaked(s, 4, &split, "").is_empty());
        assert!(leaked(s, 4, "echo ordinary keystrokes\r", "").is_empty());
    }

    #[test]
    fn known_text_is_not_a_leak() {
        // A provisioned password may share fragments with the prompt ("pass" in "password").
        let pw = "pass-1234";
        let screen = "[sudo] password for clpw:";
        assert_eq!(leaked(pw, 4, screen, screen), Vec::<String>::new());
        assert_eq!(leaked(pw, 4, "x-1234", screen), ["-123", "1234"]);
    }
}
