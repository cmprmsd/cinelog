//! I20: pane recorders track tmux context: a `ctx` marker at start, and new ones after
//! `rename-session` and `break-pane` (checked every 30 s, §5.8).

use cinelog_it::{Marker, Need, Recorded, Sandbox, need, scaled};

fn latest_ctx(r: &Recorded, pred: impl Fn(&Marker) -> bool) -> Marker {
    // The recorder polls tmux every 30 s; allow two periods.
    r.wait_cast("a matching ctx marker", scaled(70.0), |c| {
        c.markers_of("ctx").into_iter().rev().find(|m| pred(m))
    })
    .unwrap()
}

#[test]
fn i20_rename_session_and_break_pane_write_ctx_markers() {
    need!(Need::Zsh, Need::Tmux, Need::Plugin, Need::Recorder);
    let sb = Sandbox::new().unwrap();
    let t = sb.tmux().unwrap();
    let outer = sb.start_recorded(&t, "main").unwrap();
    let inner = sb.inner_tmux("ctx");
    outer
        .run(&format!(
            "{} new-session -d -s alpha -n first -x 100 -y 30",
            Sandbox::tmux_cmd(&inner)
        ))
        .unwrap();
    let first = inner.panes().unwrap().remove(0);
    let second = inner.split(&first.id).unwrap();
    let p1 = Recorded::attach(&sb, first).unwrap();
    let p2 = Recorded::attach(&sb, second).unwrap();

    for (r, pane_index) in [(&p1, "0"), (&p2, "1")] {
        let ctx = latest_ctx(r, |_| true);
        assert_eq!(ctx.str("session_name"), Some("alpha"), "{}", ctx.json);
        assert_eq!(ctx.str("window_index"), Some("0"));
        assert_eq!(ctx.str("window_name"), Some("first"));
        assert_eq!(ctx.str("pane_index"), Some(pane_index));
        assert_eq!(ctx.str("pane_id"), Some(r.pane.id.as_str()));
    }

    inner.run(&["rename-session", "-t", "alpha", "beta"]).unwrap();
    inner
        .run(&["break-pane", "-d", "-s", &p2.pane.id, "-n", "broken"])
        .unwrap();

    let c1 = latest_ctx(&p1, |m| m.str("session_name") == Some("beta"));
    assert_eq!(c1.str("window_index"), Some("0"));
    assert_eq!(c1.str("pane_id"), Some(p1.pane.id.as_str()));
    let c2 = latest_ctx(&p2, |m| m.str("window_name") == Some("broken"));
    assert_eq!(c2.str("session_name"), Some("beta"), "{}", c2.json);
    assert_eq!(c2.str("window_index"), Some("1"));
    assert_eq!(c2.str("pane_index"), Some("0"));
    assert_eq!(
        c2.str("pane_id"),
        Some(p2.pane.id.as_str()),
        "pane identity survives the move"
    );

    // Both panes stay recorded into their own casts.
    p2.run("echo after-break").unwrap();
    assert!(p1.cast().unwrap().cmd("echo after-break").is_none());
}
