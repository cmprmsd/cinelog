//! Export tests over a real index: cast slices that play on their own, rendered text and
//! escaped HTML, command and time ranges, masking, small and segmented casts.

use axum::http::{StatusCode, header};
use cinelog_cast::{CastReader, EventCode};
use serde_json::{Value, json};

use crate::api::testkit::{Cast, Kit, Meta, uuid};

fn fixture(kit: &Kit) -> Cast {
    let mut c = Cast::new(kit.dir.path(), 1, 30, 6, &Meta::default());
    let t = c.command(1.0, 1, "ls", &["alpha", "beta"], 0);
    let t = c.command(
        t + 0.5,
        2,
        "cat page.html",
        &["<script>alert(1)</script>", "\x1b[1;32mgreen\x1b[0m & co"],
        0,
    );
    // An idle gap: segmented casts cut a segment here.
    let t = c.command(t + 90.0, 3, "echo hidden", &["hidden-output"], 0);
    let t = c.command(t + 0.5, 4, "tail", &["omega"], 0);
    c.exit(t + 0.5);
    c
}

async fn export(kit: &Kit, query: &str) -> (StatusCode, String, String) {
    let (status, headers, body) = kit.get(&format!("/sessions/{}/export?{query}", uuid(1))).await;
    let disposition = headers
        .get(header::CONTENT_DISPOSITION)
        .map(|v| v.to_str().unwrap().to_owned())
        .unwrap_or_default();
    (status, disposition, String::from_utf8(body).unwrap())
}

/// Parses an exported cast with the real reader: header plus events.
fn parse_cast(kit: &Kit, text: &str) -> (cinelog_cast::Header, Vec<cinelog_cast::Event>) {
    let path = kit.dir.path().join("slice.cast");
    std::fs::write(&path, text).unwrap();
    let mut r = cinelog_cast::open(&path).unwrap();
    let mut events = Vec::new();
    while let Some(e) = r.next_event() {
        events.push(e.unwrap());
    }
    assert_eq!(r.stats().bad_lines, 0);
    (r.header().clone(), events)
}

fn screen_after(header: &cinelog_cast::Header, events: &[cinelog_cast::Event]) -> Vec<String> {
    let mut vt = avt::Terminal::new(usize::from(header.term.cols), usize::from(header.term.rows)).unwrap();
    for e in events {
        if e.code == EventCode::Output {
            vt.feed(e.data.as_str());
        }
    }
    vt.screen().text()
}

async fn exports_work(kit: &Kit) {
    let c = fixture(kit);
    kit.index(&c.path, None);

    let (status, disposition, text) = export(kit, "format=txt&cmd=1").await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        disposition.starts_with("attachment; filename=\"cinelog-0193a1b2-"),
        "{disposition}"
    );
    assert!(disposition.ends_with(".txt\""));
    assert_eq!(text, "alpha\nbeta\n");

    let (_, _, all) = export(kit, "format=txt").await;
    for needle in [
        "$ ls",
        "alpha",
        "<script>alert(1)</script>",
        "green & co",
        "hidden-output",
        "omega",
    ] {
        assert!(all.contains(needle), "{needle} in {all}");
    }

    let (status, disposition, html) = export(kit, "format=html&cmd=2").await;
    assert_eq!(status, StatusCode::OK);
    assert!(disposition.ends_with(".html\""));
    assert!(html.starts_with("<!DOCTYPE html>"));
    assert!(html.contains("content=\"default-src 'none'; style-src 'unsafe-inline'\""));
    assert!(html.contains("&lt;script&gt;alert(1)&lt;/script&gt;"));
    assert!(!html.to_lowercase().contains("<script"));
    assert!(
        html.contains("<span style=\"color:#00cd00;font-weight:bold;\">green</span> &amp; co"),
        "{html}"
    );

    // A slice from inside the session: the screen at `from` arrives as one t=0 event.
    let d = kit.json(&format!("/sessions/{}/commands", uuid(1))).await;
    let c3 = &d["commands"][2];
    let from = c3["t_start"].as_f64().unwrap();
    let (status, _, cast) = export(kit, &format!("format=cast&from={from}")).await;
    assert_eq!(status, StatusCode::OK);
    let (h, events) = parse_cast(kit, &cast);
    assert_eq!((h.version, h.term.cols, h.term.rows), (3, 30, 6));
    assert_eq!(events[0].t_us, 0);
    assert_eq!(events[0].code, EventCode::Output);
    let full_screen = screen_after(&h, &events[..1]);
    assert!(
        full_screen.iter().any(|l| l.contains("$ echo hidden")),
        "{full_screen:?}"
    );
    let labels: Vec<&str> = events
        .iter()
        .filter(|e| e.code == EventCode::Marker)
        .map(|e| e.data.as_str())
        .collect();
    assert_eq!(labels, ["$ echo hidden", "$ tail"]);
    assert_eq!(events.last().unwrap().code, EventCode::Exit);
    assert!(screen_after(&h, &events).iter().any(|l| l.contains("omega")));

    let (_, _, cmd_cast) = export(kit, "format=cast&cmd=1").await;
    let (_, events) = parse_cast(kit, &cmd_cast);
    let out: String = events
        .iter()
        .filter(|e| e.code == EventCode::Output)
        .skip(1)
        .map(|e| e.data.as_str())
        .collect();
    assert_eq!(out, "alpha\r\nbeta\r\n");

    for (q, status) in [
        ("format=pdf", StatusCode::BAD_REQUEST),
        ("format=txt&from=-1", StatusCode::BAD_REQUEST),
        ("format=txt&from=5&to=1", StatusCode::BAD_REQUEST),
        ("format=txt&cmd=1&from=0", StatusCode::BAD_REQUEST),
        ("format=txt&cmd=99", StatusCode::NOT_FOUND),
        ("cmd=1", StatusCode::BAD_REQUEST),
    ] {
        assert_eq!(export(kit, q).await.0, status, "{q}");
    }

    // Forgotten: masked in every format, and the command itself is gone.
    let (status, _) = kit
        .post("/forget", &json!({"session_uuid": uuid(1), "cmd_seq": 3}))
        .await;
    assert_eq!(status, StatusCode::OK);
    kit.purge();
    for format in ["txt", "html", "cast"] {
        let (_, _, body) = export(kit, &format!("format={format}")).await;
        assert!(!body.contains("hidden"), "{format}: {body}");
        assert!(body.contains("[redacted by alice]"), "{format}");
        assert!(body.contains("omega"), "{format}");
    }
    assert_eq!(export(kit, "format=txt&cmd=3").await.0, StatusCode::NOT_FOUND);
    let d: Value = kit.json(&format!("/sessions/{}", uuid(1))).await;
    assert_eq!(d["forgotten"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn small_cast_exports() {
    exports_work(&Kit::new(8)).await;
}

#[tokio::test]
async fn segmented_cast_exports() {
    exports_work(&Kit::new(0)).await;
}
