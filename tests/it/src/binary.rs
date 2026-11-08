//! The `cinelog` binaries under test.
//!
//! `CINELOG_IT_BIN` / `CINELOG_IT_BIN_TESTHOOKS` point at prebuilt binaries (CI builds them
//! once). Otherwise the harness builds `-p cinelog` (and `--features testhooks`) once per
//! test process into `<target>/cinelog-it/build` with the cargo that runs the tests; that
//! separate target dir never contends with the outer `cargo test` lock. Each flavour is
//! copied to `<target>/cinelog-it/bin/<flavour>/cinelog`: the file must be named `cinelog`,
//! because detection matches the recorder by comm.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

use anyhow::{Context, Result, bail};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flavour {
    Plain,
    /// Built with `--features testhooks` (clock injection, I16).
    Testhooks,
}

impl Flavour {
    fn env_var(self) -> &'static str {
        match self {
            Self::Plain => "CINELOG_IT_BIN",
            Self::Testhooks => "CINELOG_IT_BIN_TESTHOOKS",
        }
    }

    fn dir_name(self) -> &'static str {
        match self {
            Self::Plain => "plain",
            Self::Testhooks => "testhooks",
        }
    }
}

/// The repository root (this crate lives in `tests/it`).
pub fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("tests/it is two levels below the repo root")
        .to_path_buf()
}

/// The cargo target dir the running test binary was built into.
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

/// Path of the `cinelog` binary of `flavour`, building it on first use.
pub fn cinelog(flavour: Flavour) -> Result<PathBuf> {
    static PLAIN: OnceLock<Result<PathBuf, String>> = OnceLock::new();
    static HOOKS: OnceLock<Result<PathBuf, String>> = OnceLock::new();
    let cell = match flavour {
        Flavour::Plain => &PLAIN,
        Flavour::Testhooks => &HOOKS,
    };
    cell.get_or_init(|| locate(flavour).map_err(|e| format!("{e:#}")))
        .clone()
        .map_err(anyhow::Error::msg)
}

fn locate(flavour: Flavour) -> Result<PathBuf> {
    if let Some(p) = std::env::var_os(flavour.env_var()) {
        let p = PathBuf::from(p);
        if !crate::util::is_executable(&p) {
            bail!("{}={} is not an executable", flavour.env_var(), p.display());
        }
        return Ok(p);
    }
    build(flavour)
}

fn build(flavour: Flavour) -> Result<PathBuf> {
    // Builds of the two flavours share one target dir, so serialise them in-process.
    static BUILD: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _guard = BUILD.lock().unwrap_or_else(|e| e.into_inner());
    let base = target_dir().join("cinelog-it");
    let build_dir = base.join("build");
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let mut cmd = Command::new(cargo);
    cmd.current_dir(repo_root())
        .args(["build", "--quiet", "-p", "cinelog", "--bin", "cinelog", "--target-dir"])
        .arg(&build_dir)
        .env_remove("CARGO_TARGET_DIR");
    if flavour == Flavour::Testhooks {
        cmd.args(["--features", "testhooks"]);
    }
    let out = cmd.output().context("running cargo build -p cinelog")?;
    if !out.status.success() {
        bail!(
            "cargo build -p cinelog ({:?}) failed:\n{}",
            flavour,
            String::from_utf8_lossy(&out.stderr)
        );
    }
    let built = build_dir.join("debug/cinelog");
    let dest_dir = base.join("bin").join(flavour.dir_name());
    std::fs::create_dir_all(&dest_dir)?;
    let dest = dest_dir.join("cinelog");
    // Copy + rename, so a binary that is still executing from an earlier run is never
    // overwritten in place (ETXTBSY).
    let tmp = dest_dir.join(format!(".cinelog.{}", std::process::id()));
    std::fs::copy(&built, &tmp).with_context(|| format!("copying {}", built.display()))?;
    std::fs::rename(&tmp, &dest)?;
    Ok(dest)
}

/// The plugin entry file of the repo.
pub fn plugin_file() -> PathBuf {
    repo_root().join("cinelog.plugin.zsh")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repo_layout() {
        let root = repo_root();
        assert!(root.join("Cargo.toml").is_file());
        assert!(root.join("tests/it/Cargo.toml").is_file());
        assert!(target_dir().is_absolute());
    }
}
