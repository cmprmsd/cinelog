//! `cinelog install` for the calling user (PERSONAL scope, §10.6 step 3): dirs and modes,
//! config with settings migration, the `~/.local/bin` symlink, the old viewer stopped and
//! the indexer's user unit (§10.4).

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Result, bail};
use cinelog_core::config::legacy;
use cinelog_core::location;
use cinelog_core::paths::EnvSnapshot;
use cinelog_proto::NestedMode;

use super::fsutil::{self, Outcome, Symlinks, pretty};
use super::{Steps, UserManager, old_server, user_unit_file};
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

/// `packaging/systemd/cinelog.service`; its `ExecStart` runs `%h/.local/bin/cinelog`.
const UNIT: &str = include_str!("../../../../packaging/systemd/cinelog.service");
const UNIT_EXEC: &str = "ExecStart=%h/.local/bin/cinelog ";
/// How long the old viewer gets to exit on SIGTERM.
const OLD_SERVER_GRACE: Duration = Duration::from_secs(2);

pub struct Options {
    pub migrate: bool,
    /// The binary to link; `None` = the running one.
    pub exe: Option<PathBuf>,
    pub manager: UserManager,
    /// `/proc` (old viewer processes and listening sockets).
    pub proc_root: PathBuf,
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
    if ctx.config.record.nested == NestedMode::Record {
        steps.warn(crate::doctor::NESTED_RECORD_WARNING.to_owned());
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
    let (exe, linked) = link_binary(&ctx.env, opts.exe.as_deref(), steps)?;
    let port = old_server::persisted_port(cfg.legacy_port, &p.migrated_port_file());
    stop_old_server(&ctx.env, &opts.proc_root, port, steps);
    install_unit(&ctx.env, (!linked).then_some(exe.as_path()), &opts.manager, steps)?;
    match crate::token::ensure_personal(&ctx) {
        Ok(Some(t)) => steps.done(format!("web login token for {} (shown once): {}", t.name, t.secret)),
        Ok(None) => {}
        Err(e) => steps.warn(format!("web login token: {e:#}")),
    }
    Ok(ctx)
}

/// Stops the old plugin's python viewer and checks that nothing else holds the port it
/// used (now the persisted port of `cinelog serve`).
fn stop_old_server(env: &EnvSnapshot, proc_root: &Path, port: Option<u16>, steps: &mut Steps) {
    let pids = old_server::find(proc_root, env.uid, &env.home);
    if !pids.is_empty() {
        let list = |p: &[i32]| p.iter().map(i32::to_string).collect::<Vec<_>>().join(", ");
        let left = old_server::stop(&pids, OLD_SERVER_GRACE);
        if left.is_empty() {
            steps.done(format!("stopped the old viewer (http_server.py, pid {})", list(&pids)));
            steps.note("shells still running the old plugin start it again until .zimrc loads the new one");
        } else {
            steps.warn(format!(
                "could not stop the old viewer (http_server.py, pid {})",
                list(&left)
            ));
        }
    }
    if let Some(port) = port
        && old_server::port_listened(proc_root, port)
    {
        steps.warn(format!(
            "something still listens on port {port}, the viewer port carried over from the old install"
        ));
    }
}

/// The user unit, with `ExecStart` pointing at `exec` when `~/.local/bin/cinelog` is not
/// the binary (systemd quoting: `%` and `$` doubled, the path in double quotes).
fn render_unit(exec: Option<&Path>) -> String {
    let Some(exec) = exec else { return UNIT.to_owned() };
    let path = exec
        .to_string_lossy()
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('%', "%%")
        .replace('$', "$$");
    UNIT.replacen(UNIT_EXEC, &format!("ExecStart=\"{path}\" "), 1)
}

/// Writes the user unit and, when the user manager reads it, enables and starts it.
fn install_unit(env: &EnvSnapshot, exec: Option<&Path>, manager: &UserManager, steps: &mut Steps) -> Result<()> {
    let unit = user_unit_file(env);
    let dir = unit.parent().expect("the unit file has a directory");
    fsutil::ensure_dir_with(dir, 0o755, Symlinks::FollowOwnedBy(env.euid))?;
    let text = render_unit(exec);
    let changed = std::fs::read_to_string(&unit).ok().as_deref() != Some(text.as_str());
    let outcome = if changed {
        fsutil::write_atomic(&unit, text.as_bytes(), 0o644)?
    } else {
        Outcome::Unchanged
    };
    steps.file(&outcome, &unit, "(indexer and web server)", &env.home);
    if !manager.reads(&unit) {
        steps.note(format!(
            "the systemd user manager does not read {}: not enabled (recorded shells start `cinelog serve` themselves)",
            pretty(dir, &env.home)
        ));
        return Ok(());
    }
    if changed {
        manager.systemctl(&["--user", "daemon-reload"], steps);
        manager.systemctl(&["--user", "try-restart", "cinelog.service"], steps);
    }
    manager.systemctl(&["--user", "enable", "--now", "cinelog.service"], steps);
    if !manager.linger {
        steps.note("`loginctl enable-linger` keeps indexing while you are logged out");
    }
    Ok(())
}

/// Links `~/.local/bin/cinelog` to `exe` (default: the running binary). Returns the binary
/// and whether the link points at it.
fn link_binary(env: &EnvSnapshot, exe: Option<&Path>, steps: &mut Steps) -> Result<(PathBuf, bool)> {
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
        return Ok((exe, false));
    }
    let outcome = fsutil::ensure_symlink(&link, &exe)?;
    steps.file(&outcome, &link, &format!("→ {}", exe.display()), &env.home);
    if !std::env::var_os("PATH").is_some_and(|p| std::env::split_paths(&p).any(|d| d == dir)) {
        steps.note(format!("{} is not in PATH", pretty(&dir, &env.home)));
    }
    Ok((exe, true))
}

