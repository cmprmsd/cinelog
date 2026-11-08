//! I18: GNU screen inside a recorded shell pauses the outer recorder (multiplexer), and the
//! screen window's shell gets its own cast (holder `screen`).

use cinelog_it::{Need, Recorded, Sandbox, need, scaled, unique_id};

#[test]
fn i18_screen_pauses_outer_and_its_window_is_recorded() {
    need!(Need::Zsh, Need::Tmux, Need::Screen, Need::Plugin, Need::Recorder);
    let sb = Sandbox::new().unwrap();
    let t = sb.tmux().unwrap();
    let outer = sb.start_recorded(&t, "main").unwrap();
    let known = sb.cast_paths();
    let tag = unique_id();
    let start = outer.cast().unwrap().events.len();

    outer.pane.type_line(&format!("screen -S clit-{tag}")).unwrap();
    let pause = outer
        .wait_cast("pause{multiplexer}", scaled(10.0), |c| {
            c.markers_of("pause")
                .into_iter()
                .find(|m| m.index >= start && m.str("reason") == Some("multiplexer"))
        })
        .unwrap();
    let mux = outer
        .cast()
        .unwrap()
        .markers_of("multiplexer")
        .into_iter()
        .rfind(|m| m.index < pause.index)
        .expect("multiplexer marker");
    assert_eq!(mux.str("comm"), Some("screen"), "{}", mux.json);

    let window_cast = sb
        .wait_new_cast(&known, scaled(15.0), |s| s.str("holder_comm") == Some("screen"))
        .unwrap();
    let window = Recorded {
        pane: outer.pane.clone(),
        cast_path: window_cast,
    };
    window
        .wait_cast("the window shell's hello", scaled(10.0), |c| c.first_marker("hello"))
        .unwrap();
    let line = format!("echo in-screen-{tag}");
    window.run(&line).unwrap();

    // Closing the only window ends screen; the outer shell is in the foreground again.
    window.pane.type_line("exit").unwrap();
    window
        .wait_cast("the window's x event", scaled(10.0), |c| {
            c.exit_status().map(str::to_owned)
        })
        .unwrap();
    let resume = outer
        .wait_cast("resume", scaled(10.0), |c| {
            c.markers_of("resume").into_iter().find(|m| m.index > pause.index)
        })
        .unwrap();
    assert_eq!(resume.str("reason"), Some("foreground"));
    outer.run(&format!("echo back-{tag}")).unwrap();

    let c = outer.cast().unwrap();
    assert!(c.cmd(&line).is_none());
    assert!(
        !c.output().contains(&format!("in-screen-{tag}")),
        "screen output in the outer cast"
    );
    assert!(c.events_while_paused().is_empty());
    let resumed_at = resume.index + usize::from(resume.flag("alt_fixup"));
    assert!(
        !c.alt_screen_after(resumed_at),
        "outer playback left on the alternate screen"
    );
}
