//! Playback tests: the cast stream, segmented plans from indexed and from on-demand
//! segments, masking, and the player's validation rules.

use std::time::Duration;

use axum::http::{StatusCode, header};
use cinelog_cast::{CastReader, EventCode};
use serde_json::{Value, json};

use crate::api::testkit::{Cast, Kit, Meta, uuid, validate_segmented};

/// Many commands with idle gaps over 60 s (the indexer cuts a segment there), colours,
/// an alt-screen program and a resize.
fn long_cast(kit: &Kit, n: u32) -> Cast {
    let mut c = Cast::new(kit.dir.path(), n, 40, 8, &Meta::default());
    let mut t = 0.5;
    for i in 0..12u64 {
        let typed = format!("step{i}");
        let out: Vec<String> = (0..6).map(|k| format!("\x1b[3{}mline {i}.{k}\x1b[0m", k % 7)).collect();
        let out: Vec<&str> = out.iter().map(String::as_str).collect();
        t = c.command(t, i + 1, &typed, &out, (i % 2) as i32);
        match i % 4 {
            1 => {
                c.o(t + 0.01, "\x1b[?1049h\x1b[Hfull screen app\x1b[?1049l");
                t += 0.02;
            }
            2 => {
                c.ev(t + 0.01, EventCode::Resize, if i % 8 == 2 { "50x10" } else { "40x8" });
                t += 0.02;
            }
            3 => t += 75.0,
            _ => {}
        }
        t += 0.3;
    }
    c.command(t, 13, "echo secret-value", &["secret-value"], 0);
    c.exit(t + 1.0);
    c
}

