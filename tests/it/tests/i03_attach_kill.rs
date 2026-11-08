//! I3 [R2, R9]: two recorded windows attach to the same tmux session; killing the first
//! causes no cross-writes, its recorder writes `x` and exits, and nothing is orphaned.

use cinelog_it::{Need, Recorded, Sandbox, cast_timeout, need, procs, scaled, unique_id};

fn pause_reason(r: &Recorded) -> String {
    r.wait_marker("pause", |_| true)
        .unwrap()
        .str("reason")
        .unwrap_or_default()
        .to_owned()
}

#[test]
fn i3_second_attach_then_kill_first_window() {
    need!(Need::Zsh, Need::Tmux, Need::Plugin, Need::Recorder);
    let sb = Sandbox::new().unwrap();
    let t = sb.tmux().unwrap();
    let a = sb.start_recorded(&t, "main").unwrap();
    let b = sb.start_recorded(&t, "main").unwrap();
    let inner = sb.inner_tmux("shared");
    let tc = Sandbox::tmux_cmd(&inner);
    let tag = unique_id();

    // Window A creates and attaches the shared session; its recorder pauses.
    a.pane
        .type_line(&format!("{tc} new-session -s I -x 100 -y 30"))
        .unwrap();
    assert_eq!(pause_reason(&a), "multiplexer");
    let p_pane = cinelog_it::wait_until("the shared pane", cast_timeout(), || {
        inner.panes().ok()?.into_iter().next()
    })
    .unwrap();
    let p = Recorded::attach(&sb, p_pane).unwrap();

    // Window B attaches too.
    b.pane.type_line(&format!("{tc} attach -t I")).unwrap();
    assert_eq!(pause_reason(&b), "multiplexer");
    cinelog_it::wait_until("two clients", cast_timeout(), || {
        (inner.clients().ok()?.len() == 2).then_some(())
    })
    .unwrap();

    let both = format!("echo both-attached-{tag}");
    p.run(&both).unwrap();

    let a_shell = a.cast().unwrap().shell_pid().expect("shell_pid in session marker");
    a.pane.kill().unwrap();
    let x = a.wait_exit(scaled(10.0)).unwrap();
    assert!(x.parse::<i32>().is_ok(), "x event carries a status: {x:?}");
    assert!(procs::wait_gone(a_shell, scaled(5.0)), "A's shell survived its pane");
    cinelog_it::wait_until("A's tmux client to go away", scaled(5.0), || {
        (inner.clients().ok()?.len() == 1).then_some(())
    })
    .unwrap();
    assert!(
        !sb.registry()
            .iter()
            .any(|(_, e)| e["pid"].as_u64() == Some(u64::from(a.pane.pid))),
        "A's registry entry was removed"
    );

    let after = format!("echo after-kill-{tag}");
    p.run(&after).unwrap();

    let (ca, cb, cp) = (a.cast().unwrap(), b.cast().unwrap(), p.cast().unwrap());
    for typed in [&both, &after] {
        assert!(cp.cmd(typed).is_some(), "{typed} is in the shared pane's cast");
        for (name, c) in [("A", &ca), ("B", &cb)] {
            assert!(c.cmd(typed).is_none(), "{typed} leaked as a command into {name}");
            assert!(
                !c.output().contains(&typed["echo ".len()..]),
                "{typed} output leaked into {name} (it was paused)"
            );
        }
    }
    assert!(ca.events_while_paused().is_empty());
    assert!(cb.events_while_paused().is_empty());
    assert_eq!(ca.exit_status(), Some(x.as_str()));
    assert!(cb.exit_status().is_none(), "B is still recording");
    assert!(procs::alive(b.pane.pid));
}