/// `install --refresh` (run by `fetch.zsh` after an update): the symlink, and a restart of
/// the user unit if it is installed.
pub fn refresh(env: EnvSnapshot, manager: &UserManager, steps: &mut Steps) -> Result<()> {
    refuse_root(&env)?;
    link_binary(&env, None, steps)?;
    let unit = user_unit_file(&env);
    if unit.exists() && manager.reads(&unit) {
        manager.systemctl(&["--user", "try-restart", "cinelog.service"], steps);
    }
    Ok(())
}

/// `install --uninstall`: removes the symlink and the user unit. Casts, config and state
/// stay.
pub fn uninstall(env: EnvSnapshot, manager: &UserManager, steps: &mut Steps) -> Result<()> {
    refuse_root(&env)?;
    let link = local_bin(&env);
    let points_to_cinelog = std::fs::read_link(&link).is_ok_and(|t| t.file_name().is_some_and(|n| n == "cinelog"));
    if points_to_cinelog && fsutil::remove_file(&link)? {
        steps.done(format!("removed {}", pretty(&link, &env.home)));
    }
    let unit = user_unit_file(&env);
    if unit.exists() {
        if manager.reads(&unit) {
            manager.systemctl(&["--user", "disable", "--now", "cinelog.service"], steps);
        }
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

    /// A user manager that reads none of the sandbox's paths: `systemctl` must not run.
    fn no_manager() -> UserManager {
        UserManager {
            unit_dir: None,
            run: |args, _| panic!("systemctl {args:?} in a test"),
            linger: true,
        }
    }

    /// Options for a sandbox install: no user manager, an empty `/proc`.
    fn test_options(dir: &Path) -> Options {
        let proc_root = dir.join("proc");
        std::fs::create_dir_all(&proc_root).unwrap();
        Options {
            migrate: true,
            exe: None,
            manager: no_manager(),
            proc_root,
        }
    }

    #[test]
    fn the_user_unit_is_enabled_only_where_the_manager_reads_it() {
        let d = tempfile::tempdir().unwrap();
        let env = test_env(d.path());
        let unit = user_unit_file(&env);
        let exe = d.path().join("module/bin/cinelog");
        let opts = Options {
            exe: Some(exe.clone()),
            manager: UserManager {
                unit_dir: Some(unit.parent().unwrap().to_path_buf()),
                run: |args, steps| steps.done(format!("ran systemctl {}", args.join(" "))),
                linger: false,
            },
            ..test_options(d.path())
        };
        let mut steps = Steps::default();
        install(env.clone(), &opts, &mut steps).unwrap();
        let text = steps.to_string();
        assert_eq!(
            std::fs::read_to_string(&unit).unwrap(),
            UNIT,
            "linked: %h/.local/bin/cinelog"
        );
        for cmd in [
            "daemon-reload",
            "try-restart cinelog.service",
            "enable --now cinelog.service",
        ] {
            assert!(text.contains(&format!("ran systemctl --user {cmd}")), "{text}");
        }
        assert!(text.contains("enable-linger"), "{text}");

        let mut again = Steps::default();
        install(env.clone(), &opts, &mut again).unwrap();
        let text = again.to_string();
        assert!(
            !text.contains("daemon-reload") && text.contains("enable --now"),
            "{text}"
        );

        // Not where the manager looks (a sandbox HOME): written, never enabled.
        let mut other = Steps::default();
        std::fs::remove_file(&unit).unwrap();
        install(env.clone(), &test_options(d.path()), &mut other).unwrap();
        assert!(unit.exists());
        assert!(other.to_string().contains("not enabled"), "{other}");

        let mut un = Steps::default();
        uninstall(env.clone(), &opts.manager, &mut un).unwrap();
        assert!(
            un.to_string()
                .contains("ran systemctl --user disable --now cinelog.service"),
            "{un}"
        );
        assert!(!unit.exists());
    }

    #[test]
    fn a_binary_outside_local_bin_is_quoted_into_exec_start() {
        let unit = render_unit(Some(Path::new("/opt/my tools/100%/cinelog")));
        assert!(
            unit.contains("ExecStart=\"/opt/my tools/100%%/cinelog\" serve --scope personal\n"),
            "{unit}"
        );
        assert_eq!(unit.matches("ExecStart=").count(), 1);
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
            ..test_options(d.path())
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
        uninstall(env.clone(), &no_manager(), &mut un).unwrap();

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
            ..test_options(d.path())
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
    fn nested_record_is_warned_about() {
        let d = tempfile::tempdir().unwrap();
        let env = test_env(d.path());
        let cfg = env.config_home().join("cinelog/config.toml");
        std::fs::create_dir_all(cfg.parent().unwrap()).unwrap();
        std::fs::write(&cfg, "[record]\nnested = \"record\"\n").unwrap();
        let opts = Options {
            migrate: false,
            exe: Some(d.path().join("module/bin/cinelog")),
            ..test_options(d.path())
        };
        let mut steps = Steps::default();
        install(env.clone(), &opts, &mut steps).unwrap();
        assert!(steps.to_string().contains("! record.nested = \"record\""), "{steps}");

        std::fs::write(&cfg, "[record]\nnested = \"pause\"\n").unwrap();
        let mut steps = Steps::default();
        install(env, &opts, &mut steps).unwrap();
        assert!(!steps.to_string().contains("nested"), "{steps}");
    }

    #[test]
    fn symlinked_state_inside_a_syncthing_folder_is_refused() {
        let d = tempfile::tempdir().unwrap();
        let env = test_env(d.path());
        let synced = d.path().join("Sync");
        std::fs::create_dir_all(synced.join(".stfolder")).unwrap();
        std::fs::create_dir_all(synced.join("state")).unwrap();
        // test_env puts state in <d>/state; make that a link into the synced tree.
        std::os::unix::fs::symlink(synced.join("state"), d.path().join("state")).unwrap();
        let opts = Options {
            migrate: false,
            exe: Some(d.path().join("module/bin/cinelog")),
            ..test_options(d.path())
        };
        let err = install(env, &opts, &mut Steps::default()).unwrap_err();
        assert!(err.to_string().contains("Syncthing"), "{err}");
        assert!(!synced.join("state/cinelog").exists());
    }

    #[test]
    fn refuses_root_and_synced_state() {
        let d = tempfile::tempdir().unwrap();
        let mut env = test_env(d.path());
        env.euid = 0;
        let opts = Options {
            migrate: false,
            exe: Some("/x".into()),
            ..test_options(d.path())
        };
        assert!(install(env, &opts, &mut Steps::default()).is_err());

        let env = test_env(d.path());
        std::fs::create_dir_all(d.path().join("state/.stfolder")).unwrap();
        let err = install(env, &opts, &mut Steps::default()).unwrap_err();
        assert!(err.to_string().contains("Syncthing"), "{err}");
    }
}
