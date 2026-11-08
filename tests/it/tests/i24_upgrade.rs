//! I24 (M5 accept 2): upgrade from the previous release (0.12) to this build, as a user
//! gets it: `zimfw update` pulls the new plugin, zim's `--on-pull 'zsh shell/fetch.zsh'`
//! swaps `bin/cinelog`, fetch.zsh runs `install --refresh`. Asserts:
//!
//! - the swap is atomic and verified against `shell/checksums/` of the pulled module;
//! - a shell recorded by the old binary keeps recording on its `(deleted)` executable and
//!   exits cleanly; new shells run the new recorder;
//! - before the refresh, `doctor --skew` names the old running server; `install --refresh`
//!   restarts that detached server on the new binary, which opens the old databases and
//!   keeps indexing (both releases' commands are searchable);
//! - `install --team --upgrade` in `--root` test mode replaces a 0.12 team install (binary
//!   and plugin copy) and needs `--upgrade` to do so, while a process running the old team
//!   binary keeps running.
//!
//! The previous release comes from `src/upgrade.rs` (built from git; skipped in a clone
//! without that commit). Root-only parts (systemd restart of the team unit) are manual.

use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

use cinelog_it::upgrade::{self, OldRelease};
use cinelog_it::{Need, Recorded, Sandbox, SandboxOptions, binary, need, scaled, unique_id, wait_until};

fn copy_tree(from: &Path, to: &Path) {
    fs::create_dir_all(to).unwrap();
    for e in fs::read_dir(from).unwrap().flatten() {
        let dest = to.join(e.file_name());
        let ft = e.file_type().unwrap();
        if ft.is_dir() {
            copy_tree(&e.path(), &dest);
        } else if ft.is_file() {
            fs::copy(e.path(), &dest).unwrap();
        }
    }
}

/// Replaces the plugin files of `module` by those of `tree`, as `git pull` does; `bin/`
/// stays (it is not tracked).
fn checkout(tree: &Path, module: &Path) {
    let shell = module.join("shell");
    if shell.exists() {
        fs::remove_dir_all(&shell).unwrap();
    }
    copy_tree(&tree.join("shell"), &shell);
    for f in ["cinelog.plugin.zsh", "VERSION"] {
        fs::copy(tree.join(f), module.join(f)).unwrap();
    }
}

fn install_file(src: &Path, dest: &Path) {
    let tmp = dest.with_extension("tmp");
    fs::copy(src, &tmp).unwrap();
    fs::set_permissions(&tmp, fs::Permissions::from_mode(0o755)).unwrap();
    fs::rename(&tmp, dest).unwrap();
}

fn sha256_hex(path: &Path) -> String {
    cinelog_it::util::sha256_file(path).unwrap()
}

