//! I5 (root-gated): `sudo -s`, `sudo -i` and `sudo su -` from a recorded user shell start
//! a TEAM recording under `/root/logs/<runner>/` with the right `sudo_user`; the user's
//! recorder writes a `nested` link marker and pauses (`nested = "pause"`), or keeps
//! recording with `nested = "record"`.
//!
//! Needs `--features root`, `CINELOG_IT_ROOT=1` and passwordless sudo, on throwaway machines
//! only: the TEAM integration is installed into a throwaway prefix, but root's zsh is hooked
//! through the real system zshrc for the duration of the test (see `src/sudo.rs`, README.md).
//! The scenarios are serialised by [`TeamInstall`].

use cinelog_it::binary::{self, Flavour};
use cinelog_it::sudo::{TEAM_LOGS, TeamInstall};
use cinelog_it::{Marker, Need, Recorded, Sandbox, SandboxOptions, cast_timeout, need, scaled, unique_id};

fn wait_after(r: &Recorded, kind: &str, after: usize, pred: impl Fn(&Marker) -> bool) -> Marker {
    r.wait_cast(&format!("{kind} after #{after}"), scaled(15.0), |c| {
        c.markers_of(kind).into_iter().find(|m| m.index > after && pred(m))
    })
    .unwrap()
}

/// Runs `line` in the root shell that `elevate` opened. The TEAM cast is the one the user
/// recorder's `nested` marker links to (`child_uuid`), never "whatever cast is new".
fn elevated_session(
    team: &TeamInstall,
    sb: &Sandbox,
    a: &Recorded,
    elevate: &str,
    line: &str,
) -> (cinelog_it::Cast, Marker) {
    let start = a.cast().unwrap().events.len();
    a.pane.type_line(elevate).unwrap();
    // Root's prompt is root's own; synchronise on printed tokens instead.
    a.pane.sync(scaled(15.0)).unwrap();
    a.pane.type_line(line).unwrap();
    a.pane.sync(scaled(10.0)).unwrap();
    let nested = wait_after(a, "nested", start, |_| true);
    let child = nested.str("child_uuid").expect("nested.child_uuid").to_owned();
    let cast = team
        .team_cast(sb.user(), &child, cast_timeout(), |c| c.cmd(line).is_some())
        .unwrap();
    a.pane.type_line("exit").unwrap();
    // Wait until the user shell is back (its `done` for the elevating command): lines typed
    // while root's shell is still exiting are read and dropped by it.
    wait_after(a, "done", nested.index, |_| true);
    (cast, nested)
}

fn check_team_session(team: &cinelog_it::Cast, runner: &str) {
    let s = team.session().expect("session marker");
    assert_eq!(s.str("scope"), Some("team"), "{}", s.json);
    assert_eq!(s.u64("euid"), Some(0));
    assert_eq!(s.str("sudo_user"), Some(runner), "{}", s.json);
    assert!(
        team.path.starts_with(format!("{TEAM_LOGS}/{runner}/")),
        "team cast at {}",
        team.path.display()
    );
}

#[test]
fn i5_sudo_shells_record_in_team_scope_and_pause_the_user_recorder() {
    need!(Need::Zsh, Need::Tmux, Need::Plugin, Need::Recorder, Need::Root);
    let team = TeamInstall::install(&binary::cinelog(Flavour::Plain).unwrap()).unwrap();
    let sb = Sandbox::new().unwrap();
    let t = sb.tmux().unwrap();
    let a = sb.start_recorded(&t, "main").unwrap();
    let tag = unique_id();
    for elevate in ["sudo -s", "sudo -i", "sudo su -"] {
        let line = format!("echo team-{tag}-{}", elevate.replace(' ', "_"));
        let (cast, nested) = elevated_session(&team, &sb, &a, elevate, &line);
        check_team_session(&cast, sb.user());
        assert_eq!(nested.str("mode"), Some("pause"), "{}", nested.json);
        assert_eq!(nested.str("scope"), Some("team"), "{}", nested.json);
        let pause = wait_after(&a, "pause", nested.index, |m| m.str("reason") == Some("nested"));
        let resume = wait_after(&a, "resume", pause.index, |_| true);
        assert_eq!(resume.str("reason"), Some("nested-end"), "{}", resume.json);
        a.run(&format!("echo back-{tag}-{}", elevate.replace(' ', "_")))
            .unwrap();

        let c = a.cast().unwrap();
        assert!(c.cmd(&line).is_none(), "{elevate}: root command in the user cast");
        assert!(
            !c.output().contains(&line["echo ".len()..]),
            "{elevate}: root output in the user cast"
        );
        assert!(c.events_while_paused().is_empty());
    }
}

#[test]
fn i5_nested_record_mode_keeps_recording() {
    need!(Need::Zsh, Need::Tmux, Need::Plugin, Need::Recorder, Need::Root);
    let team = TeamInstall::install(&binary::cinelog(Flavour::Plain).unwrap()).unwrap();
    let sb = Sandbox::with(SandboxOptions {
        record: vec!["nested = \"record\"".into()],
        ..SandboxOptions::default()
    })
    .unwrap();
    let t = sb.tmux().unwrap();
    let a = sb.start_recorded(&t, "main").unwrap();
    let line = format!("echo team-record-{}", unique_id());
    let (cast, nested) = elevated_session(&team, &sb, &a, "sudo -s", &line);
    check_team_session(&cast, sb.user());
    assert_eq!(nested.str("mode"), Some("record"), "{}", nested.json);
    a.run("echo back").unwrap();
    let c = a.cast().unwrap();
    assert!(
        c.markers_of("pause").iter().all(|p| p.index < nested.index),
        "nested=record must not pause"
    );
    assert!(
        c.output().contains(&line["echo ".len()..]),
        "root output is copied into the user cast"
    );
}
