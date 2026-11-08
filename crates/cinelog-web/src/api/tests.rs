//! Handler tests of the JSON API over a real index.

use axum::http::StatusCode;
use serde_json::{Value, json};

use super::testkit::{Cast, Kit, Meta, uuid};

/// Session 1 (pane %3, three commands), session 2 (no tmux, terminator) and session 3
/// nested in session 1 (sudo).
fn fixture(kit: &Kit) {
    let dir = kit.dir.path();
    let mut a = Cast::new(dir, 1, 80, 24, &Meta::default());
    let t = a.command(1.0, 1, "ls -la", &["total 0", "drwx <b>bold</b>"], 0);
    let t = a.command(t + 1.0, 2, "echo secret-token", &["secret-token"], 0);
    let t = a.command(t + 1.0, 3, "false", &[], 1);
    a.exit(t + 0.5);
    let mut b = Cast::new(
        dir,
        2,
        100,
        30,
        &Meta {
            pane: "",
            holder: Some("terminator"),
            user: "bob",
            ..Meta::default()
        },
    );
    let t = b.command(0.5, 1, "docker ps", &["CONTAINER ID"], 0);
    b.exit(t + 0.5);
    let parent = uuid(1);
    let mut c = Cast::new(
        dir,
        3,
        80,
        24,
        &Meta {
            pane: "",
            parent: Some(&parent),
            holder: Some("sudo"),
            ..Meta::default()
        },
    );
    let t = c.command(0.5, 1, "id", &["uid=0(root)"], 0);
    c.exit(t + 0.5);
    for p in [&a.path, &b.path, &c.path] {
        kit.index(p, None);
    }
}

#[tokio::test]
async fn without_an_index() {
    let kit = Kit::new(8);
    let (status, _, body) = kit.get("/sessions").await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    let err: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(err["error"], "unavailable");
    let s = kit.json("/status").await;
    assert_eq!(s["schema_version"], 0);
    assert_eq!(s["port"], 10443);
    assert!(s["last_error"].is_string());
}

