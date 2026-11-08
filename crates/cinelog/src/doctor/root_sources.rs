//! What root's shell sources at startup, and whether any of it is not root-owned (§5.6).
//!
//! A static scan: `source`/`.` arguments and `zmodule` paths in root's zsh startup files
//! and the system zshrc, plus the zim install those files load. Arguments that use other
//! variables or command substitution cannot be resolved and are skipped.

use std::path::{Path, PathBuf};

use cinelog_core::location::{self, InsecurePath};

/// Root's own startup files, in the order zsh reads them, and `.zimrc`.
const DOTFILES: [&str; 5] = [".zshenv", ".zprofile", ".zshrc", ".zlogin", ".zimrc"];

/// Directories a startup file's paths are expanded against.
#[derive(Debug, Clone)]
pub struct Homes {
    pub home: PathBuf,
    pub zim_home: PathBuf,
}

impl Homes {
    /// Root's home, with `ZIM_HOME` from the first plain assignment in its startup files
    /// (zim's default `~/.zim` otherwise; root ignores an inherited `ZIM_HOME`).
    pub fn of(home: &Path) -> Self {
        let mut homes = Self {
            home: home.to_path_buf(),
            zim_home: home.join(".zim"),
        };
        let assigned = DOTFILES
            .iter()
            .filter_map(|f| std::fs::read_to_string(home.join(f)).ok())
            .find_map(|text| zim_home_assignment(&text, &homes));
        if let Some(z) = assigned {
            homes.zim_home = z;
        }
        homes
    }

    /// `~`, `$HOME` and `$ZIM_HOME` (also braced, and `${ZDOTDIR:-$HOME}`) expanded; `None`
    /// when anything else would need the shell.
    fn expand(&self, word: &str) -> Option<PathBuf> {
        let word = word.trim_matches(|c| c == '"' || c == '\'');
        let home = self.home.to_str()?;
        let zim = self.zim_home.to_str()?;
        let mut s = word.to_owned();
        if let Some(rest) = s.strip_prefix('~') {
            if !(rest.is_empty() || rest.starts_with('/')) {
                return None;
            }
            s = format!("{home}{rest}");
        }
        for (var, value) in [
            ("${ZDOTDIR:-$HOME}", home),
            ("${ZDOTDIR:-${HOME}}", home),
            ("${ZDOTDIR:-~}", home),
            ("${HOME}", home),
            ("$HOME", home),
            ("${ZIM_HOME}", zim),
            ("$ZIM_HOME", zim),
        ] {
            s = s.replace(var, value);
        }
        if s.contains(['$', '`', '*', '?', '(']) || !s.starts_with('/') {
            return None;
        }
        Some(PathBuf::from(s))
    }
}

fn zim_home_assignment(text: &str, homes: &Homes) -> Option<PathBuf> {
    code_lines(text).find_map(|l| {
        let l = l.strip_prefix("export ").unwrap_or(l).trim_start();
        let value = l.strip_prefix("ZIM_HOME=")?;
        let value = value.split_whitespace().next()?;
        homes.expand(value)
    })
}

/// Non-empty lines without their comment-only form.
fn code_lines(text: &str) -> impl Iterator<Item = &str> {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
}

/// The paths `text` sources or loads as zim modules.
pub fn sourced_paths(text: &str, homes: &Homes) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for line in code_lines(text) {
        let words: Vec<&str> = line
            .split(|c: char| c.is_whitespace() || c == ';' || c == '&' || c == '|')
            .filter(|w| !w.is_empty())
            .collect();
        for pair in words.windows(2) {
            let is_load = matches!(pair[0], "source" | "." | "zmodule");
            if is_load && let Some(p) = homes.expand(pair[1]) {
                out.push(p);
            }
        }
    }
    out
}

/// A startup path that root executes although it breaks the ownership rule.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Untrusted {
    /// The startup file that loads it (`None`: the file itself or the zim install).
    pub via: Option<PathBuf>,
    pub bad: InsecurePath,
}

