//! I2 [R1]: a recorded shell starts a tmux server; every pane (split-window, new-window)
//! gets its own cast with holder `tmux: server`, and commands land only in their pane's cast.

use cinelog_it::{Need, Recorded, Sandbox, need, unique_id};

#[test]
fn i2_each_tmux_pane_gets_its_own_cast() {
    need!(Need::Zsh, Need::Tmux, Need::Plugin, Need::Recorder);
    let sb = Sandbox::new().unwrap();
    let t = sb.tmux().unwrap();
    let outer = sb.start_recorded(&t, "main").unwrap();
    let inner = sb.inner_tmux("inner");
    let tc = Sandbox::tmux_cmd(&inner);

    outer.run(&format!("{tc} new-session -d -s w -x 100 -y 30")).unwrap();
    outer.run(&format!("{tc} split-window -t w")).unwrap();
    outer.run(&format!("{tc} new-window -t w")).unwrap();

    // The server inherited the recorded shell's environment: nothing of cinelog's may be in it.
    let server_env = inner.run(&["show-environment", "-g"]).unwrap();
    assert!(
        !server_env.lines().any(|l| l.starts_with("CINELOG_")),
        "tmux server inherited cinelog state:\n{server_env}"
    );

    let panes = inner.panes().unwrap();
    assert_eq!(panes.len(), 3, "{panes:?}");
    let recs: Vec<Recorded> = panes.into_iter().map(|p| Recorded::attach(&sb, p).unwrap()).collect();
    let tag = unique_id();
    for (i, r) in recs.iter().enumerate() {
        let session = r.cast().unwrap().session().expect("session marker");
        assert_eq!(session.str("holder_comm"), Some("tmux: server"), "{}", session.json);
        assert_eq!(session.str("scope"), Some("personal"));
        let hello = r.cast().unwrap().first_marker("hello").unwrap();
        if let Some(pane) = hello.get("tmux").and_then(|t| t.get("pane")).and_then(|p| p.as_str()) {
            assert_eq!(pane, r.pane.id, "hello names its own pane");
        }
        r.run(&format!("echo pane-{tag}-{i}")).unwrap();
    }

    let mut paths: Vec<_> = recs.iter().map(|r| r.cast_path.clone()).collect();
    paths.push(outer.cast_path.clone());
    paths.sort();
    paths.dedup();
    assert_eq!(paths.len(), 4, "outer + 3 panes, each its own cast");
    assert_eq!(sb.cast_paths().len(), 4, "no other casts: {:?}", sb.cast_paths());

    let uuids: std::collections::HashSet<String> = recs.iter().map(|r| r.uuid().unwrap()).collect();
    assert_eq!(uuids.len(), 3);

    let outer_cast = outer.cast().unwrap();
    for i in 0..recs.len() {
        let own = format!("pane-{tag}-{i}");
        for (j, other) in recs.iter().enumerate() {
            let c = other.cast().unwrap();
            let has_cmd = c.cmd(&format!("echo {own}")).is_some();
            let has_out = c.output().contains(&own);
            assert_eq!(has_cmd, i == j, "cmd of pane {i} in cast of pane {j}");
            assert_eq!(has_out, i == j, "output of pane {i} in cast of pane {j}");
        }
        assert!(
            !outer_cast.output().contains(&own),
            "pane output leaked into the outer cast"
        );
        assert!(outer_cast.cmd(&format!("echo {own}")).is_none());
    }
}