#[tokio::test]
async fn sessions_tree_and_detail() {
    let kit = Kit::new(8);
    fixture(&kit);
    let page = kit.json("/sessions").await;
    let uuids: Vec<&str> = page["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["uuid"].as_str().unwrap())
        .collect();
    assert_eq!(uuids, [uuid(3), uuid(2), uuid(1)], "newest first");
    assert_eq!(page["next"], Value::Null);
    assert_eq!(page["sessions"][2]["commands"], 3);
    assert_eq!(page["sessions"][2]["tmux"]["pane"], "%3");

    let first = kit.json("/sessions?limit=2").await;
    assert_eq!(first["sessions"].as_array().unwrap().len(), 2);
    let next = first["next"].as_str().unwrap();
    let rest = kit.json(&format!("/sessions?limit=2&cursor={next}")).await;
    assert_eq!(rest["sessions"][0]["uuid"], uuid(1));
    assert_eq!(rest["next"], Value::Null);
    assert_eq!(kit.json("/sessions?user=bob").await["sessions"][0]["uuid"], uuid(2));
    assert_eq!(
        kit.json("/sessions?pane=%253").await["sessions"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(kit.json("/sessions?live=true").await["sessions"], json!([]));
    for bad in [
        "/sessions?limit=0",
        "/sessions?cursor=zz",
        "/sessions?from=x",
        "/sessions?live=maybe",
    ] {
        assert_eq!(kit.get(bad).await.0, StatusCode::BAD_REQUEST, "{bad}");
    }

    let tree = kit.json("/tree").await;
    let host = &tree["hosts"][0];
    assert_eq!(host["host"], "testhost");
    let pane = &host["tmux_servers"][0]["sessions"][0]["windows"][0]["panes"][0];
    assert_eq!(pane["pane"], "%3");
    assert_eq!(pane["sessions"][0]["uuid"], uuid(1));
    assert_eq!(
        pane["sessions"][0]["children"][0]["uuid"],
        uuid(3),
        "nested under its parent"
    );
    let groups: Vec<&Value> = host["shells"].as_array().unwrap().iter().collect();
    assert_eq!(groups.len(), 1, "the nested sudo session is not listed again");
    assert_eq!(groups[0]["holder_comm"], "terminator");

    let d = kit.json(&format!("/sessions/{}", uuid(1).to_uppercase())).await;
    assert_eq!(d["session"]["uuid"], uuid(1));
    assert_eq!(d["children"][0]["uuid"], uuid(3));
    assert_eq!(d["parent"], Value::Null);
    assert_eq!(d["play_mode"], "cast");
    assert_eq!(d["parts"][0]["part"], 1);
    assert_eq!(d["forgotten"], json!([]));
    let child = kit.json(&format!("/sessions/{}", uuid(3))).await;
    assert_eq!(child["parent"]["uuid"], uuid(1));

    let cmds = kit.json(&format!("/sessions/{}/commands", uuid(1))).await;
    let typed: Vec<&str> = cmds["commands"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["typed"].as_str().unwrap())
        .collect();
    assert_eq!(typed, ["ls -la", "echo secret-token", "false"]);
    assert_eq!(cmds["commands"][2]["exit"], 1);

    assert_eq!(
        kit.get(&format!("/sessions/{}", uuid(9))).await.0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(kit.get("/sessions/..%2Fetc").await.0, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn search_show_status_import() {
    let kit = Kit::new(8);
    fixture(&kit);
    let r = kit.json("/search?q=SECRET").await;
    let hits = r["hits"].as_array().unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0]["kind"], "command");
    assert_eq!(hits[0]["pane"], "%3");
    assert_eq!(
        hits[0]["snippet"],
        json!([{"text": "echo ", "hit": false}, {"text": "secret", "hit": true}, {"text": "-token", "hit": false}])
    );
    let out = kit.json("/search?q=bold&in=out").await;
    let hit = &out["hits"][0];
    assert_eq!(hit["kind"], "output");
    let text: String = hit["snippet"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["text"].as_str().unwrap())
        .collect();
    assert!(text.contains("<b>bold</b>"), "markup stays text: {text}");
    assert!(!text.contains('\u{E000}'));
    assert_eq!(kit.get("/search?in=sideways").await.0, StatusCode::BAD_REQUEST);

    let e = kit.json(&format!("/show?entry={}:1", uuid(1))).await;
    assert_eq!(e["command"]["typed"], "ls -la");
    assert!(e["output"].as_str().unwrap().contains("total 0"));
    assert_eq!(kit.get("/show?entry=nope").await.0, StatusCode::BAD_REQUEST);
    assert_eq!(
        kit.get(&format!("/show?entry={}:99", uuid(1))).await.0,
        StatusCode::NOT_FOUND
    );

    let s = kit.json("/status").await;
    assert_eq!(s["schema_version"], 1);
    assert_eq!(s["commands"], 5);
    assert_eq!(s["sessions"]["ended"], 3);
    assert_eq!(s["fingerprint"], "AB:CD");
    let imp = kit.json("/import").await;
    assert_eq!(imp["files"], json!([]));
}

#[tokio::test]
async fn forget_forms_and_effects() {
    let kit = Kit::new(8);
    fixture(&kit);
    let u = uuid(1);
    for (body, status) in [
        (json!({"session_uuid": "x"}), StatusCode::BAD_REQUEST),
        (
            json!({"session_uuid": u, "cmd_seq": 1, "t_from": 1.0, "t_to": 2.0}),
            StatusCode::BAD_REQUEST,
        ),
        (json!({"session_uuid": u, "t_from": 1.0}), StatusCode::BAD_REQUEST),
        (json!({"nope": 1}), StatusCode::BAD_REQUEST),
        (json!({"session_uuid": uuid(9)}), StatusCode::NOT_FOUND),
        (json!({"session_uuid": u, "cmd_seq": 42}), StatusCode::CONFLICT),
    ] {
        assert_eq!(kit.post("/forget", &body).await.0, status, "{body}");
    }
    let (status, f) = kit.post("/forget", &json!({"session_uuid": u, "cmd_seq": 2})).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(f["target"], json!({"kind": "command", "uuid": u, "seq": 2}));
    assert_eq!(f["by"], "alice");
    let (_, again) = kit.post("/forget", &json!({"session_uuid": u, "cmd_seq": 2})).await;
    assert_eq!(again, f, "repeating returns the existing row");

    // Excluded at once, before the daemon purges anything.
    let cmds = kit.json(&format!("/sessions/{u}/commands")).await;
    assert_eq!(cmds["commands"].as_array().unwrap().len(), 2);
    assert_eq!(kit.json("/search?q=secret").await["hits"], json!([]));
    let d = kit.json(&format!("/sessions/{u}")).await;
    let kinds: Vec<&str> = d["forgotten"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["target"]["kind"].as_str().unwrap())
        .collect();
    assert_eq!(kinds, ["command"], "no range row next to the command");
    kit.purge();
    assert_eq!(
        kit.json(&format!("/sessions/{u}/commands")).await["commands"]
            .as_array()
            .unwrap()
            .len(),
        2
    );

    let (status, _) = kit.post("/forget", &json!({"session_uuid": uuid(2)})).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        kit.get(&format!("/sessions/{}", uuid(2))).await.0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(kit.json("/sessions").await["sessions"].as_array().unwrap().len(), 2);
    let (status, _) = kit
        .post("/forget", &json!({"session_uuid": uuid(2), "cmd_seq": 1}))
        .await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "nothing is left to forget in a forgotten session"
    );
    kit.purge();
    assert_eq!(
        kit.get(&format!("/sessions/{}", uuid(2))).await.0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        kit.json("/sessions").await["sessions"].as_array().unwrap().len(),
        2,
        "tombstones stay hidden"
    );
}

/// Session 5: `ls`, an hour of idle background output (unattributed), a leading-space
/// command (`skip` marker, no command row) and `echo secret-token`, with tmux window names
/// before the first and inside the last command.
fn gap_fixture(kit: &Kit) -> String {
    let mut c = Cast::new(kit.dir.path(), 5, 80, 24, &Meta::default());
    c.m(
        0.5,
        r#"{"cl":1,"k":"ctx","session_name":"w","window_index":"1","window_name":"zsh","pane_index":"0","pane_id":"%3"}"#,
    );
    let t = c.command(1.0, 1, "ls", &["file-a"], 0);
    c.o(t + 10.0, "background-job-gap-output\r\n");
    c.m(t + 3600.0, r#"{"cl":1,"k":"skip","sh":100,"t":0}"#);
    c.o(t + 3600.1, "leading-space-output\r\n");
    let t = t + 3700.0;
    c.o(t, "$ echo secret-token\r\n");
    c.m(t + 0.01, &super::testkit::cmd(2, 100, 2, "echo secret-token"));
    c.m(
        t + 0.02,
        r#"{"cl":1,"k":"ctx","session_name":"w","window_index":"1","window_name":"echo secret-token","pane_index":"0","pane_id":"%3"}"#,
    );
    c.o(t + 0.03, "secret-token\r\n");
    c.m(t + 0.2, &super::testkit::done(2, 100, 2, 0));
    c.exit(t + 1.0);
    kit.index(&c.path, None);
    uuid(5)
}

fn window_names(d: &Value) -> Vec<String> {
    d["markers"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| m["kind"] == "ctx")
        .map(|m| m["data"]["window_name"].as_str().unwrap().to_owned())
        .collect()
}

/// Regression: a command forget used to record a range row as well, and the daemon's range
/// purge then blanked unrelated unattributed output and leading-space output in it.
#[tokio::test]
async fn a_command_forget_keeps_unrelated_output_in_the_index() {
    let kit = Kit::new(8);
    let u = gap_fixture(&kit);
    let (status, _) = kit.post("/forget", &json!({"session_uuid": u, "cmd_seq": 2})).await;
    assert_eq!(status, StatusCode::OK);
    let forgets = super::db::Forgets::load(kit.ctx()).unwrap();
    assert_eq!(forgets.rows(&u).len(), 1, "only the command row");
    let span = forgets.span(&u, 2).expect("the playback span is recorded");
    assert!(span.from < 10.0 && span.to.is_some_and(|t| t > 3700.0));
    kit.purge();
    for q in ["background-job-gap-output", "leading-space-output", "file-a"] {
        let r = kit.json(&format!("/search?q={q}&in=out")).await;
        assert_eq!(r["hits"].as_array().unwrap().len(), 1, "{q} survives");
    }
    // The command's own output is gone (the echoed command line before its `cmd` marker is
    // part of the unattributed chunk, which the index keeps).
    let hits = kit.json("/search?q=secret&in=out").await;
    assert!(hits["hits"].as_array().unwrap().iter().all(|h| h["cmd_seq"].is_null()));
    // Playback still hides the command after the purge (from its recorded span).
    let (_, _, cast) = kit.get(&format!("/sessions/{u}/cast")).await;
    let cast = String::from_utf8(cast).unwrap();
    assert!(!cast.contains("secret-token") && cast.contains("[redacted by alice]"));
    // The detail reports that span although the command has left index.db (the SPA draws it).
    let d = kit.json(&format!("/sessions/{u}")).await;
    let masked = d["masked"].as_array().unwrap();
    assert_eq!(masked.len(), 1, "{masked:?}");
    assert!((masked[0]["t_from"].as_f64().unwrap() - span.from).abs() < 1e-6);
    assert_eq!(masked[0]["by"], "alice");
}

/// Regression: ctx markers inside a command forgotten with the CLI (no range row) were kept.
#[tokio::test]
async fn ctx_markers_follow_the_playback_mask() {
    let kit = Kit::new(8);
    let u = gap_fixture(&kit);
    let d = kit.json(&format!("/sessions/{u}")).await;
    assert_eq!(window_names(&d), ["zsh", "echo secret-token"]);
    kit.cli_forget(cinelog_index::model::ForgetTarget::Command {
        uuid: u.clone(),
        seq: 2,
    });
    let d = kit.json(&format!("/sessions/{u}")).await;
    assert_eq!(window_names(&d), ["zsh"], "before the purge");
    kit.purge();
    let d = kit.json(&format!("/sessions/{u}")).await;
    assert_eq!(window_names(&d), ["zsh"], "after the purge");
}
