//! I19: a VS Code-style `ZDOTDIR` wrapper (a simulated shell-integration injection) around
//! the user's `.zshrc`: the shell is recorded exactly once, the wrapper's OSC 633 sequences
//! reach the cast's output unchanged, and neither integration is installed twice.
//!
//! The plugin execs the recorder from inside the user's `.zshrc`, so the recorded shell
//! reads the startup files `ZDOTDIR` names at that moment. VS Code's own injection points
//! `ZDOTDIR` back at the user's directory before it sources the user's `.zshrc`; the plugin
//! restores the wrapper's `ZDOTDIR` for the exec, so both cases run the wrapper again.

use std::fs;

use cinelog_it::{Need, Sandbox, need, procs, unique_id};

/// The wrapper's `.zshenv`: the user's own, read with the user's `ZDOTDIR`.
const WRAPPER_ZSHENV: &str = r#"VSCODE_ZDOTDIR=$ZDOTDIR
ZDOTDIR=$USER_ZDOTDIR
[[ -f $USER_ZDOTDIR/.zshenv ]] && . $USER_ZDOTDIR/.zshenv
ZDOTDIR=$VSCODE_ZDOTDIR
"#;

/// The wrapper's `.zshrc`, after VS Code's `shellIntegration-rc.zsh`: hooks that emit
/// OSC 633 A (prompt start), B (prompt end, in `PS1`), C (pre-execution) and D (exit
/// status), then the user's `.zshrc` ([`SOURCE_USER_RC`] or [`SOURCE_USER_RC_AS_VSCODE`]),
/// which loads the plugin.
const WRAPPER_ZSHRC: &str = r#"(( ${+VSCODE_SHELL_INTEGRATION} )) && return
VSCODE_SHELL_INTEGRATION=1
typeset -gi __vsc_ran=0
__vsc_precmd() {
  local -i ret=$?
  (( __vsc_ran )) && print -n "\e]633;D;$ret\a"
  __vsc_ran=0
  print -n '\e]633;A\a'
  [[ $PS1 == *'633;B'* ]] || PS1+=$'%{\e]633;B\a%}'
  return ret
}
__vsc_preexec() {
  print -n '\e]633;C\a'
  __vsc_ran=1
}
autoload -Uz add-zsh-hook
add-zsh-hook precmd __vsc_precmd
add-zsh-hook preexec __vsc_preexec
"#;

/// `ZDOTDIR` keeps pointing at the wrapper, so the recorded shell runs it again.
const SOURCE_USER_RC: &str = ". $USER_ZDOTDIR/.zshrc\n";

/// As VS Code does it: the user's `.zshrc` runs with the user's `ZDOTDIR` (exported).
const SOURCE_USER_RC_AS_VSCODE: &str = "VSCODE_ZDOTDIR=$ZDOTDIR\nZDOTDIR=$USER_ZDOTDIR\n. $USER_ZDOTDIR/.zshrc\n";

fn count(hay: &str, needle: &str) -> usize {
    hay.matches(needle).count()
}

#[test]
fn i19_vscode_wrapper_is_recorded_once_and_osc_633_passes_through() {
    scenario(SOURCE_USER_RC);
}

#[test]
fn i19_vscode_injection_survives_the_recorder_exec() {
    scenario(SOURCE_USER_RC_AS_VSCODE);
}

fn scenario(source_user_rc: &str) {
    need!(Need::Zsh, Need::Tmux, Need::Plugin, Need::Recorder);
    let sb = Sandbox::new().unwrap();
    let wrapper = sb.root.join("zdot-vscode");
    fs::create_dir(&wrapper).unwrap();
    fs::write(wrapper.join(".zshenv"), WRAPPER_ZSHENV).unwrap();
    fs::write(wrapper.join(".zshrc"), format!("{WRAPPER_ZSHRC}{source_user_rc}")).unwrap();
    let (wrapper, home) = (wrapper.to_string_lossy(), sb.home.to_string_lossy());
    let argv = sb.env_argv(
        &[
            ("ZDOTDIR", &wrapper),
            ("USER_ZDOTDIR", &home),
            ("VSCODE_INJECTION", "1"),
            ("TERM_PROGRAM", "vscode"),
        ],
        &[sb.zsh.to_string_lossy().into_owned(), "-i".into()],
    );
    let t = sb.tmux().unwrap();
    let r = sb.start_recorded_argv(&t, "main", &argv).unwrap();
    let tag = unique_id();

    let ok = format!("echo vscode-{tag}");
    r.run(&ok).unwrap();
    let (_, done) = r.run("(exit 3)").unwrap();
    assert_eq!(done.i64("exit"), Some(3));
    let hooks = "print -r -- hooks-${#${(M)precmd_functions:#__vsc_precmd}}${#${(M)preexec_functions:#__vsc_preexec}}${#${(M)precmd_functions:#_cl_precmd}}${#${(M)preexec_functions:#_cl_preexec}}";
    r.run(hooks).unwrap();

    // One recording: one cast, one registry entry, one recorder process.
    assert_eq!(sb.cast_paths(), vec![r.cast_path.clone()], "exactly one cast");
    let entries: Vec<_> = sb
        .registry()
        .into_iter()
        .map(|(p, _)| p)
        .filter(|p| {
            let name = p.file_name().unwrap_or_default().to_string_lossy();
            name.starts_with("pts-") || name.starts_with("tty-")
        })
        .collect();
    assert_eq!(entries.len(), 1, "registry: {entries:?}");
    let recorders: Vec<u32> = sb
        .processes()
        .into_iter()
        .filter(|&p| procs::comm(p).as_deref() == Some("cinelog"))
        .collect();
    assert_eq!(recorders, vec![r.pane.pid], "a single recorder");

    let c = r.cast().unwrap();
    assert_eq!(c.markers_of("hello").len(), 1, "the plugin attached once");
    assert_eq!(
        c.markers_of("cmd")
            .iter()
            .filter(|m| m.str("typed") == Some(ok.as_str()))
            .count(),
        1
    );
    let out = c.output();
    assert!(out.contains(&format!("vscode-{tag}")));
    assert!(
        out.contains("hooks-1111"),
        "each integration's hooks are installed once in the recorded shell"
    );

    // The wrapper's sequences pass through byte for byte, once per prompt and command.
    for seq in [
        "\x1b]633;A\x07",
        "\x1b]633;B\x07",
        "\x1b]633;C\x07",
        "\x1b]633;D;0\x07",
        "\x1b]633;D;3\x07",
    ] {
        assert!(out.contains(seq), "{seq:?} missing from the cast output");
    }
    assert_eq!(
        count(&out, "\x1b]633;A"),
        count(&out, "\x1b]133;A"),
        "633;A once per prompt"
    );
    assert_eq!(
        count(&out, "\x1b]633;C"),
        count(&out, "\x1b]133;C"),
        "633;C once per command"
    );
    assert_eq!(
        count(&out, "\x1b]633;D"),
        count(&out, "\x1b]133;D"),
        "633;D once per command"
    );
    assert_eq!(count(&out, "\x1b]633;C"), c.markers_of("cmd").len());
}
