//! End-to-end checks of the M1 subcommands through the built binary, each in a throwaway
//! HOME/XDG environment. Nothing here needs root; `install --team` runs in `--prefix` mode.

use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Command as StdCommand, Stdio};

use assert_cmd::Command;
use predicates::prelude::*;

struct Env {
    dir: tempfile::TempDir,
}

impl Env {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        for d in ["home", "config", "state", "run"] {
            std::fs::create_dir(dir.path().join(d)).unwrap();
        }
        std::fs::set_permissions(dir.path().join("run"), std::fs::Permissions::from_mode(0o700)).unwrap();
        Self { dir }
    }

    fn path(&self, rel: &str) -> PathBuf {
        self.dir.path().join(rel)
    }

    fn vars(&self) -> Vec<(&'static str, PathBuf)> {
        vec![
            ("HOME", self.path("home")),
            ("XDG_CONFIG_HOME", self.path("config")),
            ("XDG_STATE_HOME", self.path("state")),
            ("XDG_RUNTIME_DIR", self.path("run")),
            ("PATH", "/usr/bin:/bin".into()),
            ("TERM", "xterm-256color".into()),
        ]
    }

    fn cmd(&self) -> Command {
        let mut c = Command::cargo_bin("cinelog").unwrap();
        c.env_clear().envs(self.vars());
        c
    }

    fn std_cmd(&self) -> StdCommand {
        let mut c = StdCommand::new(assert_cmd::cargo::cargo_bin("cinelog"));
        c.env_clear().envs(self.vars());
        c
    }
}

fn is_root() -> bool {
    nix::unistd::geteuid().is_root()
}

/// Runs `cinelog rec --check` with a fresh pty slave as stdin (this test holds the master).
fn check_on_pty(env: &Env) -> String {
    let pty = nix::pty::openpty(None, None).unwrap();
    let out = env
        .std_cmd()
        .args(["rec", "--check"])
        .stdin(Stdio::from(pty.slave))
        .output()
        .unwrap();
    assert!(out.status.success());
    drop(pty.master);
    String::from_utf8(out.stdout).unwrap()
}

#[test]
fn check_without_a_tty() {
    Env::new()
        .cmd()
        .args(["rec", "--check"])
        .write_stdin("")
        .assert()
        .success()
        .stdout("skip not-a-tty\n");
}

#[test]
fn check_on_a_pty_starts_and_honours_the_kill_switches() {
    if is_root() {
        return; // the scope would be team; covered by the unit tests
    }
    let env = Env::new();
    let line = check_on_pty(&env);
    assert!(is_start_personal(&line), "unexpected decision {line:?}");

    std::fs::create_dir_all(env.path("config/cinelog")).unwrap();
    std::fs::write(env.path("config/cinelog/disable"), "").unwrap();
    assert_eq!(check_on_pty(&env), "skip disabled\n");
    std::fs::remove_file(env.path("config/cinelog/disable")).unwrap();

    let pty = nix::pty::openpty(None, None).unwrap();
    let out = env
        .std_cmd()
        .env("CINELOG_DISABLE", "1")
        .args(["rec", "--check"])
        .stdin(Stdio::from(pty.slave))
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&out.stdout), "skip disabled\n");
}

/// `start personal <reason>` with a single-token reason.
fn is_start_personal(line: &str) -> bool {
    let mut it = line.trim_end().split(' ');
    matches!((it.next(), it.next(), it.next(), it.next()), (Some("start"), Some("personal"), Some(r), None) if !r.is_empty())
}

#[test]
fn check_with_a_broken_config_skips_and_logs() {
    if is_root() {
        return;
    }
    let env = Env::new();
    std::fs::create_dir_all(env.path("config/cinelog")).unwrap();
    std::fs::write(env.path("config/cinelog/config.toml"), "[record\n").unwrap();
    assert_eq!(check_on_pty(&env), "skip error\n");
    let log = std::fs::read_to_string(env.path("state/cinelog/rec.log")).unwrap();
    assert!(log.contains("config.toml"), "{log}");
}

#[test]
fn session_commands_outside_a_recording() {
    let env = Env::new();
    for args in [&["link"][..], &["link", "--json"], &["pause"], &["resume"]] {
        env.cmd()
            .args(args)
            .write_stdin("")
            .assert()
            .code(1)
            .stderr(predicate::str::contains("not in a recorded session"));
    }
}

#[test]
fn doctor_reports_json() {
    let env = Env::new();
    let out = env.cmd().args(["doctor", "--json"]).write_stdin("").output().unwrap();
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["version"], env!("CARGO_PKG_VERSION"));
    let ids: Vec<&str> = v["checks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["id"].as_str().unwrap())
        .collect();
    for id in [
        "version",
        "config",
        "guards",
        "detection",
        "session",
        "registry",
        "logs",
        "db-location",
    ] {
        assert!(ids.contains(&id), "missing {id} in {ids:?}");
    }
    // stdin is not a tty here, so the guards check warns and the exit code is 1.
    assert_eq!(out.status.code(), Some(1));
    let text = env.cmd().arg("doctor").write_stdin("").output().unwrap();
    assert!(String::from_utf8(text.stdout).unwrap().contains("[warn] guards"));
}

