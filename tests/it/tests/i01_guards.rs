//! I1 [R8]: non-interactive and tool-driven zsh invocations never exec the recorder, never
//! hang, and leave no cast behind.

use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use cinelog_it::util::output_with_timeout;
use cinelog_it::{Need, Sandbox, need, scaled};

fn zsh(sb: &Sandbox, extra_env: &[(&str, &str)], args: &[&str]) -> (std::process::Output, Duration) {
    let mut cmd = Command::new(&sb.zsh);
    cmd.env_clear()
        .envs(sb.env())
        .envs(extra_env.iter().copied())
        .args(args)
        .current_dir(&sb.work)
        .stdin(Stdio::null());
    let start = Instant::now();
    let out = output_with_timeout(cmd, scaled(10.0)).expect("zsh finished (no hang)");
    (out, start.elapsed())
}

fn assert_nothing_recorded(sb: &Sandbox) {
    let casts = cinelog_it::cast::find_casts(&sb.root);
    assert!(casts.is_empty(), "no cast may be created, found {casts:?}");
    assert!(
        sb.registry().is_empty(),
        "no recorder may register: {:?}",
        sb.registry()
    );
}

#[test]
fn i1_zsh_dash_c_without_a_tty_is_fast_and_unrecorded() {
    need!(Need::Zsh, Need::Plugin, Need::Recorder);
    let sb = Sandbox::new().unwrap();

    let (out, took) = zsh(&sb, &[], &["-i", "-c", "echo X"]);
    assert!(out.status.success(), "zsh -i -c failed: {out:?}");
    assert_eq!(String::from_utf8_lossy(&out.stdout), "X\n");
    assert!(took < scaled(1.0), "zsh -i -c took {took:?}");

    let (out, took) = zsh(
        &sb,
        &[("VSCODE_RESOLVING_ENVIRONMENT", "1")],
        &["-i", "-l", "-c", "env"],
    );
    assert!(out.status.success(), "env resolution failed: {out:?}");
    let env = String::from_utf8_lossy(&out.stdout);
    assert!(env.lines().any(|l| l == "VSCODE_RESOLVING_ENVIRONMENT=1"));
    assert!(
        !env.lines().any(|l| l.starts_with("CINELOG_")),
        "the plugin exports nothing: {env}"
    );
    assert!(took < scaled(1.0), "VS Code env resolution took {took:?}");
    assert_nothing_recorded(&sb);
}

/// The same guards with a real tty (`script` allocates one): `-c`, `TERM=dumb`,
/// `INSIDE_EMACS` and VS Code env resolution must not start a recording either.
#[test]
fn i1_guards_hold_on_a_tty() {
    need!(Need::Zsh, Need::Script, Need::Plugin, Need::Recorder);
    let sb = Sandbox::new().unwrap();
    let zsh = sb.zsh.to_string_lossy().into_owned();
    type Case<'a> = (&'a str, &'a [(&'a str, &'a str)], String);
    let cases: [Case<'_>; 4] = [
        ("-c", &[], format!("{zsh} -i -c 'echo tty-$((40+2))'")),
        ("TERM=dumb", &[("TERM", "dumb")], format!("{zsh} -i")),
        ("INSIDE_EMACS", &[("INSIDE_EMACS", "29.1,comint")], format!("{zsh} -i")),
        ("VS Code", &[("VSCODE_RESOLVING_ENVIRONMENT", "1")], format!("{zsh} -i")),
    ];
    for (name, env, command) in cases {
        let mut cmd = Command::new("script");
        cmd.env_clear()
            .envs(sb.env())
            .envs(env.iter().copied())
            .args(["-q", "-e", "-c", &command, "/dev/null"])
            .current_dir(&sb.work)
            .stdin(Stdio::piped());
        let start = Instant::now();
        let mut child = cmd.stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
        {
            use std::io::Write;
            let mut stdin = child.stdin.take().unwrap();
            // Interactive cases read this; the -c case ignores it.
            let _ = stdin.write_all(b"echo tty-$((40+2))\nexit\n");
        }
        let deadline = Instant::now() + scaled(10.0);
        while child.try_wait().unwrap().is_none() {
            if Instant::now() > deadline {
                let _ = child.kill();
                panic!("{name}: zsh on a tty did not finish (hang)");
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let out = child.wait_with_output().unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(stdout.contains("tty-42"), "{name}: output {stdout:?}");
        assert!(start.elapsed() < scaled(3.0), "{name}: took {:?}", start.elapsed());
        assert_nothing_recorded(&sb);
    }
}
