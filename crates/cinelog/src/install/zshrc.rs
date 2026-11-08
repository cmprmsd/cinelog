//! The managed block in the system zshrc (§5.6, §9.7): root shells source the root-owned
//! plugin copy before any user dotfile runs. zsh only.

use std::path::{Path, PathBuf};

use cinelog_core::paths::{SystemRoot, team};

pub const BEGIN: &str = "# >>> cinelog >>>";
pub const END: &str = "# <<< cinelog <<<";

/// `/etc/zsh/zshrc` (Debian, Arch) unless only `/etc/zshrc` (Fedora, macOS-style) exists.
pub fn system_zshrc(root: &SystemRoot) -> PathBuf {
    let debian = root.join(team::SYSTEM_ZSHRC);
    let other = root.join("/etc/zshrc");
    if !debian.exists() && !root.join("/etc/zsh").is_dir() && other.exists() {
        other
    } else {
        debian
    }
}

/// Single-quoted for zsh.
fn quote(p: &Path) -> String {
    format!("'{}'", p.display().to_string().replace('\'', r"'\''"))
}

pub fn block(plugin: &Path) -> String {
    let q = quote(plugin);
    format!(
        "{BEGIN}\n\
         # Records root shells (cinelog TEAM scope) before any user dotfile runs.\n\
         # Managed by `cinelog install --team --system-zshrc`; removed by `--team --uninstall`.\n\
         if (( EUID == 0 )) && [[ -r {q} ]]; then\n  source {q}\nfi\n\
         {END}\n"
    )
}

/// The block from the plugin's own snippet (`shell/system-zshrc.zsh`), pointed at `plugin`.
/// `None` if the snippet has no managed block.
pub fn block_from_snippet(snippet: &str, plugin: &Path) -> Option<String> {
    let r = span(snippet)?;
    let mut text = snippet[r].to_owned();
    if !text.ends_with('\n') {
        text.push('\n');
    }
    if plugin != Path::new(team::PLUGIN_COPY) {
        text = text.replace(team::PLUGIN_COPY, &quote(plugin));
    }
    Some(text)
}

/// Byte range of the managed block including its trailing newline.
fn span(text: &str) -> Option<std::ops::Range<usize>> {
    let start = text
        .match_indices(BEGIN)
        .find(|(i, _)| *i == 0 || text.as_bytes()[i - 1] == b'\n')?
        .0;
    let end_rel = text[start..].find(END)? + END.len();
    let mut end = start + end_rel;
    if text[end..].starts_with('\n') {
        end += 1;
    }
    Some(start..end)
}

pub fn has_block(text: &str) -> bool {
    span(text).is_some()
}

/// Replaces an existing block in place or appends one.
pub fn upsert(text: &str, block: &str) -> String {
    match span(text) {
        Some(r) => format!("{}{block}{}", &text[..r.start], &text[r.end..]),
        None if text.is_empty() => block.to_owned(),
        None => {
            let sep = if text.ends_with('\n') { "\n" } else { "\n\n" };
            format!("{text}{sep}{block}")
        }
    }
}

/// Removes the block and the blank line `upsert` put before it.
pub fn remove(text: &str) -> String {
    match span(text) {
        Some(r) => {
            let head = &text[..r.start];
            let head = head.strip_suffix("\n\n").map_or(head, |h| &head[..h.len() + 1]);
            format!("{head}{}", &text[r.end..])
        }
        None => text.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn block_is_guarded_and_quoted() {
        let b = block(Path::new("/usr/local/share/cinelog/cinelog.plugin.zsh"));
        assert!(b.contains("if (( EUID == 0 )) && [[ -r '/usr/local/share/cinelog/cinelog.plugin.zsh' ]]; then"));
        assert!(b.starts_with(BEGIN) && b.ends_with(&format!("{END}\n")));
        assert!(block(Path::new("/t/it's")).contains(r"'/t/it'\''s'"));
    }

    #[test]
    fn snippet_is_used_and_relocated() {
        let snippet = "# >>> cinelog >>> managed\nif (( EUID == 0 )) && [[ -r /usr/local/share/cinelog/cinelog.plugin.zsh ]]; then\n  \
                       source /usr/local/share/cinelog/cinelog.plugin.zsh\nfi\n# <<< cinelog <<<";
        let real = block_from_snippet(snippet, Path::new(team::PLUGIN_COPY)).unwrap();
        assert_eq!(real, format!("{snippet}\n"));
        let moved = block_from_snippet(snippet, Path::new("/t/p/usr/local/share/cinelog/cinelog.plugin.zsh")).unwrap();
        assert!(moved.contains("source '/t/p/usr/local/share/cinelog/cinelog.plugin.zsh'"));
        assert!(!moved.contains(" /usr/local/share"));
        assert_eq!(remove(&upsert("x\n", &moved)), "x\n");
        assert_eq!(block_from_snippet("echo hi\n", Path::new("/x")), None);
    }

    #[test]
    fn upsert_and_remove_roundtrip() {
        let original = "# distro zshrc\nbindkey -e\n";
        let b1 = block(Path::new("/a"));
        let with = upsert(original, &b1);
        assert_eq!(with, format!("{original}\n{b1}"));
        assert!(has_block(&with));
        assert_eq!(upsert(&with, &b1), with, "idempotent");
        let b2 = block(Path::new("/b"));
        let replaced = upsert(&with, &b2);
        assert_eq!(replaced.matches(BEGIN).count(), 1);
        assert!(replaced.contains("'/b'"));
        assert_eq!(remove(&replaced), original);
        assert_eq!(remove(original), original);
        assert_eq!(upsert("", &b1), b1);
        assert_eq!(remove(&b1), "");
        assert_eq!(remove(&upsert("x", &b1)), "x\n");
    }

    #[test]
    fn block_in_the_middle_is_replaced_in_place() {
        let b = block(Path::new("/a"));
        let text = format!("top\n{b}bottom\n");
        assert_eq!(remove(&text), "top\nbottom\n");
        assert_eq!(upsert(&text, &b), text);
        assert!(!has_block("echo '# >>> cinelog >>>'\n"));
    }

    #[test]
    fn system_zshrc_location() {
        let d = tempfile::tempdir().unwrap();
        let root = SystemRoot::prefixed(d.path());
        assert_eq!(system_zshrc(&root), d.path().join("etc/zsh/zshrc"));
        std::fs::create_dir_all(d.path().join("etc")).unwrap();
        std::fs::write(d.path().join("etc/zshrc"), "").unwrap();
        assert_eq!(system_zshrc(&root), d.path().join("etc/zshrc"));
        std::fs::create_dir_all(d.path().join("etc/zsh")).unwrap();
        assert_eq!(system_zshrc(&root), d.path().join("etc/zsh/zshrc"));
    }
}
