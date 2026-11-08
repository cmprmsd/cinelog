//! `cinelog install` for the calling user (PERSONAL scope, §10.6 step 3, M1 part): dirs and
//! modes, config with settings migration, the `~/.local/bin` symlink. No server yet.

use std::path::{Path, PathBuf};

use anyhow::{Result, bail};
use cinelog_core::config::legacy;
use cinelog_core::location;
use cinelog_core::paths::EnvSnapshot;

use super::fsutil::{self, Outcome, Symlinks, pretty};
use super::{Steps, user_unit_file};
use crate::ctx::Ctx;
use crate::module;

pub fn local_bin(env: &EnvSnapshot) -> PathBuf {
    env.home.join(".local/bin/cinelog")
}

fn refuse_root(env: &EnvSnapshot) -> Result<()> {
    if env.euid == 0 {
        bail!(
            "run `cinelog install` as your user; the system-wide install is `{}`",
            module::sudo_install_team("")
        );
    }
    Ok(())
}

pub struct Options {
    pub migrate: bool,
    /// The binary to link; `None` = the running one.
    pub exe: Option<PathBuf>,
}

pub fn install(env: EnvSnapshot, opts: &Options, steps: &mut Steps) -> Result<Ctx> {
    refuse_root(&env)?;
    let home = env.home.clone();
    let config_file = env.config_home().join("cinelog/config.toml");
    let legacy_file = opts.migrate.then(|| legacy::path_in_home(&home));
    let follow = Symlinks::FollowOwnedBy(env.euid);
    let cfg = super::migrate::ensure_config(
        cinelog_core::Scope::Personal,
        &config_file,
        legacy_file.as_deref(),
        follow,
    )?;
    steps.notes(cfg.notes);

    // Paths depend on [paths] of the (possibly new) config.
    let ctx = Ctx::with(env, cinelog_core::SystemRoot::real());
    if let Some(e) = &ctx.config_error {
        bail!("{e}");
    }
    let p = &ctx.paths;
    if let Some(problem) = location::db_location_problem(&p.db_dir) {
        bail!("{problem}");
    }
    for (dir, what) in [
        (&p.logs_root, "casts"),
        (&p.state_dir, "state"),
        (&p.db_dir, "databases"),
        (&p.runtime_dir, "registry"),
    ] {
        steps.dir(&fsutil::ensure_dir_with(dir, 0o700, follow)?, dir, what, &home);
    }
    if let Some(s) = location::synced_ancestor(&p.logs_root) {
        steps.warn(format!(
            "{} is inside the Syncthing folder {}: casts (which may contain secrets) leave this machine",
            pretty(&p.logs_root, &home),
            s.display()
        ));
    } else if let Some(fs) = location::network_fs(&p.logs_root) {
        steps.warn(format!("{} is on a {fs} filesystem", pretty(&p.logs_root, &home)));
    }
    if let Some(port) = cfg.legacy_port {
        let file = p.migrated_port_file();
        if fsutil::write_new(&file, format!("{port}\n").as_bytes(), 0o600)? == Outcome::Created {
            steps.note(format!("kept the old viewer port {port} for the future server"));
        }
    }
    link_binary(&ctx.env, opts.exe.as_deref(), steps)?;
    Ok(ctx)
}

fn link_binary(env: &EnvSnapshot, exe: Option<&Path>, steps: &mut Steps) -> Result<()> {
    let exe = match exe {
        Some(e) => e.to_path_buf(),
        None => module::current_exe()?,
    };
    let link = local_bin(env);
    let dir = env.home.join(".local/bin");
    fsutil::ensure_dir_with(&dir, 0o755, Symlinks::FollowOwnedBy(env.euid))?;
    if std::fs::symlink_metadata(&link).is_ok_and(|m| !m.file_type().is_symlink()) {
        steps.warn(format!(
            "{} is a regular file, not replaced: remove it so it links to {}",
            pretty(&link, &env.home),
            exe.display()
        ));
        return Ok(());
    }
    let outcome = fsutil::ensure_symlink(&link, &exe)?;
    steps.file(&outcome, &link, &format!("→ {}", exe.display()), &env.home);
    if !std::env::var_os("PATH").is_some_and(|p| std::env::split_paths(&p).any(|d| d == dir)) {
        steps.note(format!("{} is not in PATH", pretty(&dir, &env.home)));
    }
    Ok(())
}

/// `install --refresh` (run by `fetch.zsh` after an update): the symlink, and a restart of
/// the user unit if it is installed.
pub fn refresh(env: EnvSnapshot, steps: &mut Steps) -> Result<()> {
    refuse_root(&env)?;
    link_binary(&env, None, steps)?;
    if user_unit_file(&env).exists() {
        super::systemctl(&["--user", "try-restart", "cinelog.service"], steps);
    }
    Ok(())
}

