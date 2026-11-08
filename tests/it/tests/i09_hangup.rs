//! I9 [rec]: hangup matrix. Whatever ends the session (kill-pane, SIGTERM, SIGHUP, a shell
//! exit that leaves a HUP-ignoring job holding the slave, kill-pane with a running job), the
//! recorder writes `x`, removes its registry entry and exits, and the shell is gone.

use nix::sys::signal::Signal;

use cinelog_it::{Need, Recorded, Sandbox, need, procs, scaled};

enum End {
    KillPane,
    Signal(Signal),
    /// Typed into the shell before ending it by kill-pane.
    KillPaneWithJob(&'static str),
    /// Typed into the shell; the shell exits by itself.
    Type(&'static str),
}

fn hangup(end: End) -> (String, Sandbox) {
    let sb = Sandbox::new().unwrap();
    let t = sb.tmux().unwrap();
    let r: Recorded = sb.start_recorded(&t, "main").unwrap();
    r.run("echo up").unwrap();
    let shell = r.cast().unwrap().shell_pid().expect("shell_pid");
    let rec = r.pane.pid;
    match end {
        End::KillPane => r.pane.kill().unwrap(),
        End::Signal(sig) => assert!(procs::signal(rec, sig)),
        End::KillPaneWithJob(job) => {
            r.run(job).unwrap();
            r.pane.kill().unwrap();
        }
        End::Type(line) => r.pane.type_line(line).unwrap(),
    }
    let x = r.wait_exit(scaled(10.0)).unwrap();
    assert!(x.parse::<i32>().is_ok(), "x carries an exit status: {x:?}");
    assert!(
        procs::wait_gone(shell, scaled(5.0)),
        "shell {shell} outlived its recorder"
    );
    assert!(
        !sb.registry()
            .iter()
            .any(|(_, e)| e["pid"].as_u64() == Some(u64::from(rec))),
        "registry entry removed on exit"
    );
    let c = r.cast().unwrap();
    assert_eq!(
        c.events.iter().filter(|e| e.code == "x").count(),
        1,
        "exactly one x event"
    );
    (x, sb)
}

#[test]
fn i9_kill_pane() {
    need!(Need::Zsh, Need::Tmux, Need::Plugin, Need::Recorder);
    hangup(End::KillPane);
}

#[test]
fn i9_sigterm() {
    need!(Need::Zsh, Need::Tmux, Need::Plugin, Need::Recorder);
    hangup(End::Signal(Signal::SIGTERM));
}

#[test]
fn i9_sighup() {
    need!(Need::Zsh, Need::Tmux, Need::Plugin, Need::Recorder);
    hangup(End::Signal(Signal::SIGHUP));
}

#[test]
fn i9_exit_with_a_disowned_hup_ignoring_job() {
    need!(Need::Zsh, Need::Tmux, Need::Plugin, Need::Recorder);
    // The job keeps the slave open, so the master never reaches EOF: the recorder must
    // exit on "child reaped" and report the shell's own status.
    let (x, _sb) = hangup(End::Type("(trap '' HUP; exec sleep 1003) & disown; exit 3"));
    assert_eq!(x, "3");
}

#[test]
fn i9_kill_pane_with_a_background_job() {
    need!(Need::Zsh, Need::Tmux, Need::Plugin, Need::Recorder);
    hangup(End::KillPaneWithJob("sleep 1004 &"));
}