/// Checks root's startup files, what they source, and the zim install, against `uid` up to
/// `stop`. Missing files are skipped. Returns every offender, startup files first.
pub fn untrusted(homes: &Homes, system_rc: &Path, uid: u32, stop: &Path) -> Vec<Untrusted> {
    let exists = |p: &Path| std::fs::symlink_metadata(p).is_ok();
    let check = |p: &Path| location::insecure_component(p, uid, stop);
    let mut files: Vec<PathBuf> = vec![system_rc.to_path_buf()];
    files.extend(DOTFILES.iter().map(|f| homes.home.join(f)));
    let files: Vec<PathBuf> = files.into_iter().filter(|f| exists(f)).collect();

    let mut out = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    for f in &files {
        if let Some(bad) = check(f) {
            seen.insert(bad.path.clone());
            out.push(Untrusted { via: None, bad });
        }
    }
    for f in &files {
        let Ok(text) = std::fs::read_to_string(f) else { continue };
        for target in sourced_paths(&text, homes).into_iter().filter(|p| exists(p)) {
            if let Some(bad) = check(&target)
                && seen.insert(bad.path.clone())
            {
                out.push(Untrusted {
                    via: Some(f.clone()),
                    bad,
                });
            }
        }
    }
    if files.iter().any(|f| f.ends_with(".zimrc")) || exists(&homes.zim_home) {
        let mut zim = vec![homes.zim_home.join("init.zsh")];
        if let Ok(rd) = std::fs::read_dir(homes.zim_home.join("modules")) {
            let mut modules: Vec<PathBuf> = rd.filter_map(Result::ok).map(|e| e.path()).collect();
            modules.sort();
            zim.extend(modules);
        }
        for p in zim.into_iter().filter(|p| exists(p)) {
            if let Some(bad) = check(&p)
                && seen.insert(bad.path.clone())
            {
                out.push(Untrusted { via: None, bad });
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    fn homes() -> Homes {
        Homes {
            home: "/root".into(),
            zim_home: "/root/.zim".into(),
        }
    }

    #[test]
    fn sourced_paths_are_expanded() {
        let text = "# source /nope\n\
                    source ${ZIM_HOME}/init.zsh\n\
                    [[ ! -f ~/.p10k.zsh ]] || source ~/.p10k.zsh\n\
                    . \"$HOME/x.zsh\"; source /home/u/.zim/modules/cinelog/cinelog.plugin.zsh\n\
                    source $(brew --prefix)/y.zsh\n\
                    source relative.zsh\n\
                    zmodule zsh-users/zsh-autosuggestions\n\
                    zmodule /home/u/src/cinelog --on-pull 'zsh shell/fetch.zsh'\n";
        let got = sourced_paths(text, &homes());
        let want: Vec<PathBuf> = [
            "/root/.zim/init.zsh",
            "/root/.p10k.zsh",
            "/root/x.zsh",
            "/home/u/.zim/modules/cinelog/cinelog.plugin.zsh",
            "/home/u/src/cinelog",
        ]
        .map(PathBuf::from)
        .into();
        assert_eq!(got, want);
    }

    #[test]
    fn zim_home_comes_from_roots_files() {
        let d = tempfile::tempdir().unwrap();
        assert_eq!(Homes::of(d.path()).zim_home, d.path().join(".zim"));
        fs::write(d.path().join(".zshenv"), "export ZIM_HOME=~/.cache/zim\n").unwrap();
        assert_eq!(Homes::of(d.path()).zim_home, d.path().join(".cache/zim"));
    }

    #[test]
    fn a_user_owned_or_writable_source_is_reported() {
        let d = tempfile::tempdir().unwrap();
        let base = d.path();
        let uid = nix::unistd::geteuid().as_raw();
        let set = |p: &Path, m: u32| fs::set_permissions(p, fs::Permissions::from_mode(m)).unwrap();
        let write = |p: &Path, text: &str| {
            fs::write(p, text).unwrap();
            set(p, 0o644);
        };
        set(base, 0o755);
        let home = base.join("root");
        let user = base.join("home/u");
        let module = user.join(".zim/modules/cinelog");
        fs::create_dir_all(&home).unwrap();
        fs::create_dir_all(&module).unwrap();
        for dir in [
            home.as_path(),
            &base.join("home"),
            &user,
            &user.join(".zim"),
            &user.join(".zim/modules"),
            &module,
        ] {
            set(dir, 0o755);
        }
        write(&user.join("plugin.zsh"), "");
        let rc = base.join("zshrc");
        write(&rc, "");
        write(
            &home.join(".zshrc"),
            &format!("source {}\nsource /missing/file\n", user.join("plugin.zsh").display()),
        );
        let homes = Homes::of(&home);

        // Everything owned by us with safe modes: nothing to report.
        assert_eq!(untrusted(&homes, &rc, uid, base), []);

        // A group-writable directory on the sourced path.
        set(&user, 0o775);
        let found = untrusted(&homes, &rc, uid, base);
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].bad.path, user);
        assert_eq!(found[0].via.as_deref(), Some(home.join(".zshrc").as_path()));
        set(&user, 0o755);

        // Another owner (here: expected uid differs) flags the startup file itself first.
        let other = untrusted(&homes, &rc, uid.wrapping_add(1), base);
        assert_eq!(other[0].bad.path, rc);
        assert!(other[0].bad.problem.contains("owned by uid"));

        // A user's zim install loaded by root's .zimrc.
        write(&home.join(".zimrc"), "zmodule cmprmsd/cinelog\n");
        write(
            &home.join(".zshenv"),
            &format!("ZIM_HOME={}\n", user.join(".zim").display()),
        );
        set(&module, 0o777);
        let homes = Homes::of(&home);
        let found = untrusted(&homes, &rc, uid, base);
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].bad.path, module);
    }
}