/// `install --uninstall`: removes the symlink and the user unit. Casts, config and state
/// stay.
pub fn uninstall(env: EnvSnapshot, steps: &mut Steps) -> Result<()> {
    refuse_root(&env)?;
    let link = local_bin(&env);
    let points_to_cinelog = std::fs::read_link(&link).is_ok_and(|t| t.file_name().is_some_and(|n| n == "cinelog"));
    if points_to_cinelog && fsutil::remove_file(&link)? {
        steps.done(format!("removed {}", pretty(&link, &env.home)));
    }
    let unit = user_unit_file(&env);
    if unit.exists() {
        super::systemctl(&["--user", "disable", "--now", "cinelog.service"], steps);
        fsutil::remove_file(&unit)?;
        steps.done(format!("removed {}", pretty(&unit, &env.home)));
    }
    let ctx = Ctx::with(env, cinelog_core::SystemRoot::real());
    steps.note(format!(
        "kept casts in {}, config in {} and state in {}",
        pretty(&ctx.paths.logs_root, &ctx.env.home),
        pretty(&ctx.paths.config_file, &ctx.env.home),
        pretty(&ctx.paths.state_dir, &ctx.env.home)
    ));
    steps.note("to stop recording new shells, remove the cinelog zmodule line from .zimrc".to_owned());
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;
    use crate::check::tests::test_env;

    fn mode(p: &Path) -> u32 {
        std::fs::metadata(p).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn fresh_install_with_migration() {
        let d = tempfile::tempdir().unwrap();
        let env = test_env(d.path());
        let old = env.home.join(".config/cinelog/cinelog-settings.conf");
        std::fs::create_dir_all(old.parent().unwrap()).unwrap();
        std::fs::write(&old, "export CINELOG_VIEWER_PORT=11000\nGET_EXTERNAL_IP=1\n").unwrap();
        std::fs::create_dir_all(env.home.join("logs")).unwrap();
        std::fs::set_permissions(env.home.join("logs"), std::fs::Permissions::from_mode(0o755)).unwrap();

        let fake_exe = d.path().join("module/bin/cinelog");
        let mut steps = Steps::default();
        let opts = Options {
            migrate: true,
            exe: Some(fake_exe.clone()),
        };
        let ctx = install(env.clone(), &opts, &mut steps).unwrap();
        assert!(ctx.config.shell.external_ip);
        assert_eq!(mode(&env.home.join("logs")), 0o700, "existing logs dir tightened");
        assert_eq!(mode(&ctx.paths.config_file), 0o600);
        assert_eq!(mode(&ctx.paths.db_dir), 0o700);
        assert_eq!(mode(&d.path().join("run/cinelog")), 0o700);
        assert_eq!(std::fs::read_link(local_bin(&env)).unwrap(), fake_exe);
        assert_eq!(
            std::fs::read_to_string(ctx.paths.migrated_port_file()).unwrap(),
            "11000\n"
        );
        assert!(old.exists(), "the old settings stay in place");
        let text = steps.to_string();
        assert!(text.contains("tightened"), "{text}");

        // Idempotent.
        let mut again = Steps::default();
        install(env.clone(), &opts, &mut again).unwrap();
        assert!(again.to_string().contains("kept"), "{again}");

        let mut un = Steps::default();
        uninstall(env.clone(), &mut un).unwrap();

        // A regular file in the way is reported, never overwritten.
        std::fs::write(local_bin(&env), b"mine").unwrap();
        let mut steps = Steps::default();
        install(env.clone(), &opts, &mut steps).unwrap();
        assert!(steps.to_string().contains("regular file"));
        assert_eq!(std::fs::read(local_bin(&env)).unwrap(), b"mine");
        std::fs::remove_file(local_bin(&env)).unwrap();
        assert!(std::fs::symlink_metadata(local_bin(&env)).is_err());
        assert!(ctx.paths.config_file.exists() && env.home.join("logs").exists());
    }

    #[test]
    fn symlinked_user_dirs_are_followed() {
        let d = tempfile::tempdir().unwrap();
        let mut env = test_env(d.path());
        env.euid = nix::unistd::geteuid().as_raw();
        let disk = d.path().join("data-disk");
        for sub in ["logs", "dotconfig", "bin"] {
            std::fs::create_dir_all(disk.join(sub)).unwrap();
        }
        std::fs::set_permissions(disk.join("logs"), std::fs::Permissions::from_mode(0o775)).unwrap();
        std::fs::create_dir_all(env.home.join(".local")).unwrap();
        std::fs::create_dir_all(d.path().join("config")).unwrap();
        std::os::unix::fs::symlink(disk.join("logs"), env.home.join("logs")).unwrap();
        std::os::unix::fs::symlink(disk.join("dotconfig"), d.path().join("config/cinelog")).unwrap();
        std::os::unix::fs::symlink(disk.join("bin"), env.home.join(".local/bin")).unwrap();

        let opts = Options {
            migrate: false,
            exe: Some(d.path().join("module/bin/cinelog")),
        };
        let mut steps = Steps::default();
        install(env.clone(), &opts, &mut steps).unwrap();
        assert_eq!(mode(&disk.join("logs")), 0o700, "the link target is tightened: {steps}");
        assert!(
            std::fs::symlink_metadata(env.home.join("logs"))
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert!(disk.join("dotconfig/config.toml").is_file());
        assert!(std::fs::read_link(disk.join("bin/cinelog")).is_ok());
    }

    #[test]
    fn refuses_root_and_synced_state() {
        let d = tempfile::tempdir().unwrap();
        let mut env = test_env(d.path());
        env.euid = 0;
        let opts = Options {
            migrate: false,
            exe: Some("/x".into()),
        };
        assert!(install(env, &opts, &mut Steps::default()).is_err());

        let env = test_env(d.path());
        std::fs::create_dir_all(d.path().join("state/.stfolder")).unwrap();
        let err = install(env, &opts, &mut Steps::default()).unwrap_err();
        assert!(err.to_string().contains("Syncthing"), "{err}");
    }
}
