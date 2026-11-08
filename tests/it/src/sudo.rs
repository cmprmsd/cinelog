//! Helpers for the root-gated scenarios (I5, the sudo part of I6). They run only with
//! `--features root` and `CINELOG_IT_ROOT=1`, on throwaway CI runners.
//!
//! [`TeamInstall`] follows plan §12.2: `sudo cinelog install --team --system-zshrc --prefix
//! <P>` into a throwaway root-owned prefix, so the binary and the root-owned plugin copy
//! never touch `/usr/local`. Two system locations cannot be avoided, because root's zsh only
//! reads its compiled-in system zshrc and TEAM paths at EUID 0 are fixed below `/` (§5.6):
//! - the prefix's managed block is appended to the real system zshrc, inside a `cinelog-it`
//!   wrapper; the byte-exact original is written back on drop;
//! - TEAM recorders write below `/root/logs`, `/run/cinelog` and `/var/lib/cinelog`; every
//!   path the test created there is removed on drop.
//!
//! It refuses to run over an existing team install, and one `TeamInstall` exists at a time
//! per test process (scenarios that need it are serialised).

use std::collections::BTreeSet;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use anyhow::{Context, Result, bail};

use crate::procs;

/// Default TEAM log root (`team::LOGS`).
pub const TEAM_LOGS: &str = "/root/logs";
/// Where TEAM recorders may create files: the log root, the registry, rec.log and the DB.
const RECORDER_DIRS: [&str; 4] = [TEAM_LOGS, "/run/cinelog", "/var/lib/cinelog", "/etc/cinelog"];
const SHARE_DIR: &str = "/usr/local/share/cinelog";
const PREFIX_TEMPLATE: &str = "/tmp/cinelog-it-team.XXXXXXXX";
const PREFIX_STEM: &str = "/tmp/cinelog-it-team.";
const BLOCK_BEGIN: &str = "# >>> cinelog >>>";
const BLOCK_END: &str = "# <<< cinelog <<<";
const WRAP_BEGIN: &str = "# >>> cinelog-it ";
const WRAP_BEGIN_TAIL: &str = " (temporary, removed by the cinelog integration tests) >>>";
const WRAP_END: &str = "# <<< cinelog-it <<<";

/// `sudo -n <args>`; fails unless the command succeeds.
pub fn sudo(args: &[&str]) -> Result<Output> {
    sudo_input(args, None)
}

pub fn sudo_stdout(args: &[&str]) -> Result<String> {
    Ok(String::from_utf8_lossy(&sudo(args)?.stdout).into_owned())
}

