//! I15: the log filesystem fills up mid-session (1 MiB tmpfs in a private mount namespace).
//! The shell keeps working, the cast gets an `error` marker, the registry says `degraded`,
//! the plugin warns once, and `cinelog resume` opens part 2 once space is available.

use std::fs;
use std::path::Path;

use cinelog_it::{Cast, Need, Recorded, Sandbox, SandboxOptions, need, scaled, wait_until};
use serde_json::Value;

fn entry_of(sb: &Sandbox, rec_pid: u32) -> Option<Value> {
    sb.registry()
        .into_iter()
        .map(|(_, e)| e)
        .find(|e| e["pid"].as_u64() == Some(u64::from(rec_pid)))
}

#[test]
fn i15_disk_full_degrades_and_resume_opens_part_two() {
    need!(Need::Zsh, Need::Tmux, Need::UserNs, Need::Plugin, Need::Recorder);
    let sb = Sandbox::with(SandboxOptions {
        logs: Some("logs-tmpfs".into()),
        ..SandboxOptions::default()
    })
    .unwrap();
    let t = sb.tmux().unwrap();
    let argv = sb.tmpfs_argv(&sb.logs, "1m", 600 * 1024, sb.shell_argv());
    let pane = t.new_session("main", 120, 40, &argv).unwrap();
    sb.view_through(pane.pid);
    let r = Recorded::attach(&sb, pane).unwrap();
    r.run("echo before-full").unwrap();

    // ~1.5 MB of output cannot fit in the remaining ~400 KiB.
    r.pane
        .run_sync("head -c 1500000 /dev/zero | tr '\\0' x; echo", scaled(30.0))
        .unwrap();
    t.run(&["clear-history", "-t", &r.pane.id]).unwrap();
    let entry = wait_until("registry state degraded", scaled(15.0), || {
        entry_of(&sb, r.pane.pid).filter(|e| e["state"] == "degraded")
    })
    .unwrap();
    assert_eq!(entry["part"].as_u64().unwrap_or(1), 1);

    // The shell keeps working; the plugin's precmd warns exactly once.
    r.pane.run_sync("echo still-alive-$((1+1))", scaled(10.0)).unwrap();
    r.pane.run_sync("true", scaled(10.0)).unwrap();
    let screen = r.pane.capture().unwrap();
    assert!(screen.lines().any(|l| l == "still-alive-2"));
    let warnings = cinelog_it::util::warning_lines(&screen);
    assert_eq!(warnings.len(), 1, "one degraded warning: {warnings:?}\n{screen}");

    // Free space, then resume into a continuation part.
    fs::remove_file(sb.visible(&sb.logs.join("filler"))).unwrap();
    r.pane.run_sync("cinelog resume", scaled(10.0)).unwrap();
    let entry = wait_until("registry state live in part 2", scaled(10.0), || {
        entry_of(&sb, r.pane.pid).filter(|e| e["state"] == "live" && e["part"].as_u64() == Some(2))
    })
    .unwrap();
    let part2_path = sb.visible(Path::new(entry["cast"].as_str().expect("cast path")));
    assert_ne!(part2_path, r.cast_path);
    let part2 = Recorded {
        pane: r.pane.clone(),
        cast_path: part2_path.clone(),
    };
    part2.run("echo after-resume").unwrap();

    let p1 = r.cast().unwrap();
    let p2 = Cast::load(&part2_path).unwrap();
    let s2 = p2.session().expect("part 2 starts with a session marker");
    assert_eq!(s2.u64("part"), Some(2));
    assert_eq!(s2.str("uuid"), p1.uuid().as_deref(), "same logical session");
    let p1_name = r.cast_path.file_name().unwrap().to_string_lossy().into_owned();
    assert_eq!(s2.str("continues"), Some(p1_name.as_str()));
    assert!(
        part2_path
            .file_name()
            .unwrap()
            .to_string_lossy()
            .ends_with("_part2.cast")
    );

    // The I/O error is recorded: in part 1 if it still fit, else at the start of part 2.
    let error = p1
        .first_marker("error")
        .or_else(|| p2.first_marker("error"))
        .expect("an error marker");
    assert_eq!(error.str("error"), Some("enospc"), "{}", error.json);
    let seqs = |c: &Cast| {
        c.markers_of("cmd")
            .iter()
            .filter_map(|m| m.u64("seq"))
            .collect::<Vec<_>>()
    };
    let (s1, s2) = (seqs(&p1), seqs(&p2));
    assert!(
        s1.iter().chain(&s2).collect::<Vec<_>>().windows(2).all(|w| w[0] < w[1]),
        "seq continues across parts: {s1:?} {s2:?}"
    );
}
