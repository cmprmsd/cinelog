//! I11 (M2 accept 4): with `cinelog serve --no-http` running, a command typed in a recorded
//! pane and its output are found by `cinelog search` within 3 s, also while the command is
//! still running (provisional chunk, plan §6.4 "Live").

use std::time::{Duration, Instant};

use cinelog_it::{Need, Recorded, Sandbox, SandboxOptions, Serve, need, scaled, wait_until};
use serde_json::Value;

/// The 3 s budget of the plan, measured from pressing Enter.
fn budget() -> Duration {
    scaled(3.0)
}

/// The first screen line that starts with `prefix` and continues with digits only.
fn printed_value(r: &Recorded, prefix: &str) -> String {
    wait_until(&format!("a {prefix}<n> line on the screen"), scaled(5.0), || {
        r.pane.capture().ok()?.lines().map(str::trim).find_map(|l| {
            let rest = l.strip_prefix(prefix)?;
            (!rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit())).then(|| l.to_owned())
        })
    })
    .unwrap()
}

fn is_command(h: &Value, uuid: &str, typed: &str) -> bool {
    h["kind"] == "command" && h["session_uuid"] == uuid && h["typed"] == typed
}

fn is_output_of(h: &Value, uuid: &str, seq: u64) -> bool {
    h["kind"] == "output" && h["session_uuid"] == uuid && h["cmd_seq"] == seq
}

/// `echo needle-$RANDOM` in `r`: the command and its output are searchable within budget.
fn finished_command_is_found(sb: &Sandbox, r: &Recorded, word: &str) {
    let uuid = r.uuid().unwrap();
    let typed = format!("echo {word}-$RANDOM");
    let enter = Instant::now();
    let (cmd, done) = r.run(&typed).unwrap();
    let seq = cmd.u64("seq").unwrap();
    assert_eq!(done.i64("exit"), Some(0));
    let value = printed_value(r, &format!("{word}-"));

    let (hit, _) = sb
        .wait_hit(&[&format!("{word}-")], budget().saturating_sub(enter.elapsed()), |h| {
            is_command(h, &uuid, &typed)
        })
        .unwrap();
    assert_eq!(hit["cmd_seq"], seq);
    assert_eq!(hit["exit"], 0);
    assert_eq!(hit["entry"], format!("{uuid}:{seq}"));
    let (out_hit, _) = sb
        .wait_hit(&["-a", &value], budget().saturating_sub(enter.elapsed()), |h| {
            is_output_of(h, &uuid, seq)
        })
        .unwrap();
    assert!(
        out_hit["snippet"].as_str().is_some_and(|s| s.contains(&value)),
        "{out_hit}"
    );
    let took = enter.elapsed();
    assert!(took <= budget(), "{word}: found after {took:?}");
    eprintln!(
        "I11 {word}: command and output searchable {:.0} ms after Enter",
        took.as_secs_f64() * 1e3
    );

    let show = sb
        .cinelog(&["show", &format!("{uuid}:{seq}")])
        .unwrap()
        .expect_ok()
        .unwrap();
    assert!(show.stdout.contains(&typed) && show.stdout.contains(&value), "{show}");
}