/// `sudo -n <args>` with `input` on stdin (secrets and file contents never go on argv).
pub fn sudo_input(args: &[&str], input: Option<&[u8]>) -> Result<Output> {
    let mut child = Command::new("sudo")
        .arg("-n")
        .args(args)
        .stdin(if input.is_some() { Stdio::piped() } else { Stdio::null() })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("sudo {args:?}"))?;
    if let (Some(data), Some(mut stdin)) = (input, child.stdin.take()) {
        stdin.write_all(data)?;
    }
    let out = child.wait_with_output()?;
    if !out.status.success() {
        bail!(
            "sudo {:?} failed: {}",
            args,
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(out)
}

/// The system zshrc root's zsh reads: `/etc/zsh/zshrc` unless only `/etc/zshrc` exists
/// (the rule of `install --team`).
pub fn system_zshrc() -> PathBuf {
    let debian = Path::new("/etc/zsh/zshrc");
    let other = Path::new("/etc/zshrc");
    if !debian.exists() && !Path::new("/etc/zsh").is_dir() && other.exists() {
        other.to_path_buf()
    } else {
        debian.to_path_buf()
    }
}

/// The managed `cinelog` block of a zshrc, both marker lines included.
pub fn managed_block(text: &str) -> Option<String> {
    let start = line_start(text, BLOCK_BEGIN, 0)?;
    let end = line_start(text, BLOCK_END, start)?;
    let end = text[end..].find('\n').map_or(text.len(), |i| end + i + 1);
    let mut block = text[start..end].to_owned();
    if !block.ends_with('\n') {
        block.push('\n');
    }
    Some(block)
}

/// Byte offset of the first line at or after `from` that starts with `prefix`.
fn line_start(text: &str, prefix: &str, from: usize) -> Option<usize> {
    let mut pos = from;
    for line in text[from..].split_inclusive('\n') {
        if line.starts_with(prefix) {
            return Some(pos);
        }
        pos += line.len();
    }
    None
}

/// `text` with every `cinelog-it` wrapper removed, and the prefixes the wrappers named.
pub fn strip_wrappers(text: &str) -> (String, Vec<String>) {
    let mut out = String::with_capacity(text.len());
    let mut prefixes = Vec::new();
    let mut inside = false;
    for line in text.split_inclusive('\n') {
        if !inside && let Some(rest) = line.strip_prefix(WRAP_BEGIN) {
            inside = true;
            if let Some(p) = rest.trim_end().strip_suffix(WRAP_BEGIN_TAIL) {
                prefixes.push(p.to_owned());
            }
            continue;
        }
        if inside {
            inside = !line.starts_with(WRAP_END);
            continue;
        }
        out.push_str(line);
    }
    (out, prefixes)
}

/// `original` plus the wrapped `block`.
pub fn with_wrapper(original: &str, prefix: &str, block: &str) -> String {
    let mut s = original.to_owned();
    if !s.is_empty() && !s.ends_with('\n') {
        s.push('\n');
    }
    s.push_str(&format!("{WRAP_BEGIN}{prefix}{WRAP_BEGIN_TAIL}\n{block}{WRAP_END}\n"));
    s
}

/// Paths of `after` that are not in `before` and whose parent is not new either: removing
/// these removes exactly what appeared.
pub fn new_roots(before: &BTreeSet<String>, after: &BTreeSet<String>) -> Vec<String> {
    let new: BTreeSet<&String> = after.difference(before).collect();
    new.iter()
        .filter(|p| {
            Path::new(p.as_str())
                .parent()
                .is_none_or(|parent| !new.contains(&parent.to_string_lossy().into_owned()))
        })
        .map(|p| (*p).clone())
        .collect()
}

/// Every path below the existing `dirs` (themselves included), listed as root.
fn snapshot(dirs: &[&str]) -> Result<BTreeSet<String>> {
    let mut args = vec![
        "sh",
        "-c",
        r#"for d; do if [ -e "$d" ]; then find "$d" -xdev; fi; done"#,
        "sh",
    ];
    args.extend_from_slice(dirs);
    Ok(sudo_stdout(&args)?.lines().map(str::to_owned).collect())
}

fn read_root_file(path: &Path) -> Result<Option<Vec<u8>>> {
    let p = path.to_string_lossy();
    if sudo(&["test", "-e", &p]).is_err() {
        return Ok(None);
    }
    Ok(Some(sudo(&["cat", "--", &p])?.stdout))
}

static TEAM_LOCK: Mutex<()> = Mutex::new(());

/// A TEAM integration from a throwaway prefix, live for root's zsh (see the module docs).
pub struct TeamInstall {
    prefix: Option<String>,
    rc: PathBuf,
    rc_original: Vec<u8>,
    rc_modified: bool,
    before: BTreeSet<String>,
    _lock: MutexGuard<'static, ()>,
}

impl TeamInstall {
    /// Installs `bin` and this checkout's plugin. Test binaries are local builds, so they
    /// are not in `shell/checksums/` (`--allow-unverified`).
    pub fn install(bin: &Path) -> Result<Self> {
        let lock = TEAM_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let rc = system_zshrc();
        let Some(raw) = read_root_file(&rc)? else {
            bail!(
                "{} does not exist: root's zsh has no system zshrc to hook",
                rc.display()
            );
        };
        let rc_original = clear_stale_wrappers(&rc, raw)?;
        let text = String::from_utf8_lossy(&rc_original);
        if managed_block(&text).is_some() || Path::new(SHARE_DIR).exists() {
            bail!(
                "a cinelog team install exists on this machine ({} has a cinelog block or {SHARE_DIR} exists); \
                 the root scenarios refuse to run over it",
                rc.display()
            );
        }
        let mut team = Self {
            prefix: None,
            rc,
            rc_original,
            rc_modified: false,
            before: snapshot(&RECORDER_DIRS)?,
            _lock: lock,
        };
        let prefix = sudo_stdout(&["mktemp", "-d", PREFIX_TEMPLATE])?.trim().to_owned();
        if !prefix.starts_with(PREFIX_STEM) {
            bail!("mktemp returned {prefix:?}");
        }
        team.prefix = Some(prefix.clone());

        let b = bin.to_string_lossy();
        let module = crate::binary::repo_root();
        let m = module.to_string_lossy();
        sudo(&[
            &b,
            "install",
            "--team",
            "--system-zshrc",
            "--prefix",
            &prefix,
            "--binary",
            &b,
            "--allow-unverified",
            "--module",
            &m,
            "--no-migrate",
        ])?;
        let staged = read_root_file(&Path::new(&prefix).join("etc/zsh/zshrc"))?.context("prefix zshrc")?;
        let block = managed_block(&String::from_utf8_lossy(&staged)).context("no managed block in the prefix zshrc")?;
        if !block.contains(&prefix) {
            bail!("the prefix block does not load the prefix plugin copy:\n{block}");
        }
        for (flag, rel) in [
            ("-x", "usr/local/bin/cinelog"),
            ("-f", "usr/local/share/cinelog/cinelog.plugin.zsh"),
        ] {
            sudo(&["test", flag, &format!("{prefix}/{rel}")]).with_context(|| format!("{rel} in the prefix"))?;
        }

        let hooked = with_wrapper(&String::from_utf8_lossy(&team.rc_original), &prefix, &block);
        team.rc_modified = true;
        write_as_root(&team.rc, hooked.as_bytes())?;
        Ok(team)
    }

    /// The throwaway install prefix.
    pub fn prefix(&self) -> &str {
        self.prefix.as_deref().unwrap_or_default()
    }

    /// The TEAM cast of session `uuid` below `/root/logs/<runner>/` that this install's
    /// recorders created, once it contains a command for which `ready` holds.
    pub fn team_cast(
        &self,
        runner: &str,
        uuid: &str,
        timeout: Duration,
        ready: impl Fn(&crate::Cast) -> bool,
    ) -> Result<crate::Cast> {
        crate::wait_until(&format!("TEAM cast of session {uuid}"), timeout, || {
            team_casts(TEAM_LOGS, runner)
                .ok()?
                .into_iter()
                .filter(|p| !self.before.contains(&p.to_string_lossy().into_owned()))
                .filter_map(|p| load_cast_as_root(&p).ok())
                .find(|c| c.uuid().as_deref() == Some(uuid) && ready(c))
        })
    }

    /// Root processes started from the prefix (TEAM recorders and their helpers).
    fn prefix_processes(&self) -> Vec<u32> {
        let Some(prefix) = &self.prefix else {
            return Vec::new();
        };
        procs::all_pids()
            .into_iter()
            .filter(|&pid| {
                procs::cmdline(pid)
                    .and_then(|a| a.into_iter().next())
                    .is_some_and(|argv0| argv0.starts_with(prefix.as_str()))
            })
            .collect()
    }

    fn kill_prefix_processes(&self) {
        for sig in ["-TERM", "-KILL"] {
            let pids: Vec<String> = self.prefix_processes().iter().map(u32::to_string).collect();
            if pids.is_empty() {
                return;
            }
            let mut args = vec!["kill", sig, "--"];
            args.extend(pids.iter().map(String::as_str));
            let _ = sudo(&args);
            let _ = crate::wait_until("TEAM recorders to exit", Duration::from_secs(3), || {
                self.prefix_processes().is_empty().then_some(())
            });
        }
    }
}

/// Removes `cinelog-it` wrappers left by a crashed run whose prefix is gone; returns the
/// zshrc without them. A wrapper whose prefix still exists belongs to a live run.
fn clear_stale_wrappers(rc: &Path, raw: Vec<u8>) -> Result<Vec<u8>> {
    let text = String::from_utf8_lossy(&raw).into_owned();
    let (clean, prefixes) = strip_wrappers(&text);
    if prefixes.is_empty() && clean == text {
        return Ok(raw);
    }
    if let Some(live) = prefixes.iter().find(|p| Path::new(p).exists()) {
        bail!("{} is hooked by another cinelog-it run (prefix {live})", rc.display());
    }
    write_as_root(rc, clean.as_bytes())?;
    Ok(clean.into_bytes())
}

/// Rewrites `path` in place as root (owner and mode stay).
fn write_as_root(path: &Path, data: &[u8]) -> Result<()> {
    let p = path.to_string_lossy();
    sudo_input(&["sh", "-c", r#"cat > "$1""#, "sh", &p], Some(data)).map(|_| ())
}

impl Drop for TeamInstall {
    fn drop(&mut self) {
        self.kill_prefix_processes();
        if self.rc_modified
            && let Err(e) = write_as_root(&self.rc, &self.rc_original)
        {
            eprintln!("cinelog-it: restoring {} failed: {e:#}", self.rc.display());
        }
        match snapshot(&RECORDER_DIRS) {
            Ok(after) => {
                let roots: Vec<String> = new_roots(&self.before, &after)
                    .into_iter()
                    .filter(|p| RECORDER_DIRS.iter().any(|d| Path::new(p).starts_with(d)))
                    .collect();
                if !roots.is_empty() {
                    let mut args = vec!["rm", "-rf", "--one-file-system", "--"];
                    args.extend(roots.iter().map(String::as_str));
                    if let Err(e) = sudo(&args) {
                        eprintln!("cinelog-it: removing {roots:?} failed: {e:#}");
                    }
                }
            }
            Err(e) => eprintln!("cinelog-it: listing the TEAM directories failed: {e:#}"),
        }
        if let Some(prefix) = self.prefix.as_deref().filter(|p| p.starts_with(PREFIX_STEM))
            && let Err(e) = sudo(&["rm", "-rf", "--one-file-system", "--", prefix])
        {
            eprintln!("cinelog-it: removing {prefix} failed: {e:#}");
        }
    }
}

/// Team casts of `sudo_user` (read through sudo; `/root` is not readable for us).
pub fn team_casts(logs_root: &str, sudo_user: &str) -> Result<Vec<PathBuf>> {
    let dir = format!("{logs_root}/{sudo_user}");
    files_as(None, Path::new(&dir), Some("cast"))
}

/// Regular files below `dir`, listed as root or as `user` (optionally only `*.<ext>`).
pub fn files_as(user: Option<&str>, dir: &Path, ext: Option<&str>) -> Result<Vec<PathBuf>> {
    let d = dir.to_string_lossy();
    let mut args: Vec<&str> = Vec::new();
    if let Some(u) = user {
        args.extend(["-u", u]);
    }
    args.extend(["sh", "-c", r#"[ ! -d "$1" ] || find "$1" -type f"#, "sh", &d]);
    let mut v: Vec<PathBuf> = sudo_stdout(&args)?
        .lines()
        .filter(|l| !l.is_empty())
        .map(PathBuf::from)
        .filter(|p| ext.is_none_or(|e| p.extension().is_some_and(|x| x == e)))
        .collect();
    v.sort();
    Ok(v)
}

/// Contents of `path`, read as root or as `user`.
pub fn read_as(user: Option<&str>, path: &Path) -> Result<Vec<u8>> {
    let p = path.to_string_lossy();
    let mut args: Vec<&str> = Vec::new();
    if let Some(u) = user {
        args.extend(["-u", u]);
    }
    args.extend(["cat", "--", &p]);
    Ok(sudo(&args)?.stdout)
}

pub fn read_as_root(path: &Path) -> Result<Vec<u8>> {
    read_as(None, path)
}

pub fn load_cast_as_root(path: &Path) -> Result<crate::Cast> {
    crate::Cast::parse(path, &read_as_root(path)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    const RC: &str = "# system rc\nexport FOO=1\n";

    fn block(prefix: &str) -> String {
        format!("{BLOCK_BEGIN}\nif (( EUID == 0 )); then source '{prefix}/x.zsh'; fi\n{BLOCK_END}\n")
    }

    #[test]
    fn managed_block_is_cut_out_with_its_markers() {
        let text = format!("{RC}\n{}# after\n", block("/p"));
        assert_eq!(managed_block(&text), Some(block("/p")));
        assert_eq!(managed_block(RC), None);
        let unterminated = format!("{BLOCK_BEGIN}\nx\n{BLOCK_END}");
        assert_eq!(managed_block(&unterminated), Some(format!("{unterminated}\n")));
    }

    #[test]
    fn wrapper_round_trip_restores_the_original_bytes() {
        let prefix = "/tmp/cinelog-it-team.abcdefgh";
        for original in [RC, "no trailing newline", ""] {
            let hooked = with_wrapper(original, prefix, &block(prefix));
            assert!(managed_block(&hooked).is_some());
            let (clean, prefixes) = strip_wrappers(&hooked);
            assert_eq!(prefixes, [prefix]);
            let expected = if original.is_empty() || original.ends_with('\n') {
                original.to_owned()
            } else {
                format!("{original}\n")
            };
            assert_eq!(clean, expected);
        }
        assert_eq!(strip_wrappers(RC), (RC.to_owned(), Vec::new()));
    }

    #[test]
    fn a_real_team_block_is_not_mistaken_for_a_wrapper() {
        let text = format!("{RC}{}", block("/usr/local/share/cinelog"));
        let (clean, prefixes) = strip_wrappers(&text);
        assert_eq!(clean, text);
        assert!(prefixes.is_empty());
        assert!(managed_block(&clean).is_some(), "detected as an existing team install");
    }

    #[test]
    fn new_roots_are_the_topmost_new_paths() {
        let set = |v: &[&str]| v.iter().map(|s| (*s).to_owned()).collect::<BTreeSet<_>>();
        let before = set(&["/root/logs", "/root/logs/alice", "/root/logs/alice/old.cast"]);
        let after = set(&[
            "/root/logs",
            "/root/logs/alice",
            "/root/logs/alice/old.cast",
            "/root/logs/alice/new.cast",
            "/root/logs/bob",
            "/root/logs/bob/2026-11",
            "/root/logs/bob/2026-11/a.cast",
            "/run/cinelog",
            "/run/cinelog/pts-3.json",
        ]);
        assert_eq!(
            new_roots(&before, &after),
            ["/root/logs/alice/new.cast", "/root/logs/bob", "/run/cinelog"]
        );
        assert!(new_roots(&after, &after).is_empty());
    }
}