/// `doctor --json | jq` in a terminal: doctor's redirected stdout is not a guard problem.
#[test]
fn doctor_json_through_a_pipe_inside_a_terminal() {
    if StdCommand::new("script").arg("--version").output().is_err() {
        return;
    }
    let env = Env::new();
    let out_file = env.path("doctor.json");
    let inner = format!(
        "'{}' doctor --json | cat > '{}'",
        assert_cmd::cargo::cargo_bin("cinelog").display(),
        out_file.display()
    );
    let status = StdCommand::new("script")
        .env_clear()
        .envs(env.vars())
        .args(["-qec", &inner, "/dev/null"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .status()
        .unwrap();
    assert!(status.success());
    let v: serde_json::Value = serde_json::from_slice(&std::fs::read(&out_file).unwrap()).unwrap();
    let guards = v["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["id"] == "guards")
        .unwrap();
    assert_eq!(guards["status"], "ok", "{guards}");
}

#[test]
fn personal_install_refresh_and_uninstall() {
    if is_root() {
        return;
    }
    let env = Env::new();
    let old = env.path("home/.config/cinelog/cinelog-settings.conf");
    std::fs::create_dir_all(old.parent().unwrap()).unwrap();
    std::fs::write(
        &old,
        "export CINELOG_VIEWER_PORT=11000\nexport CINELOG_AUTH_UUID=\"do-not-print-me\"\nMOTD_ENABLED=0\n",
    )
    .unwrap();
    let out = env.cmd().arg("install").write_stdin("").output().unwrap();
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(
        out.status.success(),
        "{stdout}\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!stdout.contains("do-not-print-me"));
    assert!(stdout.contains("migrated [shell] motd"), "{stdout}");
    let cfg = std::fs::read_to_string(env.path("config/cinelog/config.toml")).unwrap();
    assert!(cfg.contains("\nmotd = false\n"));
    assert!(env.path("home/logs").is_dir());
    let link = env.path("home/.local/bin/cinelog");
    assert_eq!(
        std::fs::read_link(&link).unwrap(),
        assert_cmd::cargo::cargo_bin("cinelog")
    );

    std::fs::remove_file(&link).unwrap();
    env.cmd().args(["install", "--refresh"]).assert().success();
    assert!(std::fs::read_link(&link).is_ok());

    env.cmd().args(["install", "--uninstall"]).assert().success();
    assert!(std::fs::symlink_metadata(&link).is_err());
    assert!(env.path("config/cinelog/config.toml").exists());
}

fn fake_module(env: &Env) -> PathBuf {
    let m = env.path("module");
    std::fs::create_dir_all(m.join("shell")).unwrap();
    std::fs::write(m.join("cinelog.plugin.zsh"), "# plugin\n").unwrap();
    std::fs::write(m.join("VERSION"), format!("{}\n", env!("CARGO_PKG_VERSION"))).unwrap();
    std::fs::write(m.join("shell/early.zsh"), "# early\n").unwrap();
    m
}

#[test]
fn team_install_in_prefix_mode() {
    let env = Env::new();
    let module = fake_module(&env);
    let prefix = env.path("sys");
    let base = |extra: &[&str]| {
        let mut c = env.cmd();
        c.args(["install", "--team", "--module"])
            .arg(&module)
            .arg("--prefix")
            .arg(&prefix)
            .arg("--binary")
            .arg(assert_cmd::cargo::cargo_bin("cinelog"));
        c.args(extra);
        c
    };
    base(&[])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("--allow-unverified"));
    base(&["--allow-unverified", "--system-zshrc"]).assert().success();
    let bin = prefix.join("usr/local/bin/cinelog");
    assert_eq!(
        std::fs::read(&bin).unwrap(),
        std::fs::read(assert_cmd::cargo::cargo_bin("cinelog")).unwrap()
    );
    let rc = std::fs::read_to_string(prefix.join("etc/zsh/zshrc")).unwrap();
    assert!(rc.contains(&format!(
        "'{}'",
        prefix.join("usr/local/share/cinelog/cinelog.plugin.zsh").display()
    )));
    assert!(prefix.join("etc/cinelog/team.toml").is_file());
    assert!(prefix.join("root/logs").is_dir());

    // The installed copy reports its version like the original.
    StdCommand::new(&bin)
        .arg("--version")
        .output()
        .map(|o| assert!(String::from_utf8_lossy(&o.stdout).starts_with("cinelog ")))
        .unwrap();

    // `sudo cinelog install --team --upgrade` finds the installed team binary: it must
    // install the module's new bin/cinelog, not itself.
    std::fs::create_dir_all(module.join("bin")).unwrap();
    std::fs::create_dir_all(module.join("shell/checksums")).unwrap();
    let release = b"\x7fELF release 9.9.9".as_slice();
    std::fs::write(module.join("bin/cinelog"), release).unwrap();
    std::fs::write(module.join("VERSION"), "9.9.9\n").unwrap();
    std::fs::write(
        module.join("shell/checksums/v9.9.9.sha256"),
        format!("{}  cinelog-x86_64-unknown-linux-musl\n", sha256_hex(release)),
    )
    .unwrap();
    let out = StdCommand::new(&bin)
        .env_clear()
        .envs(env.vars())
        .args(["install", "--team", "--upgrade", "--module"])
        .arg(&module)
        .arg("--prefix")
        .arg(&prefix)
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(std::fs::read(&bin).unwrap(), release);
    assert_eq!(
        std::fs::read_to_string(prefix.join("usr/local/share/cinelog/VERSION")).unwrap(),
        "9.9.9\n"
    );

    base(&["--uninstall"]).assert().success();
    assert!(!prefix.join("usr/local/share/cinelog").exists());
    assert!(
        !std::fs::read_to_string(prefix.join("etc/zsh/zshrc"))
            .unwrap()
            .contains("cinelog")
    );
}

/// `sudo cinelog install --team --uninstall` runs /usr/local/bin/cinelog, which is in no
/// module; after a rollback the module may be gone or have no bin/cinelog either.
#[test]
fn team_uninstall_runs_from_a_binary_outside_any_module() {
    let env = Env::new();
    let module = fake_module(&env);
    let prefix = env.path("sys");
    env.cmd()
        .args(["install", "--team", "--allow-unverified", "--system-zshrc", "--module"])
        .arg(&module)
        .arg("--prefix")
        .arg(&prefix)
        .arg("--binary")
        .arg(assert_cmd::cargo::cargo_bin("cinelog"))
        .assert()
        .success();
    std::fs::remove_dir_all(&module).unwrap();
    let lone = env.path("elsewhere/cinelog");
    std::fs::create_dir_all(lone.parent().unwrap()).unwrap();
    std::fs::copy(assert_cmd::cargo::cargo_bin("cinelog"), &lone).unwrap();

    let out = StdCommand::new(&lone)
        .env_clear()
        .envs(env.vars())
        .args(["install", "--team", "--uninstall", "--prefix"])
        .arg(&prefix)
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(!prefix.join("usr/local/share/cinelog").exists());
    let rc = std::fs::read_to_string(prefix.join("etc/zsh/zshrc")).unwrap();
    assert!(!rc.contains("cinelog"), "{rc}");
}

fn sha256_hex(bytes: &[u8]) -> String {
    let out = StdCommand::new("sha256sum")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .and_then(|mut c| {
            use std::io::Write;
            c.stdin.take().unwrap().write_all(bytes)?;
            c.wait_with_output()
        })
        .unwrap();
    String::from_utf8(out.stdout).unwrap()[..64].to_owned()
}

#[test]
fn team_install_without_prefix_needs_root() {
    if is_root() {
        return;
    }
    let env = Env::new();
    let module = fake_module(&env);
    env.cmd()
        .args(["install", "--team", "--allow-unverified", "--module"])
        .arg(&module)
        .assert()
        .code(1)
        .stderr(predicate::str::contains("sudo"));
}

#[test]
fn completions_mention_the_new_flags() {
    Env::new()
        .cmd()
        .args(["completions", "zsh"])
        .assert()
        .success()
        .stdout(predicate::str::contains("--allow-unverified").and(predicate::str::contains("--module")));
}

/// Under `script`, registers `script` itself as the recorder of the pty, then runs
/// `link --json` and `pause`: the pause marker must reach the tty with the registry nonce.
#[test]
fn link_and_pause_inside_a_registered_pty() {
    if is_root() || StdCommand::new("script").arg("--version").output().is_err() {
        return;
    }
    let env = Env::new();
    let nonce = "0123456789abcdef0123456789abcdef";
    let bin = assert_cmd::cargo::cargo_bin("cinelog");
    let inner = format!(
        r#"pts=$(tty); n=${{pts#/dev/pts/}}
st=$(cut -d')' -f2- /proc/$PPID/stat | awk '{{print $20}}')
boot=$(cat /proc/sys/kernel/random/boot_id)
mkdir -p "$XDG_RUNTIME_DIR/cinelog"; umask 077
printf '{{"format":1,"pid":%s,"starttime":%s,"boot_id":"%s","uuid":"0193-test","nonce":"{nonce}","scope":"personal","cast":"/l/x.cast","pts":"%s","state":"live","part":1,"version":"0"}}' \
  $PPID $st $boot $pts > "$XDG_RUNTIME_DIR/cinelog/pts-$n.json"
'{bin}' link --json; echo "link=$?"; '{bin}' pause --reason lunch; echo "pause=$?""#,
        bin = bin.display()
    );
    let out = StdCommand::new("script")
        .env_clear()
        .envs(env.vars())
        .args(["-qec", &inner, "/dev/null"])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.contains(r#"{"uuid":"0193-test","cast":"/l/x.cast","url":null,"state":"live"}"#),
        "{text}"
    );
    assert!(text.contains("link=0") && text.contains("pause=0"), "{text}");
    assert!(text.contains(&format!("\x1b]7777;cinelog;1;{nonce};b;")), "{text:?}");
    assert!(text.contains("has not confirmed"), "no recorder confirms here: {text}");
}