async fn poll_ready(kit: &Kit, u: &str) -> Value {
    for _ in 0..400 {
        let info = kit.json(&format!("/sessions/{u}/play")).await;
        if info["ready"] == true {
            return info;
        }
        assert!(info["progress"].as_f64().is_some());
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("segments never became ready");
}

async fn segmented(kit: &Kit, u: &str) -> (Value, Vec<Value>) {
    let info = poll_ready(kit, u).await;
    assert_eq!(info["mode"], "segmented");
    assert_eq!(info["url"], format!("/api/sessions/{u}/play/index.json"));
    let index = kit.json(&format!("/sessions/{u}/play/index.json")).await;
    let n = index["segments"].as_array().unwrap().len();
    let mut segs = Vec::new();
    for i in 0..n {
        segs.push(kit.json(&format!("/sessions/{u}/play/seg/{i}")).await);
    }
    (index, segs)
}

/// All output data of a cast file, in order.
fn file_output(path: &std::path::Path) -> String {
    let mut r = cinelog_cast::open(path).unwrap();
    let mut s = String::new();
    while let Some(ev) = r.next_event() {
        let ev = ev.unwrap();
        if ev.code == EventCode::Output {
            s.push_str(&ev.data);
        }
    }
    s
}

fn seg_output(segs: &[Value]) -> String {
    segs.iter()
        .flat_map(|s| s["events"].as_array().unwrap().iter())
        .filter(|e| e[1] == "o")
        .map(|e| e[2].as_str().unwrap())
        .collect()
}

/// Segment `n` replayed from its snapshot ends in the state segment `n + 1` starts with.
fn assert_continuous(segs: &[Value]) {
    let screen = |vt: &avt::Terminal| -> Vec<String> { vt.screen().text() };
    for w in segs.windows(2) {
        let (a, b) = (&w[0], &w[1]);
        let size = |s: &Value| {
            (
                s["snapshot"]["cols"].as_u64().unwrap() as u16,
                s["snapshot"]["rows"].as_u64().unwrap() as u16,
            )
        };
        let mut sz = size(a);
        let mut vt = super::plan::terminal(sz, a["snapshot"]["init"].as_str().unwrap());
        for e in a["events"].as_array().unwrap() {
            let ev = super::stream::OutEvent::new(
                0,
                e[1].as_str().unwrap().chars().next().unwrap(),
                e[2].as_str().unwrap(),
            );
            super::plan::apply(&mut vt, &mut sz, &ev);
        }
        assert_eq!(sz, size(b), "size at the boundary");
        let next = super::plan::terminal(size(b), b["snapshot"]["init"].as_str().unwrap());
        assert_eq!(screen(&vt), screen(&next));
        assert_eq!(vt.cursor(), next.cursor());
    }
}

#[tokio::test]
async fn small_casts_stream_whole() {
    let kit = Kit::new(8);
    let c = long_cast(&kit, 1);
    kit.index(&c.path, None);
    let u = uuid(1);
    let info = kit.json(&format!("/sessions/{u}/play")).await;
    assert_eq!(info["mode"], "cast");
    assert_eq!(info["ready"], true);
    assert_eq!(info["url"], format!("/api/sessions/{u}/cast"));
    let (status, headers, body) = kit.get(&format!("/sessions/{u}/cast")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers[header::CONTENT_TYPE], "application/x-asciicast");
    let text = String::from_utf8(body).unwrap();
    let mut lines = text.lines();
    let h: Value = serde_json::from_str(lines.next().unwrap()).unwrap();
    assert!(
        text.starts_with(r#"{"version":3,"term":{"cols":40,"rows":8"#),
        "{text:.80}"
    );
    assert_eq!(h["timestamp"], 1_731_070_001);
    let events: Vec<Value> = lines.map(|l| serde_json::from_str(l).unwrap()).collect();
    let markers: Vec<&str> = events
        .iter()
        .filter(|e| e[1] == "m")
        .map(|e| e[2].as_str().unwrap())
        .collect();
    assert_eq!(markers.len(), 13, "only commands, as labels: {markers:?}");
    assert_eq!(markers[0], "$ step0");
    let output: String = events
        .iter()
        .filter(|e| e[1] == "o")
        .map(|e| e[2].as_str().unwrap())
        .collect();
    assert_eq!(output, file_output(&c.path));
    assert_eq!(events.last().unwrap()[1], "x");
    // Intervals sum to the cast's own times.
    let total: f64 = events.iter().map(|e| e[0].as_f64().unwrap()).sum();
    assert!(
        (total
            - kit.json(&format!("/sessions/{u}")).await["session"]["duration"]
                .as_f64()
                .unwrap())
        .abs()
            < 1e-3
    );

    let (status, _) = kit.post("/forget", &json!({"session_uuid": u, "cmd_seq": 13})).await;
    assert_eq!(status, StatusCode::OK);
    let (_, _, body) = kit.get(&format!("/sessions/{u}/cast")).await;
    let text = String::from_utf8(body).unwrap();
    assert!(!text.contains("secret-value"), "masked");
    assert!(text.contains("[redacted by alice]"));
    assert_eq!(text.matches("\"m\"").count(), 12);
}

#[tokio::test]
async fn indexed_segments_play_and_validate() {
    let kit = Kit::new(0);
    let c = long_cast(&kit, 1);
    kit.index(&c.path, None);
    let u = uuid(1);
    assert_eq!(
        kit.get(&format!("/sessions/{u}/cast")).await.0,
        StatusCode::PAYLOAD_TOO_LARGE
    );
    let detail = kit.json(&format!("/sessions/{u}")).await;
    assert_eq!(detail["play_mode"], "segmented");
    let (index, segs) = segmented(&kit, &u).await;
    assert!(segs.len() >= 3, "idle gaps cut segments: {}", segs.len());
    validate_segmented(&index, &segs);
    assert_continuous(&segs);
    assert_eq!(seg_output(&segs), file_output(&c.path));
    assert_eq!(index["markers"].as_array().unwrap().len(), 13);
    assert_eq!(index["term"], json!({"cols": 40, "rows": 8, "theme": null}));
    assert_eq!(
        kit.get(&format!("/sessions/{u}/play/seg/99")).await.0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        kit.get(&format!("/sessions/{u}/play/seg/x")).await.0,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn unindexed_casts_are_segmented_on_demand() {
    let kit = Kit::new(0);
    let c = long_cast(&kit, 1);
    // One small poll: the session exists, almost nothing of it is indexed.
    kit.index(&c.path, Some(400));
    let u = uuid(1);
    let (index, segs) = segmented(&kit, &u).await;
    assert!(segs.len() >= 3, "{}", segs.len());
    validate_segmented(&index, &segs);
    assert_continuous(&segs);
    assert_eq!(seg_output(&segs), file_output(&c.path));
    let markers: Vec<&str> = index["markers"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m[1].as_str().unwrap())
        .collect();
    assert_eq!(markers.len(), 13, "chapters from the cast's markers: {markers:?}");
}

#[tokio::test]
async fn forgotten_spans_never_reach_segments() {
    let kit = Kit::new(0);
    let c = long_cast(&kit, 1);
    kit.index(&c.path, None);
    let u = uuid(1);
    let _ = segmented(&kit, &u).await;
    let (status, _) = kit.post("/forget", &json!({"session_uuid": u, "cmd_seq": 4})).await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = kit.post("/forget", &json!({"session_uuid": u, "cmd_seq": 13})).await;
    assert_eq!(status, StatusCode::OK);
    kit.purge();
    let (index, segs) = segmented(&kit, &u).await;
    validate_segmented(&index, &segs);
    assert_continuous(&segs);
    let all = serde_json::to_string(&segs).unwrap();
    assert!(!all.contains("secret-value"), "masked in events and snapshots");
    assert!(!all.contains("line 3."), "command 4's output is masked");
    assert!(all.contains("[redacted by alice]"));
    let markers = serde_json::to_string(&index["markers"]).unwrap();
    assert!(!markers.contains("step3") && !markers.contains("secret"));
}

#[tokio::test]
async fn a_forget_from_the_cli_drops_cached_plans() {
    let kit = Kit::new(0);
    let c = long_cast(&kit, 1);
    kit.index(&c.path, None);
    let u = uuid(1);
    let (_, before) = segmented(&kit, &u).await;
    assert!(serde_json::to_string(&before).unwrap().contains("secret-value"));
    // `cinelog forget` writes meta.db itself; this server's plan cache never hears of it.
    let mut meta = cinelog_index::db::open_meta(&kit.ctx().db).unwrap();
    meta.forget(
        &cinelog_index::model::ForgetTarget::Command {
            uuid: u.clone(),
            seq: 13,
        },
        "cli",
    )
    .unwrap();
    // Purged: the span now comes from the cast's own markers (no range row).
    drop(meta);
    kit.purge();
    let index = kit.json(&format!("/sessions/{u}/play/index.json")).await;
    let n = index["segments"].as_array().unwrap().len();
    let mut segs = Vec::new();
    for i in 0..n {
        segs.push(kit.json(&format!("/sessions/{u}/play/seg/{i}")).await);
    }
    validate_segmented(&index, &segs);
    let all = serde_json::to_string(&segs).unwrap();
    assert!(!all.contains("secret-value"));
    assert!(all.contains("[redacted by cli]"));
    assert!(!all.contains("echo secret"), "the echoed command line is masked too");
}

/// Session 2 in two rotation parts: part 2 starts 200 s in, at another size; a command runs
/// across the switch; idle gaps over 60 s in both parts. Returns the part paths and every
/// output event's data in order.
fn rotated(kit: &Kit) -> (Vec<std::path::PathBuf>, String) {
    let dir = kit.dir.path();
    let mut p1 = Cast::part(dir, 2, 1, 0.0, 40, 8, &Meta::default());
    let mut t = 0.5;
    for i in 0..4u64 {
        let line = format!("\x1b[3{i}mpart one line {i}\x1b[0m");
        t = p1.command(t, i + 1, &format!("one{i}"), &[&line], 0) + 0.2;
        if i == 1 {
            t += 70.0;
        }
    }
    p1.ev(t, EventCode::Resize, "45x9");
    p1.o(t + 0.1, "$ tail -f log\r\n");
    p1.m(t + 0.11, &crate::api::testkit::cmd(5, 100, 5, "tail -f log"));
    p1.o(t + 0.12, "tail line one\r\n");
    p1.rotate(t + 0.2, 2, 1);
    let mut p2 = Cast::part(dir, 2, 2, 200.0, 50, 10, &Meta::default());
    p2.o(0.3, "tail line two\r\n");
    p2.m(0.4, &crate::api::testkit::done(5, 100, 5, 0));
    let mut t = 0.6;
    for i in 5..9u64 {
        let line = format!("part two line {i}");
        t = p2.command(t, i + 1, &format!("two{i}"), &[&line], 0) + 0.2;
        if i == 6 {
            t += 80.0;
        }
    }
    p2.exit(t + 0.5);
    let out = file_output(&p1.path) + &file_output(&p2.path);
    (vec![p1.path.clone(), p2.path.clone()], out)
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Zst {
    None,
    /// Part 1 compressed before indexing (the index has its frame table).
    BeforeIndexing,
    /// Part 1 compressed after indexing (found under its other name; no frame table).
    AfterIndexing,
}

async fn check_rotated(threshold_mb: u64, zst: Zst) {
    let kit = Kit::new(threshold_mb);
    let (paths, expected) = rotated(&kit);
    let mut first = paths[0].clone();
    if zst == Zst::BeforeIndexing {
        first = Cast::compress(&first);
    }
    kit.index(&first, None);
    if zst == Zst::AfterIndexing {
        Cast::compress(&first);
    }
    let u = uuid(2);
    let detail = kit.json(&format!("/sessions/{u}")).await;
    let parts = detail["parts"].as_array().unwrap();
    assert_eq!(parts.len(), 2, "{zst:?}");
    assert!((parts[1]["t_offset"].as_f64().unwrap() - 200.0).abs() < 1e-3);
    if threshold_mb == 0 {
        let (index, segs) = segmented(&kit, &u).await;
        assert!(segs.len() >= 3, "{zst:?}: {}", segs.len());
        validate_segmented(&index, &segs);
        assert_continuous(&segs);
        assert_eq!(seg_output(&segs), expected, "{zst:?}");
        assert_eq!(index["markers"].as_array().unwrap().len(), 9);
        assert!(index["duration"].as_f64().unwrap() > 280.0);
        let last = &segs[segs.len() - 1]["snapshot"];
        assert_eq!((last["cols"].as_u64(), last["rows"].as_u64()), (Some(50), Some(10)));
    } else {
        let (status, _, body) = kit.get(&format!("/sessions/{u}/cast")).await;
        assert_eq!(status, StatusCode::OK);
        let text = String::from_utf8(body).unwrap();
        let mut lines = text.lines();
        let h: Value = serde_json::from_str(lines.next().unwrap()).unwrap();
        assert_eq!(
            (h["term"]["cols"].as_u64(), h["term"]["rows"].as_u64()),
            (Some(40), Some(8))
        );
        let events: Vec<Value> = lines.map(|l| serde_json::from_str(l).unwrap()).collect();
        let output: String = events
            .iter()
            .filter(|e| e[1] == "o")
            .map(|e| e[2].as_str().unwrap())
            .collect();
        assert_eq!(output, expected, "{zst:?}");
        assert!(events.iter().any(|e| e[1] == "r" && e[2] == "50x10"), "part 2's size");
        let total: f64 = events.iter().map(|e| e[0].as_f64().unwrap()).sum();
        let duration = detail["session"]["duration"].as_f64().unwrap();
        assert!((total - duration).abs() < 1e-3, "{total} vs {duration}");
    }
    // Exports across the part switch.
    let (status, _, body) = kit
        .get(&format!("/sessions/{u}/export?format=txt&from=70&to=210"))
        .await;
    assert_eq!(status, StatusCode::OK);
    let txt = String::from_utf8(body).unwrap();
    assert!(txt.contains("part one line 2") && txt.contains("tail line two") && txt.contains("part two line 5"));
    let (_, _, body) = kit.get(&format!("/sessions/{u}/export?format=txt&cmd=5")).await;
    assert_eq!(String::from_utf8(body).unwrap(), "tail line one\ntail line two\n");
    // Forgetting the command across the switch masks both parts.
    let (status, _) = kit.post("/forget", &json!({"session_uuid": u, "cmd_seq": 5})).await;
    assert_eq!(status, StatusCode::OK);
    kit.purge();
    let (_, _, body) = kit.get(&format!("/sessions/{u}/export?format=cast")).await;
    let cast = String::from_utf8(body).unwrap();
    assert!(!cast.contains("tail line") && cast.contains("[redacted by alice]") && cast.contains("part two line 8"));
}

#[tokio::test]
async fn rotated_parts_play_as_one_session() {
    for zst in [Zst::None, Zst::BeforeIndexing, Zst::AfterIndexing] {
        check_rotated(0, zst).await;
        check_rotated(8, zst).await;
    }
}

/// An old-plugin asciinema v2 cast with its commands.log, idle gaps over 60 s and a resize.
fn legacy_cast(kit: &Kit) -> std::path::PathBuf {
    use crate::api::testkit::legacy_gen::{Cmd, Gen};
    let mut g = Gen::new(7).with_size(60, 12);
    g.draw_prompt();
    for i in 0..8 {
        let line = format!("legacy output {i}");
        g.run(&Cmd::new(&format!("echo step{i}")).out(&[&line]));
        if i % 3 == 1 {
            g.idle(75.0);
        }
        if i == 4 {
            g.resize(70, 14);
        }
    }
    g.run(&Cmd::new("echo legacy-secret").out(&["legacy-secret"]));
    let legacy = g.finish(0, true);
    let dir = kit.dir.path().join("logs/old");
    std::fs::create_dir_all(&dir).unwrap();
    legacy.write(&dir, "legacy.cast").unwrap()
}

async fn legacy_uuid(kit: &Kit) -> String {
    let page = kit.json("/sessions?legacy=true").await;
    page["sessions"][0]["uuid"].as_str().unwrap().to_owned()
}

#[tokio::test]
async fn legacy_v2_casts_play_and_export() {
    for threshold in [0, 8] {
        let kit = Kit::new(threshold);
        let path = legacy_cast(&kit);
        let expected = file_output(&path);
        kit.index_legacy(&path);
        let u = legacy_uuid(&kit).await;
        let commands = kit.json(&format!("/sessions/{u}/commands")).await;
        let n = commands["commands"].as_array().unwrap().len();
        assert_eq!(n, 9);
        let last_seq = commands["commands"][n - 1]["seq"].as_u64().unwrap();
        if threshold == 0 {
            let (index, segs) = segmented(&kit, &u).await;
            assert!(segs.len() >= 3, "{}", segs.len());
            validate_segmented(&index, &segs);
            assert_continuous(&segs);
            assert_eq!(seg_output(&segs), expected);
            let markers: Vec<&str> = index["markers"]
                .as_array()
                .unwrap()
                .iter()
                .map(|m| m[1].as_str().unwrap())
                .collect();
            assert_eq!(markers.len(), 9, "chapters from the index: {markers:?}");
            assert_eq!(markers[0], "$ echo step0");
        } else {
            let (status, _, body) = kit.get(&format!("/sessions/{u}/cast")).await;
            assert_eq!(status, StatusCode::OK);
            let text = String::from_utf8(body).unwrap();
            let mut lines = text.lines();
            let h: Value = serde_json::from_str(lines.next().unwrap()).unwrap();
            assert_eq!(h["version"], 3, "served as v3");
            let events: Vec<Value> = lines.map(|l| serde_json::from_str(l).unwrap()).collect();
            assert!(events.iter().all(|e| e[0].as_f64().unwrap() >= 0.0), "intervals");
            let output: String = events
                .iter()
                .filter(|e| e[1] == "o")
                .map(|e| e[2].as_str().unwrap())
                .collect();
            assert_eq!(output, expected);
            assert_eq!(events.iter().filter(|e| e[1] == "m").count(), 9);
        }
        let (status, _, body) = kit.get(&format!("/sessions/{u}/export?format=txt")).await;
        assert_eq!(status, StatusCode::OK);
        let txt = String::from_utf8(body).unwrap();
        assert!(txt.contains("legacy output 0") && txt.contains("legacy output 7"));
        let (_, _, body) = kit.get(&format!("/sessions/{u}/export?format=cast&from=80")).await;
        let cast = String::from_utf8(body).unwrap();
        let h: Value = serde_json::from_str(cast.lines().next().unwrap()).unwrap();
        assert_eq!(h["version"], 3);
        assert!(cast.contains("legacy output 7") && !cast.contains("legacy output 0\\r"));
        // A forget through the API stays masked after the purge (legacy casts have no
        // markers to fall back on).
        let (status, _) = kit
            .post("/forget", &json!({"session_uuid": u, "cmd_seq": last_seq}))
            .await;
        assert_eq!(status, StatusCode::OK);
        kit.purge();
        let (_, _, body) = kit.get(&format!("/sessions/{u}/export?format=txt")).await;
        let txt = String::from_utf8(body).unwrap();
        assert!(!txt.contains("legacy-secret") && txt.contains("[redacted by alice]"));
        if threshold == 0 {
            let (index, segs) = segmented(&kit, &u).await;
            validate_segmented(&index, &segs);
            assert!(!serde_json::to_string(&segs).unwrap().contains("legacy-secret"));
        } else {
            let (_, _, body) = kit.get(&format!("/sessions/{u}/cast")).await;
            assert!(!String::from_utf8(body).unwrap().contains("legacy-secret"));
        }
    }
}

/// The same events at two header sizes (session 3).
fn sized_cast(kit: &Kit, cols: u16, rows: u16) -> Cast {
    let mut c = Cast::new(kit.dir.path(), 3, cols, rows, &Meta::default());
    let t = c.command(0.5, 1, "echo before", &["before"], 0);
    c.ev(t + 0.1, EventCode::Resize, "65535x65535");
    let t = c.command(t + 0.2, 2, "echo huge", &["huge terminal"], 0);
    c.ev(t + 70.0, EventCode::Resize, "65535x0");
    c.ev(t + 70.1, EventCode::Resize, "1200x40");
    let t = c.command(t + 70.2, 3, "echo after", &["after"], 0);
    c.exit(t + 0.5);
    c
}

/// Regression: sizes from `r` events and headers reached avt unclamped (65535x65535 is
/// 4e9 cells and aborts the process).
#[tokio::test]
async fn huge_terminal_sizes_are_clamped() {
    use super::source::{MAX_COLS, MAX_ROWS};
    let max = (u64::from(MAX_COLS), u64::from(MAX_ROWS));
    for threshold in [0, 8] {
        let kit = Kit::new(threshold);
        let c = sized_cast(&kit, 40, 8);
        // The indexer's own avt has no clamp: index only the part before the resize.
        kit.index(&c.path, Some(300));
        let u = uuid(3);
        if threshold == 0 {
            let (index, segs) = segmented(&kit, &u).await;
            validate_segmented(&index, &segs);
            assert_continuous(&segs);
            for s in &segs {
                let snap = &s["snapshot"];
                assert!(snap["cols"].as_u64().unwrap() <= max.0 && snap["rows"].as_u64().unwrap() <= max.1);
            }
            let resizes: Vec<&str> = segs
                .iter()
                .flat_map(|s| s["events"].as_array().unwrap().iter())
                .filter(|e| e[1] == "r")
                .map(|e| e[2].as_str().unwrap())
                .collect();
            assert_eq!(resizes, ["1000x500", "1000x40"], "no zero sizes");
        } else {
            let (_, _, body) = kit.get(&format!("/sessions/{u}/cast")).await;
            let text = String::from_utf8(body).unwrap();
            assert!(text.contains(r#""r", "1000x500""#) && !text.contains("65535x65535"));
        }
        for format in ["txt", "html", "cast"] {
            let (status, _, body) = kit.get(&format!("/sessions/{u}/export?format={format}")).await;
            assert_eq!(status, StatusCode::OK, "{format}");
            assert!(String::from_utf8(body).unwrap().contains("huge terminal"), "{format}");
        }
        let (status, _, _) = kit.get(&format!("/sessions/{u}/export?format=cast&from=1")).await;
        assert_eq!(status, StatusCode::OK);

        // A header that large (the file rewritten after indexing).
        std::fs::remove_file(&c.path).unwrap();
        drop(sized_cast(&kit, 65535, 65535));
        let mut s = super::session::Loaded::load(kit.ctx(), &kit.state.play.markers, &u).unwrap();
        s.segments.clear();
        let plan = super::plan::Plan::build(&s, &super::plan::Progress::default()).unwrap();
        assert_eq!((plan.cols, plan.rows), (MAX_COLS, MAX_ROWS));
        if threshold == 8 {
            let (_, _, body) = kit.get(&format!("/sessions/{u}/cast")).await;
            let text = String::from_utf8(body).unwrap();
            assert!(
                text.starts_with(r#"{"version":3,"term":{"cols":1000,"rows":500"#),
                "{text:.60}"
            );
            let (status, _, body) = kit.get(&format!("/sessions/{u}/export?format=txt")).await;
            assert_eq!(status, StatusCode::OK);
            assert!(String::from_utf8(body).unwrap().contains("huge terminal"));
        }
    }
}

/// Regression: once a command forgotten with the CLI was purged, every load scanned the
/// whole cast for its markers. The span is now recorded, and a growing cast is scanned on
/// from where the last scan stopped.
#[tokio::test]
async fn marker_spans_are_found_once() {
    use std::io::Write as _;
    let kit = Kit::new(0);
    let mut c = Cast::new(kit.dir.path(), 4, 40, 8, &Meta::default());
    let t = c.command(0.5, 1, "ls", &["visible-one"], 0);
    c.o(t + 0.3, "$ cat secret\r\n");
    c.m(t + 0.31, &crate::api::testkit::cmd(2, 100, 2, "cat secret"));
    c.o(t + 0.32, "secret-running\r\n");
    c.flush();
    kit.index(&c.path, None);
    let u = uuid(4);
    kit.cli_forget(cinelog_index::model::ForgetTarget::Command {
        uuid: u.clone(),
        seq: 2,
    });
    kit.purge();
    let cache = &kit.state.play.markers;
    let load = || super::session::Loaded::load(kit.ctx(), cache, &u).unwrap();
    let s = load();
    assert_eq!(s.mask.spans.len(), 1);
    assert_eq!(s.mask.spans[0].to_us, u64::MAX, "running: open-ended");
    let size1 = std::fs::metadata(&c.path).unwrap().len();
    assert_eq!(cache.position(&u), Some((0, size1)), "scanned to the end");
    assert!(
        crate::api::db::Forgets::load(kit.ctx()).unwrap().span(&u, 2).is_none(),
        "an open span is not recorded"
    );
    // The command ends and the cast grows: the scan goes on from where it stopped.
    c.o(t + 0.5, "secret-more\r\n");
    c.m(t + 0.6, &crate::api::testkit::done(2, 100, 2, 0));
    c.o(t + 0.7, "visible-two\r\n");
    c.flush();
    let s = load();
    let span = &s.mask.spans[0];
    assert!(span.to_us < u64::MAX && span.from_us <= 1_000_000);
    assert_eq!(cache.position(&u), Some((0, std::fs::metadata(&c.path).unwrap().len())));
    let recorded = crate::api::db::Forgets::load(kit.ctx()).unwrap().span(&u, 2);
    assert!(recorded.is_some_and(|r| r.to.is_some()), "the closed span is recorded");
    // From now on the recorded span is used: the cast is not read for it again, so
    // corrupting the markers on disk changes nothing.
    let text = std::fs::read_to_string(&c.path)
        .unwrap()
        .replace(r#""k":"done""#, r#""k":"xxxx""#);
    std::fs::File::create(&c.path)
        .unwrap()
        .write_all(text.as_bytes())
        .unwrap();
    let again = load();
    assert_eq!(again.mask, s.mask);
    let (index, segs) = segmented(&kit, &u).await;
    validate_segmented(&index, &segs);
    let all = serde_json::to_string(&segs).unwrap();
    assert!(!all.contains("secret-running") && !all.contains("secret-more"));
    assert!(all.contains("visible-one") && all.contains("visible-two"));
}

/// Regression: a panicking build left its slot `Building` and the build count taken.
#[tokio::test]
async fn a_panicking_build_releases_its_slot() {
    let kit = Kit::new(0);
    let c = long_cast(&kit, 1);
    kit.index(&c.path, None);
    let u = uuid(1);
    let play = &kit.state.play;
    let (generation, _) = super::PlayState::claim(&mut play.lock(), &u, "k").unwrap();
    assert_eq!(play.lock().building, 1);
    let r = play.run_build_with(&u, generation, "k".into(), || panic!("avt exploded"));
    assert!(r.is_err());
    {
        let inner = play.lock();
        assert_eq!(inner.building, 0);
        let slot = &inner.slots[&u];
        assert!(slot.build.is_none() && slot.failed.is_some());
    }
    // The next request builds normally.
    let index = kit.json(&format!("/sessions/{u}/play/index.json")).await;
    assert!(!index["segments"].as_array().unwrap().is_empty());
}

/// Regression: synchronous builds (index, segment, export) bypassed the build limit.
#[tokio::test]
async fn synchronous_builds_wait_for_a_free_slot() {
    let kit = Kit::new(0);
    let c = long_cast(&kit, 1);
    kit.index(&c.path, None);
    let u = uuid(1);
    let play = &kit.state.play;
    let held: Vec<(String, u64)> = (0..super::MAX_BUILDS)
        .map(|i| {
            let other = format!("other-{i}");
            let (g, _) = super::PlayState::claim(&mut play.lock(), &other, "k").unwrap();
            (other, g)
        })
        .collect();
    let st = kit.state.clone();
    let u2 = u.clone();
    let waiter = std::thread::spawn(move || st.play.plan(&st.ctx, &u2).map(|p| p.segments.len()));
    std::thread::sleep(Duration::from_millis(200));
    assert!(!waiter.is_finished(), "waits while the limit is reached");
    assert!(play.lock().slots.get(&u).is_none_or(|s| s.build.is_none()));
    let (other, g) = &held[0];
    play.finish(other, *g, "k".into(), Err("done".into()));
    let n = waiter.join().unwrap().unwrap();
    assert!(n >= 3);
    for (other, g) in &held[1..] {
        play.finish(other, *g, "k".into(), Err("done".into()));
    }
    assert_eq!(play.lock().building, 0);
}

#[test]
fn progress_is_clamped() {
    let p = super::plan::Progress::default();
    p.set(2.0);
    assert_eq!(p.get(), 1.0);
    p.set(0.25);
    assert_eq!(p.get(), 0.25);
}

/// First open, cached open and seeks on a real index (manual benchmark, plan §13 M3
/// acceptance 2): `CINELOG_BENCH_DB=<dir with index.db> CINELOG_BENCH_UUID=<uuid> cargo test
/// --release -p cinelog-web --lib real_cast_bench -- --ignored --nocapture`. Read-only;
/// prints timings and counts only.
#[tokio::test]
#[ignore = "needs a real index (CINELOG_BENCH_DB, CINELOG_BENCH_UUID)"]
async fn real_cast_bench() {
    use std::time::Instant;
    let (Some(db), Some(u)) = (
        std::env::var_os("CINELOG_BENCH_DB"),
        std::env::var("CINELOG_BENCH_UUID").ok(),
    ) else {
        return;
    };
    let kit = Kit::new(8);
    let mut ctx = kit.ctx().clone();
    ctx.db = cinelog_index::model::DbPaths::in_dir(std::path::PathBuf::from(db));
    let loaded = super::session::Loaded::load(&ctx, &kit.state.play.markers, &u).unwrap();
    println!(
        "cast bytes {} indexed segments {} labels {}",
        loaded.files.total_bytes(),
        loaded.segments.len(),
        loaded.labels.len()
    );
    for (name, drop_db_segments) in [("indexed segments", false), ("on demand", true)] {
        let mut s = super::session::Loaded::load(&ctx, &kit.state.play.markers, &u).unwrap();
        if drop_db_segments {
            s.segments.clear();
        }
        let t = Instant::now();
        let plan = super::plan::Plan::build(&s, &super::plan::Progress::default()).unwrap();
        println!(
            "{name}: plan in {:.2} s, {} segments, duration {:.0} s",
            t.elapsed().as_secs_f64(),
            plan.segments.len(),
            plan.duration()
        );
        let mut worst = 0.0f64;
        for n in [
            0,
            plan.segments.len() / 3,
            plan.segments.len() / 2,
            plan.segments.len() - 1,
        ] {
            let t = Instant::now();
            let seg = plan.segment(n).unwrap();
            let body = serde_json::to_vec(&seg).unwrap();
            worst = worst.max(t.elapsed().as_secs_f64());
            assert!(!body.is_empty());
        }
        println!("{name}: worst segment fetch {worst:.3} s");
        let index = serde_json::to_value(plan.index()).unwrap();
        let segs: Vec<Value> = (0..plan.segments.len())
            .map(|n| serde_json::to_value(plan.segment(n).unwrap()).unwrap())
            .collect();
        validate_segmented(&index, &segs);
        assert_continuous(&segs);
        println!(
            "{name}: all {} segments pass the player's checks and join up",
            segs.len()
        );
    }
}

/// The CLI's hook: a span recorded before the purge keeps a legacy command masked.
#[tokio::test]
async fn the_cli_can_record_a_forgotten_commands_span() {
    let kit = Kit::new(8);
    let path = legacy_cast(&kit);
    kit.index_legacy(&path);
    let u = legacy_uuid(&kit).await;
    let commands = kit.json(&format!("/sessions/{u}/commands")).await;
    let seq = commands["commands"].as_array().unwrap().last().unwrap()["seq"]
        .as_u64()
        .unwrap();
    let db = &kit.ctx().db;
    let mut meta = cinelog_index::db::open_meta(db).unwrap();
    assert!(!super::record_forgotten_command_span(db, meta.as_mut(), &u, 999).unwrap());
    assert!(!super::record_forgotten_command_span(db, meta.as_mut(), &uuid(9), seq).unwrap());
    assert!(super::record_forgotten_command_span(db, meta.as_mut(), &u, seq).unwrap());
    drop(meta);
    kit.cli_forget(cinelog_index::model::ForgetTarget::Command { uuid: u.clone(), seq });
    kit.purge();
    let (_, _, body) = kit.get(&format!("/sessions/{u}/cast")).await;
    let cast = String::from_utf8(body).unwrap();
    assert!(!cast.contains("legacy-secret") && cast.contains("[redacted by cli]"));
}
