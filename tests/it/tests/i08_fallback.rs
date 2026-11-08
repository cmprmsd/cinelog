//! I8 [R9]: when recording cannot start (read-only log dir, missing binary, corrupt config,
//! full disk) the shell still comes up, prints exactly one warning, and there is no exec
//! loop: the shell keeps its pid and the sandbox's process count stays stable.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::time::Duration;

use cinelog_it::util::{settle, warning_lines};
use cinelog_it::{Need, Pane, Sandbox, SandboxOptions, need, scaled};

/// How the shell came up without a working recording.
#[derive(Debug, PartialEq, Eq)]
enum Outcome {
    /// The recorder fell back (or never ran): the pane's process is the shell itself.
    Unrecorded,
    /// The recorder started but degraded at once (passthrough only, registry says so).
    Degraded,
}

fn assert_comes_up_once(sb: &Sandbox, pane: &Pane, expect: &str) -> Outcome {
    pane.wait_prompt(Pane::startup_timeout()).unwrap();
    pane.run_sync(
        "echo alive-$((6*7)) pid-$$ \"failed-[${CINELOG_FAILED-unset}]\"",
        scaled(10.0),
    )
    .unwrap();
    let screen = pane.capture().unwrap();
    let line = screen
        .lines()
        .find_map(|l| l.strip_prefix("alive-42 pid-"))
        .unwrap_or_else(|| panic!("the shell answers:\n{screen}"));
    let (pid, rest) = line.split_once(' ').unwrap();
    assert_eq!(rest, "failed-[unset]", "the plugin unsets the fallback variables");
    let outcome = if pid == pane.pid.to_string() {
        Outcome::Unrecorded
    } else {
        let degraded = sb
            .registry()
            .into_iter()
            .any(|(_, e)| e["pid"].as_u64() == Some(u64::from(pane.pid)) && e["state"] == "degraded");
        assert!(
            degraded,
            "shell {pid} is neither the pane process nor under a degraded recorder"
        );
        Outcome::Degraded
    };

    let before = sb.processes().len();
    settle(Duration::from_secs(2));
    let after = sb.processes().len();
    assert_eq!(before, after, "process count changed: exec loop?");
    pane.run_sync("echo still-$((1+1))", scaled(10.0)).unwrap();

    let screen = pane.capture().unwrap();
    let w = warning_lines(&screen);
    assert_eq!(w.len(), 1, "exactly one warning, got {w:?}\n{screen}");
    assert!(
        w[0].to_lowercase().contains(expect),
        "warning {:?} should mention {expect:?}",
        w[0]
    );
    assert!(pane.exists());
    outcome
}

fn assert_no_cast(sb: &Sandbox) {
    let casts = cinelog_it::cast::find_casts(&sb.root);
    assert!(casts.is_empty(), "nothing may be recorded: {casts:?}");
}

#[test]
fn i8_read_only_log_dir() {
    need!(Need::Zsh, Need::Tmux, Need::Plugin, Need::Recorder);
    let sb = Sandbox::new().unwrap();
    fs::set_permissions(&sb.logs, fs::Permissions::from_mode(0o500)).unwrap();
    let t = sb.tmux().unwrap();
    let pane = t.new_session("main", 120, 40, &sb.shell_argv()).unwrap();
    assert_eq!(assert_comes_up_once(&sb, &pane, "not recording"), Outcome::Unrecorded);
    assert_no_cast(&sb);
    assert!(!sb.rec_log().trim().is_empty(), "the reason is appended to rec.log");
}

#[test]
fn i8_missing_binary() {
    need!(Need::Zsh, Need::Tmux, Need::Plugin);
    let sb = Sandbox::with(SandboxOptions {
        flavour: None,
        ..SandboxOptions::default()
    })
    .unwrap();
    // The plugin falls back to `${commands[cinelog]}`: a system-wide install must stay hidden.
    assert_eq!(sb.resolve("cinelog"), None, "a cinelog binary is on the sandbox PATH");
    let t = sb.tmux().unwrap();
    let pane = t.new_session("main", 120, 40, &sb.shell_argv()).unwrap();
    assert_eq!(assert_comes_up_once(&sb, &pane, "missing"), Outcome::Unrecorded);
    assert_no_cast(&sb);
}

#[test]
fn i8_corrupt_config() {
    need!(Need::Zsh, Need::Tmux, Need::Plugin, Need::Recorder);
    let sb = Sandbox::with(SandboxOptions {
        raw_config: Some("[record\nnested = = \"pause\"\n\u{0}garbage".into()),
        ..SandboxOptions::default()
    })
    .unwrap();
    let t = sb.tmux().unwrap();
    let pane = t.new_session("main", 120, 40, &sb.shell_argv()).unwrap();
    assert_eq!(assert_comes_up_once(&sb, &pane, "not recording"), Outcome::Unrecorded);
    assert_no_cast(&sb);
}

/// Starts a shell whose log dir is a tmpfs that is completely full before the shell starts
/// (the prefill exceeds the 256 KiB size).
fn full_disk_shell(sb: &Sandbox) -> (cinelog_it::Tmux, Pane) {
    let t = sb.tmux().unwrap();
    let argv = sb.tmpfs_argv(&sb.logs, "256k", 1 << 20, sb.shell_argv());
    let pane = t.new_session("main", 120, 40, &argv).unwrap();
    sb.view_through(pane.pid);
    (t, pane)
}

fn full_disk_sandbox() -> Sandbox {
    Sandbox::with(SandboxOptions {
        logs: Some("logs-tmpfs".into()),
        ..SandboxOptions::default()
    })
    .unwrap()
}

/// A log dir that is full before the shell starts: no recorder (the free-space check runs
/// before forkpty, PROTOCOL §6.4), one "not recording" warning, no exec loop, and the
/// reason in rec.log.
#[test]
fn i8_full_disk() {
    need!(Need::Zsh, Need::Tmux, Need::UserNs, Need::Plugin, Need::Recorder);
    let sb = full_disk_sandbox();
    let (_t, pane) = full_disk_shell(&sb);
    assert_eq!(assert_comes_up_once(&sb, &pane, "not recording"), Outcome::Unrecorded);
    assert_no_cast(&sb);
    let log = sb.rec_log().to_lowercase();
    assert!(
        ["space", "full", "enospc"].iter().any(|w| log.contains(w)),
        "rec.log names the full log dir: {log:?}"
    );
}