#[test]
fn i11_live_commands_and_output_are_searchable_within_3s() {
    need!(Need::Zsh, Need::Tmux, Need::Plugin, Need::Recorder, Need::Index);
    let sb = Sandbox::new().unwrap();
    let t = sb.tmux().unwrap();
    // One session exists before the indexer starts, one starts while it runs.
    let early = sb.start_recorded(&t, "early").unwrap();
    let serve = Serve::start(&sb, "serve", &[]).unwrap();
    wait_until("serve to hold serve.lock", scaled(15.0), || {
        let s = sb.status_json().ok()?;
        (s["daemon"]["serve_lock_held"] == true).then_some(())
    })
    .unwrap_or_else(|e| panic!("{e:#}\n{}", serve.log_text()));
    let late = sb.start_recorded(&t, "late").unwrap();

    finished_command_is_found(&sb, &early, "needle");
    finished_command_is_found(&sb, &late, "haystack");

    // A running command: its row and its on-screen output are searchable before `done`.
    let uuid = late.uuid().unwrap();
    let typed = "echo running-$RANDOM; sleep 600";
    let enter = Instant::now();
    late.pane.type_line(typed).unwrap();
    let cmd = late.wait_marker("cmd", |m| m.str("typed") == Some(typed)).unwrap();
    let seq = cmd.u64("seq").unwrap();
    let value = printed_value(&late, "running-");
    sb.wait_hit(&["running-"], budget().saturating_sub(enter.elapsed()), |h| {
        is_command(h, &uuid, typed) && h["exit"].is_null()
    })
    .unwrap();
    let (live_hit, _) = sb
        .wait_hit(&["--out", &value], budget().saturating_sub(enter.elapsed()), |h| {
            is_output_of(h, &uuid, seq)
        })
        .unwrap();
    assert!(
        enter.elapsed() <= budget(),
        "running command found after {:?}",
        enter.elapsed()
    );
    eprintln!("I11 running command: output chunk kind {}", live_hit["chunk_kind"]);

    // Interrupted: the provisional rows give way to the final ones, still findable.
    late.pane.send_keys(&["C-c"]).unwrap();
    late.wait_marker("done", |m| m.u64("seq") == Some(seq)).unwrap();
    let (final_hit, _) = sb
        .wait_hit(&["--out", &value], scaled(5.0), |h| {
            is_output_of(h, &uuid, seq) && h["chunk_kind"] != "provisional"
        })
        .unwrap();
    assert_eq!(final_hit["chunk_kind"], "primary", "{final_hit}");
    sb.wait_hit(&["running-"], scaled(5.0), |h| {
        is_command(h, &uuid, typed) && h["exit"].as_i64().is_some_and(|e| e != 0)
    })
    .unwrap();

    let status = sb.status_json().unwrap();
    assert!(status["index"]["commands"].as_u64().unwrap() >= 3, "{status}");
    let st = serve.stop(scaled(10.0)).unwrap();
    assert!(st.success(), "serve exits cleanly on SIGTERM: {st:?}");
    let status = sb.status_json().unwrap();
    assert_eq!(status["daemon"]["serve_lock_held"], false, "{status}");
}

/// A search that keeps failing is reported as that failure, not as a bare timeout.
#[test]
fn i11_wait_hit_reports_the_failing_search() {
    need!(Need::Index);
    let sb = Sandbox::new().unwrap();
    // No indexer ran, so there is no index.db and every search fails.
    let err = sb
        .wait_hit(&["anything"], Duration::from_millis(300), |_| true)
        .unwrap_err();
    let msg = format!("{err:#}");
    assert!(msg.contains("the last search failed"), "{msg}");
    assert!(msg.contains("no index yet"), "{msg}");
}

/// Autostart (§5.1): the first recorded shell starts the indexer itself (`serve --ensure`
/// from the recorder, detached), and its commands become searchable. A second shell finds
/// the lock held and starts nothing.
#[test]
fn i11_a_recorded_shell_starts_the_indexer() {
    need!(Need::Zsh, Need::Tmux, Need::Plugin, Need::Recorder, Need::Index);
    let sb = Sandbox::with(SandboxOptions {
        autostart: "shell",
        ..SandboxOptions::default()
    })
    .unwrap();
    let t = sb.tmux().unwrap();
    let first = sb.start_recorded(&t, "first").unwrap();
    let held = |sb: &Sandbox| sb.status_json().ok().map(|s| s["daemon"]["serve_lock_held"] == true);
    wait_until("the recorder to start serve", scaled(15.0), || held(&sb)?.then_some(())).unwrap();
    let writer = |sb: &Sandbox| sb.status_json().unwrap()["index"]["info"]["writer_pid"].clone();
    let pid = writer(&sb);
    finished_command_is_found(&sb, &first, "autostarted");
    let second = sb.start_recorded(&t, "second").unwrap();
    finished_command_is_found(&sb, &second, "second");
    assert!(pid.is_string(), "{pid}");
    assert_eq!(writer(&sb), pid, "one server");
}
