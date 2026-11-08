//! Binary-interface contract checks (docs/PROTOCOL.md, "Binary interface").

use assert_cmd::Command;

fn cinelog() -> Command {
    Command::cargo_bin("cinelog").unwrap()
}

#[test]
fn version_line() {
    let out = cinelog().arg("--version").output().unwrap();
    assert!(out.status.success());
    assert_eq!(
        String::from_utf8(out.stdout).unwrap(),
        format!("cinelog {}\n", env!("CARGO_PKG_VERSION"))
    );
}

#[test]
fn protocol_line() {
    cinelog().args(["rec", "--protocol"]).assert().success().stdout("1\n");
}

/// The documented fallback, provoked with the kill switch in a throwaway environment (a
/// real recording must never reach the developer's own `~/logs`).
#[test]
fn fallback_sets_failed_env_and_strips_other_cinelog_vars() {
    let dir = tempfile::tempdir().unwrap();
    let mut cmd = cinelog();
    cmd.env_clear()
        .env("HOME", dir.path().join("home"))
        .env("XDG_CONFIG_HOME", dir.path().join("config"))
        .env("XDG_STATE_HOME", dir.path().join("state"))
        .env("XDG_RUNTIME_DIR", dir.path().join("run"))
        .env("PATH", "/usr/bin:/bin")
        .env("CINELOG_DISABLE", "1")
        .env("CINELOG_NONCE_FD", "9")
        .args([
            "rec",
            "--",
            "/bin/sh",
            "-c",
            r#"echo "$CINELOG_FAILED|${CINELOG_NONCE_FD-unset}|$([ "$CINELOG_FAILED_PID" = $$ ] && echo pid-ok)""#,
        ])
        .assert()
        .success()
        .stdout(predicates::str::starts_with("disabled|unset|pid-ok"));
    assert!(!dir.path().join("home/logs").exists(), "nothing was recorded");
}

#[test]
fn version_file_matches_crate_version() {
    let v = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../../VERSION")).unwrap();
    assert_eq!(v.trim(), env!("CARGO_PKG_VERSION"));
}
