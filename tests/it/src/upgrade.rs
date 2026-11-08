//! The previous release for the upgrade scenario (I24): its source tree (plugin, `shell/`,
//! `VERSION`) and its `cinelog` binary.
//!
//! - `CINELOG_IT_UPGRADE_FROM`: the git ref to upgrade from. Unset, the harness tries the
//!   tag `v0.12.0`, then [`DEFAULT_COMMIT`] (the last 0.12.0 commit), and skips the scenario
//!   when the repository has neither (a shallow clone). An explicit ref that does not resolve
//!   is an error.
//! - `CINELOG_IT_OLD_SRC`, `CINELOG_IT_OLD_BIN`: a prepared tree and binary instead.
//!
//! The tree is `git archive`d (no checkout, no ref is touched) to
//! `<target>/cinelog-it/upgrade-from-<hash>/src` and built there with `--locked` into its own
//! target dir, once per test process.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::OnceLock;

use anyhow::{Context, Result, bail};

use crate::binary::repo_root;
use crate::util::is_executable;

/// The M4 commit, the last one of 0.12.0.
pub const DEFAULT_COMMIT: &str = "54ed9918ef5ccdcfe8bdc80242251d13ed493c63";
const DEFAULT_TAG: &str = "v0.12.0";
const FROM_VAR: &str = "CINELOG_IT_UPGRADE_FROM";

#[derive(Debug, Clone)]
pub struct OldRelease {
    /// What the tree came from (a commit hash, or the prepared tree's path).
    pub origin: String,
    pub tree: PathBuf,
    pub bin: PathBuf,
    /// `<bin> --version` without the `cinelog ` prefix.
    pub version: String,
}

/// The previous release; `Ok(None)` when no ref to build it from exists here.
pub fn old_release() -> Result<Option<OldRelease>> {
    static CELL: OnceLock<Result<Option<OldRelease>, String>> = OnceLock::new();
    CELL.get_or_init(|| prepare().map_err(|e| format!("{e:#}")))
        .clone()
        .map_err(anyhow::Error::msg)
}

fn prepare() -> Result<Option<OldRelease>> {
    let env_path = |k: &str| std::env::var_os(k).filter(|v| !v.is_empty()).map(PathBuf::from);
    let (origin, tree, base) = match env_path("CINELOG_IT_OLD_SRC") {
        Some(tree) => (tree.display().to_string(), tree, None),
        None => {
            let Some(hash) = resolve_ref()? else { return Ok(None) };
            let base = target_dir()
                .join("cinelog-it")
                .join(format!("upgrade-from-{}", &hash[..12]));
            let tree = extract(&hash, &base.join("src"))?;
            (hash, tree, Some(base))
        }
    };
    if !tree.join("cinelog.plugin.zsh").is_file() || !tree.join("VERSION").is_file() {
        bail!("{} is not a cinelog source tree", tree.display());
    }
    let bin = match env_path("CINELOG_IT_OLD_BIN") {
        Some(b) if is_executable(&b) => b,
        Some(b) => bail!("CINELOG_IT_OLD_BIN={} is not an executable", b.display()),
        None => {
            let base = base.unwrap_or_else(|| tree.join("target/cinelog-it-old"));
            build(&tree, &base)?
        }
    };
    let out = Command::new(&bin).arg("--version").output()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let version = text
        .trim()
        .strip_prefix("cinelog ")
        .with_context(|| format!("{} --version printed {text:?}", bin.display()))?
        .to_owned();
    Ok(Some(OldRelease {
        origin,
        tree,
        bin,
        version,
    }))
}

/// The commit to upgrade from, or `None` when this clone has none of the defaults.
fn resolve_ref() -> Result<Option<String>> {
    let explicit = std::env::var(FROM_VAR).ok().filter(|v| !v.is_empty());
    let candidates = match &explicit {
        Some(r) => vec![r.clone()],
        None => vec![DEFAULT_TAG.to_owned(), DEFAULT_COMMIT.to_owned()],
    };
    for r in &candidates {
        let out = Command::new("git")
            .arg("-C")
            .arg(repo_root())
            .args(["rev-parse", "--verify", "--quiet"])
            .arg(format!("{r}^{{commit}}"))
            .stderr(Stdio::null())
            .output();
        match out {
            Ok(o) if o.status.success() => {
                let hash = String::from_utf8_lossy(&o.stdout).trim().to_owned();
                if hash.len() >= 12 && hash.bytes().all(|b| b.is_ascii_hexdigit()) {
                    return Ok(Some(hash));
                }
            }
            Ok(_) => {}
            Err(e) if explicit.is_some() => return Err(e).context("running git"),
            Err(_) => return Ok(None),
        }
    }
    match explicit {
        Some(r) => bail!("{FROM_VAR}={r} is not a commit of {}", repo_root().display()),
        None => Ok(None),
    }
}

