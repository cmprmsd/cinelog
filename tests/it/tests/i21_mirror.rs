//! I21 (§5.7): the recorder holds the shell's former pid, so tmux looks at the recorder.
//! It must mirror the inner cwd (`#{pane_current_path}`) and the inner foreground command
//! (`#{pane_current_command}`, automatic-rename), while its comm stays `cinelog`.

use std::path::Path;

use cinelog_it::{Need, Pane, Sandbox, need, procs, scaled, wait_until};

fn basename(s: &str) -> String {
    Path::new(s.trim_start_matches('-'))
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

fn wait_display(pane: &Pane, format: &str, want: &str) {
    wait_until(&format!("{format} == {want:?}"), scaled(5.0), || {
        let got = pane.display(format).ok()?;
        (got == want || basename(&got) == want).then_some(())
    })
    .unwrap_or_else(|e| panic!("{e:#}; last value {:?}", pane.display(format).unwrap_or_default()));
}

#[test]
fn i21_cwd_and_command_name_are_mirrored() {
    need!(Need::Zsh, Need::Tmux, Need::Plugin, Need::Recorder);
    let sb = Sandbox::new().unwrap();
    let t = sb.tmux().unwrap();
    let r = sb.start_recorded(&t, "main").unwrap();
    t.run(&["set-option", "-w", "-t", &r.pane.id, "automatic-rename", "on"])
        .unwrap();

    r.run("cd /tmp").unwrap();
    let tmp = std::fs::canonicalize("/tmp").unwrap();
    wait_display(&r.pane, "#{pane_current_path}", &tmp.to_string_lossy());

    r.run(&format!("cd {}", sb.work.display())).unwrap();
    let work = std::fs::canonicalize(&sb.work).unwrap();
    wait_display(&r.pane, "#{pane_current_path}", &work.to_string_lossy());

    r.pane.type_line("sleep 31").unwrap();
    wait_display(&r.pane, "#{pane_current_command}", "sleep");
    wait_display(&r.pane, "#{window_name}", "sleep");
    assert_eq!(
        procs::comm(r.pane.pid).as_deref(),
        Some("cinelog"),
        "comm stays cinelog for detection"
    );
    r.pane.send_keys(&["C-c"]).unwrap();
    r.wait_done("sleep 31", 0, scaled(5.0)).unwrap();
    wait_display(&r.pane, "#{pane_current_command}", "zsh");
}
