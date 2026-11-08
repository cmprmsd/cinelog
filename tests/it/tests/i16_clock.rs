//! I16: a realtime step (+1 h, injected through the `testhooks` build's
//! `CINELOG_TESTHOOK_CLOCK_FILE`) makes the recorder write a `clock{reason:step}` marker at
//! once, and the piecewise mapping of cast offsets to wall time (§5.4) stays correct.

use std::fs;

use cinelog_it::{Cast, Flavour, Marker, Need, Recorded, Sandbox, SandboxOptions, need, scaled};

const STEP_US: i64 = 3_600_000_000;

/// Wall time (µs) of a cast offset: the last `session`/`clock` marker before it plus the
/// elapsed interval time.
fn wall_us(c: &Cast, time: f64) -> i64 {
    let base = c
        .markers()
        .into_iter()
        .rev()
        .find(|m| m.time <= time && (m.kind == "session" || m.kind == "clock"))
        .expect("a session marker");
    let epoch = if base.kind == "session" {
        base.i64("start_epoch_us").expect("start_epoch_us")
    } else {
        base.i64("epoch_us").expect("epoch_us")
    };
    epoch + ((time - base.time) * 1e6) as i64
}

/// `$EPOCHREALTIME` of a `cmd` (the plugin's real clock, which the hook does not shift).
fn shell_time_us(cmd: &Marker) -> i64 {
    (cmd.f64("t").expect("cmd.t") * 1e6) as i64
}

fn assert_close(what: &str, got_us: i64, want_us: i64, tolerance_s: f64) {
    let diff = (got_us - want_us) as f64 / 1e6;
    assert!(diff.abs() <= tolerance_s, "{what}: off by {diff:.3}s");
}

#[test]
fn i16_clock_step_writes_a_marker_and_keeps_the_mapping() {
    need!(Need::Zsh, Need::Tmux, Need::Plugin, Need::Recorder, Need::Testhooks);
    let sb = Sandbox::with(SandboxOptions {
        flavour: Some(Flavour::Testhooks),
        ..SandboxOptions::default()
    })
    .unwrap();
    let offset = sb.work.join("clock-offset-us");
    fs::write(&offset, "0").unwrap();
    let t = sb.tmux().unwrap();
    // The recorder runs directly: an outer plugin would unset the CINELOG_* test hook.
    let argv = sb.direct_rec_argv(
        sb.bin.as_ref().unwrap(),
        &[("CINELOG_TESTHOOK_CLOCK_FILE", &offset.to_string_lossy())],
    );
    let pane = t.new_session("main", 120, 40, &argv).unwrap();
    let r = Recorded::attach(&sb, pane).unwrap();

    let (before, _) = r.run("echo before-step").unwrap();
    fs::write(&offset, STEP_US.to_string()).unwrap();
    let step = r
        .wait_cast("clock{step}", scaled(15.0), |c| {
            c.markers_of("clock")
                .into_iter()
                .find(|m| m.str("reason") == Some("step"))
        })
        .unwrap();
    let (after, _) = r.run("echo after-step").unwrap();
    assert!(after.index > step.index);

    let c = r.cast().unwrap();
    let session = c.session().unwrap();
    assert_close(
        "boottime advances with the cast offset",
        step.i64("boottime_us").unwrap() - session.i64("boottime_us").unwrap(),
        (step.time * 1e6) as i64,
        1.0,
    );
    assert_close(
        "the step marker carries the stepped realtime",
        step.i64("epoch_us").unwrap(),
        wall_us(&c, before.time) + ((step.time - before.time) * 1e6) as i64 + STEP_US,
        3.0,
    );
    assert_close(
        "mapping before the step",
        wall_us(&c, before.time),
        shell_time_us(&before),
        1.0,
    );
    assert_close(
        "mapping after the step",
        wall_us(&c, after.time),
        shell_time_us(&after) + STEP_US,
        1.0,
    );
}