/// `git archive <hash>` unpacked into `dest` (reused once complete).
fn extract(hash: &str, dest: &Path) -> Result<PathBuf> {
    let done = dest.join(".cinelog-it-complete");
    if std::fs::read_to_string(&done).is_ok_and(|h| h.trim() == hash) {
        return Ok(dest.to_path_buf());
    }
    if dest.exists() {
        std::fs::remove_dir_all(dest)?;
    }
    std::fs::create_dir_all(dest)?;
    let tar = dest.with_extension("tar");
    let st = Command::new("git")
        .arg("-C")
        .arg(repo_root())
        .args(["archive", "--format=tar", "-o"])
        .arg(&tar)
        .arg(hash)
        .status()
        .context("git archive")?;
    if !st.success() {
        bail!("git archive {hash} failed ({st})");
    }
    let st = Command::new("tar").arg("-xf").arg(&tar).arg("-C").arg(dest).status()?;
    std::fs::remove_file(&tar)?;
    if !st.success() {
        bail!("unpacking the archive of {hash} failed ({st})");
    }
    std::fs::write(&done, hash)?;
    Ok(dest.to_path_buf())
}

/// `cargo build -p cinelog` of `tree` into `<base>/build`, copied to `<base>/bin/cinelog`.
fn build(tree: &Path, base: &Path) -> Result<PathBuf> {
    let build_dir = base.join("build");
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let out = Command::new(cargo)
        .current_dir(tree)
        .args([
            "build",
            "--quiet",
            "--locked",
            "-p",
            "cinelog",
            "--bin",
            "cinelog",
            "--target-dir",
        ])
        .arg(&build_dir)
        .env_remove("CARGO_TARGET_DIR")
        .env_remove("CINELOG_WEB_DIST")
        .output()
        .context("running cargo build for the previous release")?;
    if !out.status.success() {
        bail!(
            "building the previous release in {} failed:\n{}",
            tree.display(),
            String::from_utf8_lossy(&out.stderr)
        );
    }
    let dest_dir = base.join("bin");
    std::fs::create_dir_all(&dest_dir)?;
    let dest = dest_dir.join("cinelog");
    let tmp = dest_dir.join(format!(".cinelog.{}", std::process::id()));
    std::fs::copy(build_dir.join("debug/cinelog"), &tmp)?;
    std::fs::rename(&tmp, &dest)?;
    Ok(dest)
}

/// The cargo target dir of the running test binary (as [`crate::binary`] finds it).
fn target_dir() -> PathBuf {
    if let Some(d) = std::env::var_os("CARGO_TARGET_DIR") {
        let d = PathBuf::from(d);
        return if d.is_absolute() { d } else { repo_root().join(d) };
    }
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.ancestors().nth(3).map(Path::to_path_buf))
        .unwrap_or_else(|| repo_root().join("target"))
}

/// The release asset name `shell/fetch.zsh` downloads on this machine.
pub fn release_target() -> Option<&'static str> {
    match std::env::consts::ARCH {
        "x86_64" => Some("x86_64-unknown-linux-musl"),
        "aarch64" => Some("aarch64-unknown-linux-musl"),
        _ => None,
    }
}

/// `/proc/<pid>/exe` as the kernel shows it (` (deleted)` once the file was replaced).
pub fn exe_link(pid: u32) -> Option<String> {
    std::fs::read_link(format!("/proc/{pid}/exe"))
        .ok()
        .map(|p| p.to_string_lossy().into_owned())
}

/// Whether `pid` executes the file at `path` (same inode).
pub fn runs_file(pid: u32, path: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    match (std::fs::metadata(format!("/proc/{pid}/exe")), std::fs::metadata(path)) {
        (Ok(a), Ok(b)) => a.dev() == b.dev() && a.ino() == b.ino(),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn release_target_matches_fetch_zsh() {
        let fetch = std::fs::read_to_string(repo_root().join("shell/fetch.zsh")).unwrap();
        if let Some(t) = release_target() {
            assert!(fetch.contains(t), "{t}");
        }
    }

    #[test]
    fn exe_of_this_process() {
        let me = std::process::id();
        let exe = std::env::current_exe().unwrap();
        assert!(runs_file(me, &exe));
        assert!(exe_link(me).is_some_and(|l| !l.ends_with(" (deleted)")));
        assert!(!runs_file(me, Path::new("/bin/sh")));
    }
}