fn version_of(bin: &Path) -> String {
    let out = Command::new(bin).arg("--version").output().unwrap();
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

/// Runs `zsh <module>/shell/fetch.zsh <args>` in the module, as zim's `--on-pull` does.
fn on_pull(sb: &Sandbox, module: &Path, args: &[&str]) -> (bool, String) {
    let out = Command::new(&sb.zsh)
        .env_clear()
        .envs(sb.env())
        .current_dir(module)
        .arg(module.join("shell/fetch.zsh"))
        .args(args)
        .stdin(Stdio::null())
        .output()
        .unwrap();
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    (out.status.success(), text)
}

fn lock_holder(sb: &Sandbox) -> Option<u32> {
    let pid: u32 = fs::read_to_string(sb.registry_dir().join("serve.lock"))
        .ok()?
        .trim()
        .parse()
        .ok()?;
    let lock = fs::metadata(sb.registry_dir().join("serve.lock")).ok()?;
    let locks = fs::read_to_string("/proc/locks").ok()?;
    let id = format!(
        "{:02x}:{:02x}:{}",
        libc::major(lock.dev()),
        libc::minor(lock.dev()),
        lock.ino()
    );
    locks
        .lines()
        .any(|l| {
            let f: Vec<&str> = l.split_whitespace().collect();
            f.len() > 5 && f[1] == "FLOCK" && f[3] == "WRITE" && f[4] == pid.to_string() && f[5] == id
        })
        .then_some(pid)
}

fn server_json_version(sb: &Sandbox) -> Option<(u32, String)> {
    let v: serde_json::Value = serde_json::from_slice(&fs::read(sb.registry_dir().join("server.json")).ok()?).ok()?;
    Some((v["pid"].as_u64()? as u32, v["version"].as_str()?.to_owned()))
}

fn skew(sb: &Sandbox) -> String {
    let out = sb.cinelog(&["doctor", "--skew"]).unwrap();
    assert_eq!(out.code, Some(0), "doctor --skew always exits 0: {out}");
    assert!(!out.stdout.contains('\x1b'), "no ANSI: {out}");
    out.stdout
}

#[test]
fn i24_upgrade_from_the_previous_release() {
    need!(Need::Zsh, Need::Tmux, Need::Plugin, Need::Recorder, Need::Index);
    let Some(old) = upgrade::old_release().unwrap() else {
        skip(&format!(
            "neither tag v0.12.0 nor commit {} is in this clone (set CINELOG_IT_UPGRADE_FROM)",
            upgrade::DEFAULT_COMMIT
        ));
        return;
    };
    let Some(target) = upgrade::release_target() else {
        skip(&format!("no release asset for {}", std::env::consts::ARCH));
        return;
    };
    let new_bin = binary::cinelog(binary::Flavour::Plain).unwrap();
    let new_version = version_of(&new_bin).strip_prefix("cinelog ").unwrap().to_owned();
    assert!(
        cinelog_it_older(&old.version, &new_version),
        "{} is not older than {new_version}",
        old.version
    );

    let mut sb = Sandbox::with(SandboxOptions {
        flavour: None,
        autostart: "shell",
        ..SandboxOptions::default()
    })
    .unwrap();
    let module = sb.plugin_dir.clone();
    let bin = module.join("bin/cinelog");
    checkout(&old.tree, &module);
    install_file(&old.bin, &bin);
    symlink(&bin, sb.home.join(".local/bin/cinelog")).unwrap();
    sb.bin = Some(bin.clone());
    assert_eq!(version_of(&bin), format!("cinelog {}", old.version));

    personal_upgrade(&sb, &old, &module, &bin, &new_bin, &new_version, target);
    team_upgrade(&sb, &old, &module, &new_version);
}

/// Skips the scenario, or fails it under `CINELOG_IT_REQUIRE=1`: CI must never report the
/// only end-to-end upgrade check as passed without running it.
fn skip(why: &str) {
    assert!(
        !cinelog_it::gate::strict(),
        "i24 cannot run (CINELOG_IT_REQUIRE=1): {why}"
    );
    eprintln!("SKIP i24: {why}");
}

fn cinelog_it_older(a: &str, b: &str) -> bool {
    let p = |v: &str| -> Vec<u64> {
        v.split(['-', '+'])
            .next()
            .unwrap()
            .split('.')
            .map(|x| x.parse().unwrap())
            .collect()
    };
    p(a) < p(b)
}

fn personal_upgrade(
    sb: &Sandbox,
    old: &OldRelease,
    module: &Path,
    bin: &Path,
    new_bin: &Path,
    new_version: &str,
    target: &str,
) {
    let id = unique_id();
    let t = sb.tmux().unwrap();

    // 0.12: a recorded shell, which autostarts the 0.12 server.
    let a = sb.start_recorded(&t, "old").unwrap();
    assert!(upgrade::runs_file(a.pane.pid, bin), "the old recorder runs bin/cinelog");
    let before = format!("echo before-{id}");
    a.run(&before).unwrap();
    let old_server = wait_until("the 0.12 server holds serve.lock", scaled(20.0), || lock_holder(sb)).unwrap();
    assert!(upgrade::runs_file(old_server, bin));
    wait_until("the 0.12 server's server.json", scaled(20.0), || {
        server_json_version(sb).filter(|(pid, v)| *pid == old_server && *v == old.version)
    })
    .unwrap();
    sb.wait_hit(&[&format!("before-{id}")], scaled(20.0), |h| {
        h["typed"] == before.as_str()
    })
    .unwrap();

    // `git pull`: the new plugin, VERSION and the checksum the release workflow committed.
    checkout(&binary::repo_root(), module);
    let rel = sb.work.join("release");
    let asset_dir = rel.join(format!("v{new_version}"));
    fs::create_dir_all(&asset_dir).unwrap();
    let asset = asset_dir.join(format!("cinelog-{target}"));
    install_file(new_bin, &asset);
    let sums = format!("{}  cinelog-{target}\n", sha256_hex(&asset));
    fs::create_dir_all(module.join("shell/checksums")).unwrap();
    fs::write(module.join(format!("shell/checksums/v{new_version}.sha256")), &sums).unwrap();

    // A tampered asset is refused and the old binary stays.
    fs::write(&asset, b"#!/bin/sh\necho tampered\n").unwrap();
    let base = format!("file://{}", rel.display());
    let (ok, text) = on_pull(sb, module, &["--base-url", &base, "--no-cargo", "--no-refresh"]);
    assert!(!ok && text.contains("checksum mismatch"), "{text}");
    assert_eq!(version_of(bin), format!("cinelog {}", old.version));
    install_file(new_bin, &asset);

    // `--on-pull`: verified download, atomic swap; the refresh is deferred here so the skew
    // warning can be seen first.
    let ino = fs::metadata(bin).unwrap().ino();
    let (ok, text) = on_pull(sb, module, &["--base-url", &base, "--no-cargo", "--no-refresh"]);
    assert!(ok, "{text}");
    assert!(!text.contains("warning"), "verified against the repo file: {text}");
    assert_eq!(version_of(bin), format!("cinelog {new_version}"));
    assert_ne!(fs::metadata(bin).unwrap().ino(), ino, "replaced by rename");
    assert!(!module.join("bin/cinelog.new").exists());
    for (what, pid) in [("recorder", a.pane.pid), ("server", old_server)] {
        let link = upgrade::exe_link(pid).unwrap_or_default();
        assert!(
            link.ends_with(" (deleted)"),
            "the old {what} runs its (deleted) exe: {link}"
        );
    }

    let warnings = skew(sb);
    assert!(
        warnings.lines().any(|l| l.starts_with("cinelog: the running server")
            && l.contains(&format!("is {}, older than {new_version}", old.version))
            && l.contains("cinelog install --refresh")),
        "{warnings}"
    );
    assert!(!warnings.contains("plugin"), "VERSION and binary agree: {warnings}");

    // The next pull finds the binary current and runs the deferred `install --refresh`.
    let (ok, text) = on_pull(sb, module, &["--base-url", &base, "--no-cargo"]);
    assert!(ok && text.contains("up to date"), "{text}");
    assert!(
        text.contains(&format!("restarted the server (pid {old_server} → ")),
        "install --refresh restarts the detached server: {text}"
    );
    let new_server = wait_until("a new server holds serve.lock", scaled(20.0), || {
        lock_holder(sb).filter(|p| *p != old_server)
    })
    .unwrap();
    assert!(upgrade::runs_file(new_server, bin), "restarted on the new binary");
    assert!(
        cinelog_it::procs::wait_gone(old_server, scaled(10.0)),
        "the old server stopped: {}",
        upgrade::exe_link(old_server).unwrap_or_default()
    );
    wait_until("the new server's server.json", scaled(20.0), || {
        server_json_version(sb).filter(|(pid, v)| *pid == new_server && v == new_version)
    })
    .unwrap();
    assert_eq!(
        fs::read_link(sb.home.join(".local/bin/cinelog")).unwrap(),
        bin,
        "the ~/.local/bin link"
    );
    let warnings = skew(sb);
    assert!(!warnings.contains("running server"), "{warnings}");
    // Idempotent: a second refresh leaves the current server alone.
    let again = sb.cinelog(&["install", "--refresh"]).unwrap().expect_ok().unwrap();
    assert!(again.stdout.contains("already runs"), "{again}");
    assert_eq!(lock_holder(sb), Some(new_server));

    // The 0.12 recorder keeps recording on its deleted executable; the new server indexes
    // its commands into the databases the old one created.
    let after = format!("echo after-{id}");
    a.run(&after).unwrap();
    sb.wait_hit(&[&format!("after-{id}")], scaled(20.0), |h| {
        h["typed"] == after.as_str()
    })
    .unwrap();
    sb.wait_hit(&[&format!("before-{id}")], scaled(20.0), |h| {
        h["typed"] == before.as_str()
    })
    .unwrap();

    // A new shell runs the new recorder.
    let b = sb.start_recorded(&t, "new").unwrap();
    assert!(upgrade::runs_file(b.pane.pid, bin), "the new shell runs the new binary");
    let fresh = format!("echo fresh-{id}");
    b.run(&fresh).unwrap();
    sb.wait_hit(&[&format!("fresh-{id}")], scaled(20.0), |h| {
        h["typed"] == fresh.as_str()
    })
    .unwrap();

    // Both shells end cleanly.
    for r in [&a, &b] {
        r.pane.type_line("exit").unwrap();
        r.wait_exit(scaled(10.0)).unwrap();
    }
    let status = sb.cinelog(&["status", "--json"]).unwrap();
    assert!(status.ok(), "{status}");
    check_casts(&[&a, &b]);
}

fn check_casts(recorded: &[&Recorded]) {
    for r in recorded {
        let cast = r.cast().unwrap();
        assert!(
            cast.exit_status().is_some(),
            "{} has its x event",
            r.cast_path.display()
        );
    }
}

/// A child process killed and reaped when dropped, also when an assertion fails first.
struct Running(Child);

impl Drop for Running {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// A process that runs `bin` until killed (a stand-in for anything running the team
/// binary while it is replaced), with its own runtime and state below `dir`.
fn run_from(sb: &Sandbox, bin: &Path, dir: &Path) -> Running {
    fs::create_dir_all(dir.join("home")).unwrap();
    fs::create_dir_all(dir.join("run")).unwrap();
    let mut env = sb.env();
    for (k, v) in &mut env {
        match k.as_str() {
            "HOME" | "ZDOTDIR" => *v = dir.join("home").display().to_string(),
            "XDG_CONFIG_HOME" => *v = dir.join("home/.config").display().to_string(),
            "XDG_STATE_HOME" => *v = dir.join("home/.local/state").display().to_string(),
            "XDG_RUNTIME_DIR" => *v = dir.join("run").display().to_string(),
            _ => {}
        }
    }
    fs::create_dir_all(dir.join("home/.config/cinelog")).unwrap();
    fs::write(
        dir.join("home/.config/cinelog/config.toml"),
        format!(
            "[paths]\nlogs = \"{}\"\n[server]\nautostart = \"off\"\n",
            dir.join("logs").display()
        ),
    )
    .unwrap();
    Running(
        Command::new(bin)
            .env_clear()
            .envs(env)
            .args(["serve", "--no-http"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    )
}

fn team_install(sb: &Sandbox, bin: &Path, args: &[&str]) -> (bool, String) {
    let out = Command::new(bin)
        .env_clear()
        .envs(sb.env())
        .args(["install", "--team"])
        .args(args)
        .stdin(Stdio::null())
        .output()
        .unwrap();
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    (out.status.success(), text)
}

fn team_upgrade(sb: &Sandbox, old: &OldRelease, module: &Path, new_version: &str) {
    let root: PathBuf = sb.root.join("teamroot");
    let root_s = root.display().to_string();
    let team_bin = root.join("usr/local/bin/cinelog");
    let share = root.join("usr/local/share/cinelog");

    // A 0.12 team install (a local build: unverified).
    let old_bin = old.bin.display().to_string();
    let old_tree = old.tree.display().to_string();
    let (ok, text) = team_install(
        sb,
        &old.bin,
        &[
            "--root",
            &root_s,
            "--module",
            &old_tree,
            "--binary",
            &old_bin,
            "--allow-unverified",
        ],
    );
    assert!(ok, "{text}");
    assert_eq!(version_of(&team_bin), format!("cinelog {}", old.version));
    assert_eq!(fs::read_to_string(share.join("VERSION")).unwrap().trim(), old.version);
    let mut running = run_from(sb, &team_bin, &sb.work.join("teamproc"));
    wait_until("the old team binary runs", scaled(10.0), || {
        upgrade::runs_file(running.0.id(), &team_bin).then_some(())
    })
    .unwrap();

    // The new module binary, verified against the module's checksum file.
    let new_bin = module.join("bin/cinelog");
    let module_s = module.display().to_string();
    let (ok, text) = team_install(sb, &new_bin, &["--root", &root_s, "--module", &module_s]);
    assert!(
        !ok && text.contains("--upgrade"),
        "a different binary needs --upgrade: {text}"
    );
    assert_eq!(version_of(&team_bin), format!("cinelog {}", old.version));

    let (ok, text) = team_install(sb, &new_bin, &["--root", &root_s, "--module", &module_s, "--upgrade"]);
    assert!(ok, "{text}");
    assert!(text.contains("verified"), "{text}");
    assert_eq!(version_of(&team_bin), format!("cinelog {new_version}"));
    assert_eq!(fs::read_to_string(share.join("VERSION")).unwrap().trim(), new_version);
    assert_eq!(
        fs::read(share.join("cinelog.plugin.zsh")).unwrap(),
        fs::read(module.join("cinelog.plugin.zsh")).unwrap()
    );
    assert_eq!(fs::metadata(&team_bin).unwrap().permissions().mode() & 0o7777, 0o755);

    let link = upgrade::exe_link(running.0.id()).unwrap_or_default();
    let alive = running.0.try_wait().unwrap().is_none();
    drop(running);
    assert!(alive, "the process running the old team binary survived the upgrade");
    assert!(link.ends_with(" (deleted)"), "{link}");

    // Idempotent: the same upgrade again changes nothing.
    let (ok, text) = team_install(sb, &new_bin, &["--root", &root_s, "--module", &module_s, "--upgrade"]);
    assert!(ok && text.contains("is up to date"), "{text}");
}
