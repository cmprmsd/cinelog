//! I10: `zsh` inside a recorded shell is detected as `recorded`, attaches to the same cast
//! via the registry, and its commands get distinct seqs nested under the outer `zsh` command.

use cinelog_it::{Need, Sandbox, need, scaled, unique_id};

#[test]
fn i10_nested_zsh_shares_the_cast_with_distinct_seqs() {
    need!(Need::Zsh, Need::Tmux, Need::Plugin, Need::Recorder);
    let sb = Sandbox::new().unwrap();
    let t = sb.tmux().unwrap();
    let r = sb.start_recorded(&t, "main").unwrap();
    let uuid = r.uuid().unwrap();
    let tag = unique_id();

    r.run(&format!("echo outer-1-{tag}")).unwrap();
    r.pane.type_line("zsh").unwrap();
    let inner_hello = r
        .wait_cast("the inner shell's hello", scaled(15.0), |c| {
            c.markers_of("hello").into_iter().nth(1)
        })
        .unwrap();
    r.pane.wait_prompt(scaled(10.0)).unwrap();
    let check = "cinelog rec --check";
    r.run(check).unwrap();
    let expect = format!("recorded {uuid}");
    assert!(
        r.pane.capture().unwrap().lines().any(|l| l == expect),
        "rec --check in the nested shell prints {expect:?}"
    );
    r.run(&format!("echo inner-1-{tag}")).unwrap();
    r.run(&format!("echo inner-2-{tag}")).unwrap();
    // `exit` runs preexec but never reaches precmd: no `done` for it.
    r.pane.type_line("exit").unwrap();
    r.wait_done("zsh", 0, scaled(10.0)).unwrap();
    r.run(&format!("echo outer-2-{tag}")).unwrap();

    assert_eq!(
        sb.cast_paths(),
        vec![r.cast_path.clone()],
        "the nested shell started no cast"
    );
    let c = r.cast().unwrap();
    let hellos = c.markers_of("hello");
    assert_eq!(hellos.len(), 2);
    let (outer_sh, inner_sh) = (hellos[0].u64("sh").unwrap(), inner_hello.u64("sh").unwrap());
    assert_ne!(outer_sh, inner_sh);
    assert_eq!(
        inner_hello.u64("ppid"),
        Some(outer_sh),
        "inner zsh is a child of the outer one"
    );

    let tree = c.command_tree();
    let seqs: Vec<u64> = tree.iter().map(|n| n.seq).collect();
    assert!(
        seqs.windows(2).all(|w| w[0] < w[1]),
        "distinct increasing seqs: {seqs:?}"
    );
    let node = |typed: &str| {
        tree.iter()
            .find(|n| n.typed == typed)
            .unwrap_or_else(|| panic!("{typed} missing"))
    };
    let zsh = node("zsh");
    assert_eq!((zsh.sh, zsh.parent_seq), (outer_sh, None));
    for (i, typed) in [
        check.to_owned(),
        format!("echo inner-1-{tag}"),
        format!("echo inner-2-{tag}"),
        "exit".into(),
    ]
    .iter()
    .enumerate()
    {
        let n = node(typed);
        assert_eq!(n.sh, inner_sh, "{typed}");
        assert_eq!(n.n, i as u64 + 1, "the inner shell counts from 1: {typed}");
        assert_eq!(n.parent_seq, Some(zsh.seq), "{typed} nests under `zsh`");
        assert_eq!(n.depth, 1);
    }
    for typed in [format!("echo outer-1-{tag}"), format!("echo outer-2-{tag}")] {
        let n = node(&typed);
        assert_eq!((n.sh, n.parent_seq, n.depth), (outer_sh, None, 0), "{typed}");
    }
    assert!(zsh.exit.is_some(), "the outer `zsh` command was closed by its done");
}
