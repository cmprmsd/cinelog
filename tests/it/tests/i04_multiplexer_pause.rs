//! I4: the outer recorder pauses while a tmux client runs in its foreground (`tmux attach`,
//! `exec tmux`, `sh -c 'sleep 1; tmux a'`), records no `o`/`i` while paused, and leaves the
//! playback on the primary screen after resuming (alt-screen fix-up).

use cinelog_it::{Marker, Need, Recorded, Sandbox, Tmux, cast_timeout, need, scaled, unique_id, wait_until};

struct Setup {
    _sb: Sandbox,
    inner: Tmux,
    outer: Recorded,
    shared: Recorded,
}

/// A recorded outer shell plus a detached inner tmux session `I` with a recorded pane.
fn setup() -> Setup {
    let sb = Sandbox::new().unwrap();
    let t = sb.tmux().unwrap();
    let outer = sb.start_recorded(&t, "main").unwrap();
    let inner = sb.inner_tmux("mux");
    outer
        .run(&format!(
            "{} new-session -d -s I -x 100 -y 30",
            Sandbox::tmux_cmd(&inner)
        ))
        .unwrap();
    let pane = inner.panes().unwrap().into_iter().next().unwrap();
    let shared = Recorded::attach(&sb, pane).unwrap();
    Setup {
        _sb: sb,
        inner,
        outer,
        shared,
    }
}

fn wait_pause(r: &Recorded, after: usize) -> Marker {
    r.wait_cast("pause{multiplexer}", scaled(10.0), |c| {
        c.markers_of("pause")
            .into_iter()
            .find(|m| m.index > after && m.str("reason") == Some("multiplexer"))
    })
    .unwrap()
}

fn wait_resume(r: &Recorded, after: usize) -> Marker {
    r.wait_cast("resume", scaled(10.0), |c| {
        c.markers_of("resume").into_iter().find(|m| m.index > after)
    })
    .unwrap()
}

fn last_index(r: &Recorded) -> usize {
    r.cast().unwrap().events.len().saturating_sub(1)
}

/// While attached, typing reaches the inner pane but not the outer cast.
fn type_through_client(s: &Setup, tag: &str) -> String {
    let line = format!("echo through-client-{tag}");
    s.outer.pane.type_line(&line).unwrap();
    s.shared.wait_done(&line, 0, cast_timeout()).unwrap();
    line
}

fn assert_paused_cleanly(outer: &Recorded, pause: &Marker, resume: Option<&Marker>, secret_text: &str) {
    let c = outer.cast().unwrap();
    let mux = c
        .markers_of("multiplexer")
        .into_iter()
        .rfind(|m| m.index < pause.index)
        .expect("a multiplexer marker precedes the pause");
    assert_eq!(mux.str("comm"), Some("tmux: client"), "{}", mux.json);
    let leaked = c.events_while_paused();
    assert!(leaked.is_empty(), "o/i events while paused: {leaked:?}");
    assert!(!c.output().contains(secret_text), "client output was recorded");
    assert!(!c.input().contains(secret_text), "client keystrokes were recorded");
    if let Some(resume) = resume {
        let mut upto = resume.index;
        if resume.flag("alt_fixup") {
            let fix = &c.events[resume.index + 1];
            assert_eq!(fix.code, "o");
            assert!(fix.data.starts_with("\x1b[?1049l"), "fix-up event {fix:?}");
            upto += 1;
        }
        assert!(
            !c.alt_screen_after(upto),
            "playback is left on the alternate screen after the resume"
        );
    }
}

#[test]
fn i4_tmux_attach_pauses_and_resumes() {
    need!(Need::Zsh, Need::Tmux, Need::Plugin, Need::Recorder);
    let s = setup();
    let tag = unique_id();
    let start = last_index(&s.outer);
    s.outer
        .pane
        .type_line(&format!("{} attach -t I", Sandbox::tmux_cmd(&s.inner)))
        .unwrap();
    let pause = wait_pause(&s.outer, start);
    let typed = type_through_client(&s, &tag);
    s.inner.run(&["detach-client", "-s", "I"]).unwrap();
    let resume = wait_resume(&s.outer, pause.index);
    assert_eq!(resume.str("reason"), Some("foreground"), "{}", resume.json);
    s.outer.run(&format!("echo back-{tag}")).unwrap();
    assert_paused_cleanly(&s.outer, &pause, Some(&resume), &typed["echo ".len()..]);
    assert!(s.outer.cast().unwrap().output().contains(&format!("back-{tag}")));
}

#[test]
fn i4_exec_tmux_pauses_until_the_recording_ends() {
    need!(Need::Zsh, Need::Tmux, Need::Plugin, Need::Recorder);
    let s = setup();
    let tag = unique_id();
    let shell = s.outer.cast().unwrap().shell_pid().expect("shell_pid");
    s.outer
        .pane
        .type_line(&format!("exec {} attach -t I", Sandbox::tmux_cmd(&s.inner)))
        .unwrap();
    // `exec` keeps the pgrp and changes the leader's comm: still detected.
    let pause = wait_pause(&s.outer, 0);
    let typed = type_through_client(&s, &tag);
    let (tty, _) = wait_until("the exec'd client", cast_timeout(), || {
        s.inner.clients().ok()?.into_iter().find(|(_, pid)| *pid == shell)
    })
    .unwrap();
    s.inner.run(&["detach-client", "-t", &tty]).unwrap();
    let x = s.outer.wait_exit(scaled(10.0)).unwrap();
    assert!(x.parse::<i32>().is_ok());
    assert_paused_cleanly(&s.outer, &pause, None, &typed["echo ".len()..]);
}

#[test]
fn i4_delayed_attach_from_sh_c_is_caught_by_the_rescan() {
    need!(Need::Zsh, Need::Tmux, Need::Plugin, Need::Recorder);
    let s = setup();
    let tag = unique_id();
    let line = format!("sh -c 'sleep 1; {} a -t I'", Sandbox::tmux_cmd(&s.inner));
    s.outer.pane.type_line(&line).unwrap();
    let cmd = s
        .outer
        .wait_cast("the sh -c cmd", cast_timeout(), |c| c.cmd(&line))
        .unwrap();
    // `sh -c` keeps its pgrp; tmux appears only later and is found by the 1 s rescan.
    let pause = wait_pause(&s.outer, cmd.index);
    assert!(pause.time - cmd.time >= 0.9, "paused before tmux even started");
    let typed = type_through_client(&s, &tag);
    s.inner.run(&["detach-client", "-s", "I"]).unwrap();
    let resume = wait_resume(&s.outer, pause.index);
    s.outer.wait_done(&line, 0, cast_timeout()).unwrap();
    s.outer.run(&format!("echo back-{tag}")).unwrap();
    assert_paused_cleanly(&s.outer, &pause, Some(&resume), &typed["echo ".len()..]);
}
