//! Live stream and UI bus tests against real casts (written and grown with the cast writer),
//! real registry files and a real index.db.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use cinelog_cast::EventCode;
use cinelog_core::registry::{ProcessProbe, REGISTRY_FORMAT, RecState, Registry, RegistryEntry, SystemProbe, TtyKey};
use cinelog_core::{Nonce, Scope};
use cinelog_index::model::{ForgetTarget as ImTarget, PollBudget, RenderOptions, SessionState as ImState};
use http_body_util::BodyExt;
use serde_json::Value;
use tokio::sync::broadcast;
use tower::ServiceExt;

use super::follow::{self, POLL};
use super::watch::{Timing, Watcher};
use super::*;
use crate::api::testkit::{Cast, Kit, Meta, cmd, uuid};
use crate::api::types::{ForgetTarget, Forgotten, LegacyProgress, RecStatus, SessionState, Tag};
use crate::bus::BUS_CAPACITY;
use crate::state::Principal;

// ------------------------------------------------------------------ fixtures

/// Writes a registry entry for session `n` on `/dev/pts/<pts>`: alive (this test process's
/// pid, starttime and boot id) or dead (a starttime that does not match).
fn register(kit: &Kit, pts: u32, n: u32, cast: &Path, state: RecState, reason: Option<&str>, alive: bool) {
    let pid = std::process::id();
    let starttime = SystemProbe.starttime(pid).unwrap() + u64::from(!alive);
    let entry = RegistryEntry {
        format: REGISTRY_FORMAT,
        pid,
        starttime,
        boot_id: SystemProbe.boot_id().unwrap(),
        uuid: uuid(n),
        nonce: Nonce::from_bytes([7; 16]),
        scope: Scope::Personal,
        cast: cast.to_owned(),
        pts: format!("/dev/pts/{pts}"),
        host_tty: "/dev/pts/1".into(),
        shell_pid: pid,
        parent_uuid: None,
        state,
        state_reason: reason.map(str::to_owned),
        part: 1,
        version: crate::VERSION.into(),
        opts: String::new(),
    };
    let reg = Registry::new(&kit.ctx().paths.runtime_dir);
    reg.ensure_dir().unwrap();
    reg.write(&TtyKey::Pts(pts), &entry).unwrap();
}

fn unregister(kit: &Kit, pts: u32) {
    let reg = Registry::new(&kit.ctx().paths.runtime_dir);
    std::fs::remove_file(reg.path_for(&TtyKey::Pts(pts))).unwrap();
}

/// Indexes `path` as far as it is written and closes it with `state`.
fn index_as(kit: &Kit, path: &Path, state: ImState) {
    let factory = cinelog_index::render::cast_pipelines(RenderOptions::default()).unwrap();
    let mut w = kit.writer();
    let mut p = factory.open(path, None).unwrap();
    let budget = PollBudget {
        max_bytes: u64::MAX,
        live: false,
    };
    while let Some(b) = p.poll(budget).unwrap() {
        w.apply(&b).unwrap();
    }
    w.apply(&p.finish(state).unwrap()).unwrap();
}

fn router(kit: &Kit) -> Router {
    Router::new()
        .merge(routes())
        .layer(axum::middleware::from_fn(
            |mut req: Request<Body>, next: axum::middleware::Next| async move {
                req.extensions_mut().insert(Principal("alice".into()));
                next.run(req).await
            },
        ))
        .with_state(kit.state.clone())
}

#[derive(Debug)]
struct Frame {
    event: Option<String>,
    data: String,
}

/// Reads SSE frames from a response body (comments skipped).
struct Client {
    body: Body,
    buf: String,
}

impl Client {
    async fn open(kit: &Kit, uri: &str) -> Result<Self, (StatusCode, Value)> {
        let req = Request::get(uri).body(Body::empty()).unwrap();
        let resp = router(kit).oneshot(req).await.unwrap();
        let status = resp.status();
        if status != StatusCode::OK {
            let body = resp.into_body().collect().await.unwrap().to_bytes();
            return Err((status, serde_json::from_slice(&body).unwrap_or(Value::Null)));
        }
        assert_eq!(resp.headers()[header::CONTENT_TYPE], "text/event-stream");
        assert_eq!(resp.headers()[header::CACHE_CONTROL], "no-store");
        Ok(Self {
            body: resp.into_body(),
            buf: String::new(),
        })
    }

    /// The next frame; `None` when the stream ended.
    async fn next(&mut self) -> Option<Frame> {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(i) = self.buf.find("\n\n") {
                let raw: String = self.buf.drain(..i + 2).collect();
                let mut event = None;
                let mut data: Option<Vec<String>> = None;
                for line in raw.lines() {
                    if let Some(v) = line.strip_prefix("event: ") {
                        event = Some(v.to_owned());
                    } else if line == "data:" {
                        data.get_or_insert_with(Vec::new).push(String::new());
                    } else if let Some(v) = line.strip_prefix("data: ") {
                        data.get_or_insert_with(Vec::new).push(v.to_owned());
                    }
                }
                match data {
                    Some(d) => {
                        return Some(Frame {
                            event,
                            data: d.join("\n"),
                        });
                    }
                    None => continue,
                }
            }
            match tokio::time::timeout_at(deadline, self.body.frame()).await {
                Err(_) => panic!("no SSE frame within 10 s; buffered {:?}", self.buf),
                Ok(None) => return None,
                Ok(Some(f)) => {
                    if let Ok(d) = f.unwrap().into_data() {
                        self.buf.push_str(std::str::from_utf8(&d).unwrap());
                    }
                }
            }
        }
    }

    /// The next `message` event's JSON.
    async fn message(&mut self) -> Value {
        let f = self.next().await.expect("a frame");
        assert_eq!(f.event, None, "{f:?}");
        serde_json::from_str(&f.data).unwrap()
    }

    async fn expect_done(&mut self) {
        let f = self.next().await.expect("done");
        assert_eq!((f.event.as_deref(), f.data.as_str()), (Some("done"), ""), "{f:?}");
        assert!(self.next().await.is_none(), "the stream ends after done");
    }

    async fn named(&mut self) -> (String, Value) {
        let f = self.next().await.expect("a frame");
        (f.event.expect("named"), serde_json::from_str(&f.data).unwrap())
    }
}

fn screen(init: &Value) -> Vec<String> {
    let mut vt = avt::Terminal::new(
        init["cols"].as_u64().unwrap() as usize,
        init["rows"].as_u64().unwrap() as usize,
    )
    .unwrap();
    vt.feed(init["init"].as_str().unwrap());
    vt.screen().text()
}

fn shows(init: &Value, needle: &str) -> bool {
    screen(init).iter().any(|l| l.contains(needle))
}

fn live_uri(n: u32) -> String {
    format!("/sessions/{}/live", uuid(n))
}

fn drain(rx: &mut broadcast::Receiver<BusEvent>) -> Vec<BusEvent> {
    std::iter::from_fn(|| rx.try_recv().ok()).collect()
}

fn fast() -> Timing {
    Timing {
        forget_poll: Duration::ZERO,
        progress: Duration::ZERO,
    }
}

// ------------------------------------------------------------------ live stream

#[tokio::test]
async fn ended_session_late_join_gets_the_final_screen_then_done() {
    let kit = Kit::new(8);
    let mut c = Cast::new(kit.dir.path(), 1, 80, 24, &Meta::default());
    c.o(0.5, "hello world\r\n");
    c.command(1.0, 1, "ls", &["a.txt"], 0);
    c.exit(2.0);
    kit.index(&c.path, None);

    let mut s = Client::open(&kit, &live_uri(1)).await.unwrap();
    let init = s.message().await;
    assert_eq!((init["cols"].as_u64(), init["rows"].as_u64()), (Some(80), Some(24)));
    assert_eq!(init["time"].as_f64(), Some(2.0), "{init}");
    assert!(
        shows(&init, "hello world") && shows(&init, "a.txt"),
        "{:?}",
        screen(&init)
    );
    s.expect_done().await;
}

#[tokio::test]
async fn live_session_streams_appended_events_until_exit() {
    let kit = Kit::new(8);
    let mut c = Cast::new(kit.dir.path(), 2, 80, 24, &Meta::default());
    c.o(0.5, "before join\r\n");
    c.flush();
    // Not indexed yet: the registry and the cast describe it.
    register(&kit, 3, 2, &c.path, RecState::Live, None, true);

    let mut s = Client::open(&kit, &live_uri(2)).await.unwrap();
    let init = s.message().await;
    assert!(shows(&init, "before join"));
    assert_eq!(init["time"].as_f64(), Some(0.5));

    c.o(3.0, "after join\r\n");
    c.m(3.1, &cmd(1, 100, 1, "ls -l"));
    c.ev(3.2, EventCode::Resize, "100x30");
    c.ev(3.3, EventCode::Input, "q");
    c.flush();
    assert_eq!(s.message().await, serde_json::json!([3.0, "o", "after join\r\n"]));
    assert_eq!(s.message().await, serde_json::json!([3.1, "m", "$ ls -l"]));
    assert_eq!(s.message().await, serde_json::json!([3.2, "r", "100x30"]));
    assert_eq!(s.message().await, serde_json::json!([3.3, "i", "q"]));

    c.o(4.0, "bye\r\n");
    c.exit(4.5);
    assert_eq!(s.message().await, serde_json::json!([4.0, "o", "bye\r\n"]));
    s.expect_done().await;
}

#[tokio::test]
async fn crash_of_the_recorder_sends_offline_then_done() {
    for remove in [false, true] {
        let kit = Kit::new(8);
        let mut c = Cast::new(kit.dir.path(), 3, 80, 24, &Meta::default());
        c.o(0.5, "running\r\n");
        c.flush();
        register(&kit, 4, 3, &c.path, RecState::Live, None, true);
        kit.index(&c.path, None);

        let mut s = Client::open(&kit, &live_uri(3)).await.unwrap();
        assert!(shows(&s.message().await, "running"));
        let crashed = Instant::now();
        if remove {
            unregister(&kit, 4);
        } else {
            register(&kit, 4, 3, &c.path, RecState::Live, None, false);
        }
        assert_eq!(s.message().await, serde_json::json!({"state": "offline"}));
        s.expect_done().await;
        assert!(crashed.elapsed() < Duration::from_secs(2), "{:?}", crashed.elapsed());
    }
}

#[tokio::test]
async fn sessions_that_are_over_at_join() {
    let kit = Kit::new(8);
    // Crashed (no `x`, the index closed it as crashed): offline after the final screen.
    let mut c = Cast::new(kit.dir.path(), 4, 80, 24, &Meta::default());
    c.o(0.5, "last words\r\n");
    c.flush();
    index_as(&kit, &c.path, ImState::Crashed);
    let mut s = Client::open(&kit, &live_uri(4)).await.unwrap();
    assert!(shows(&s.message().await, "last words"));
    assert_eq!(s.message().await, serde_json::json!({"state": "offline"}));
    s.expect_done().await;

    // Degraded: no crash, just the end.
    let mut c = Cast::new(kit.dir.path(), 5, 80, 24, &Meta::default());
    c.o(0.5, "disk full\r\n");
    c.flush();
    index_as(&kit, &c.path, ImState::Degraded);
    let mut s = Client::open(&kit, &live_uri(5)).await.unwrap();
    assert!(shows(&s.message().await, "disk full"));
    s.expect_done().await;

    // Indexed as live, but no recorder: a crash nobody has noticed yet.
    let mut c = Cast::new(kit.dir.path(), 6, 80, 24, &Meta::default());
    c.o(0.5, "orphan\r\n");
    c.flush();
    kit.index(&c.path, None);
    let mut s = Client::open(&kit, &live_uri(6)).await.unwrap();
    assert!(shows(&s.message().await, "orphan"));
    assert_eq!(s.message().await, serde_json::json!({"state": "offline"}));
    s.expect_done().await;
}

#[tokio::test]
async fn unknown_forgotten_and_malformed_sessions() {
    let kit = Kit::new(8);
    let mut c = Cast::new(kit.dir.path(), 7, 80, 24, &Meta::default());
    c.o(0.5, "x\r\n");
    c.exit(1.0);
    kit.index(&c.path, None);
    let (status, body) = Client::open(&kit, &live_uri(99)).await.err().unwrap();
    assert_eq!(
        (status, body["error"].as_str()),
        (StatusCode::NOT_FOUND, Some("not_found"))
    );
    let (status, _) = Client::open(&kit, "/sessions/..%2F..%2Fetc/live").await.err().unwrap();
    assert_eq!(status, StatusCode::BAD_REQUEST);
    kit.cli_forget(ImTarget::Session { uuid: uuid(7) });
    let (status, _) = Client::open(&kit, &live_uri(7)).await.err().unwrap();
    assert_eq!(status, StatusCode::NOT_FOUND);

    // A recorder of a forgotten session is not watchable either.
    register(&kit, 5, 7, &c.path, RecState::Live, None, true);
    let (status, _) = Client::open(&kit, &live_uri(7)).await.err().unwrap();
    assert_eq!(status, StatusCode::NOT_FOUND);

    // A dead recorder of a session the index has never seen: unknown.
    let mut c = Cast::new(kit.dir.path(), 8, 80, 24, &Meta::default());
    c.o(0.5, "x\r\n");
    c.flush();
    register(&kit, 6, 8, &c.path, RecState::Live, None, false);
    let (status, _) = Client::open(&kit, &live_uri(8)).await.err().unwrap();
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn rotation_parts_are_followed_with_continuous_time() {
    let kit = Kit::new(8);
    let meta = Meta::default();
    let mut p1 = Cast::new(kit.dir.path(), 9, 80, 24, &meta);
    p1.o(0.5, "part one\r\n");
    p1.flush();
    register(&kit, 7, 9, &p1.path, RecState::Live, None, true);
    let mut s = Client::open(&kit, &live_uri(9)).await.unwrap();
    assert!(shows(&s.message().await, "part one"));

    // The recorder creates the next part, ends the current one with `rotate`, then writes on.
    let mut p2 = Cast::part(kit.dir.path(), 9, 2, 1.0, 100, 30, &meta);
    p1.rotate(1.0, 9, 1);
    std::thread::sleep(POLL * 3);
    p2.flush();
    register(&kit, 7, 9, &p2.path, RecState::Live, None, true);
    p2.o(0.5, "part two\r\n");
    p2.flush();
    // The new part's header size arrives as a resize at the boundary.
    assert_eq!(s.message().await, serde_json::json!([1.0, "r", "100x30"]));
    assert_eq!(s.message().await, serde_json::json!([1.5, "o", "part two\r\n"]));
    p2.exit(2.0); // part-local: session second 3.0
    s.expect_done().await;

    // A late joiner reads across both parts.
    let mut s = Client::open(&kit, &live_uri(9)).await.unwrap();
    let init = s.message().await;
    assert_eq!((init["cols"].as_u64(), init["time"].as_f64()), (Some(100), Some(3.0)));
    assert!(shows(&init, "part two"));
    s.expect_done().await;
}

/// A session with a command whose output must disappear once it is forgotten.
fn secret_session(kit: &Kit, n: u32) -> Cast {
    let mut c = Cast::new(kit.dir.path(), n, 80, 24, &Meta::default());
    c.o(0.2, "start\r\n");
    c.command(1.0, 1, "echo secretvm3x", &["secretvm3x"], 0);
    c.command(2.0, 2, "echo fine", &["fine"], 0);
    c.flush();
    c
}

fn forgotten_event(n: u32, seq: u64) -> BusEvent {
    BusEvent::Forgotten(Forgotten {
        target: ForgetTarget::Command { uuid: uuid(n), seq },
        by: "cli".into(),
        at: crate::now(),
    })
}

#[tokio::test]
async fn forgetting_while_watching_rejoins_without_it() {
    let kit = Kit::new(8);
    let mut c = secret_session(&kit, 10);
    register(&kit, 8, 10, &c.path, RecState::Live, None, true);
    kit.index(&c.path, None);
    let mut s = Client::open(&kit, &live_uri(10)).await.unwrap();
    assert!(shows(&s.message().await, "secretvm3x"));

    kit.cli_forget(ImTarget::Command { uuid: uuid(10), seq: 1 });
    kit.state.bus.publish(forgotten_event(10, 1));
    let init = s.message().await;
    assert!(init.get("cols").is_some(), "a fresh LiveInit: {init}");
    assert!(!shows(&init, "secretvm3x"), "{:?}", screen(&init));
    assert!(
        shows(&init, "[redacted by cli]") && shows(&init, "fine"),
        "{:?}",
        screen(&init)
    );

    c.o(3.0, "later\r\n");
    c.flush();
    assert_eq!(s.message().await, serde_json::json!([3.0, "o", "later\r\n"]));

    // Without a bus event the list is re-read every 2 s; forgetting the session ends it.
    kit.cli_forget(ImTarget::Session { uuid: uuid(10) });
    s.expect_done().await;
}

#[tokio::test]
async fn running_forgotten_command_is_masked_as_it_grows() {
    let kit = Kit::new(8);
    let mut c = Cast::new(kit.dir.path(), 11, 80, 24, &Meta::default());
    c.command(0.5, 1, "true", &[], 0);
    c.o(1.0, "$ cat secrets\r\n");
    c.m(1.01, &cmd(2, 100, 2, "cat secrets"));
    c.o(1.02, "first secret line\r\n");
    c.flush();
    register(&kit, 9, 11, &c.path, RecState::Live, None, true);
    kit.index(&c.path, None);
    kit.cli_forget(ImTarget::Command { uuid: uuid(11), seq: 2 });
    let mut s = Client::open(&kit, &live_uri(11)).await.unwrap();
    let init = s.message().await;
    assert!(!shows(&init, "secret"), "{:?}", screen(&init));
    assert!(shows(&init, "[redacted by cli]"));
    // Still running: what it prints next is hidden too.
    c.o(1.5, "second secret line\r\n");
    c.o(2.0, "x");
    c.flush();
    c.exit(2.5);
    s.expect_done().await;
}

/// Whether a live message shows any of `needles`: on the screen of a `LiveInit`, in the data
/// of an event.
fn leaks(msg: &Value, needles: &[&str]) -> bool {
    if msg.get("init").is_some() {
        let text = screen(msg).join("\n");
        return needles.iter().any(|n| text.contains(n));
    }
    let raw = msg.to_string();
    needles.iter().any(|n| raw.contains(n))
}

/// Reads messages until one satisfies `until`, none of them leaking `needles`.
async fn read_until(s: &mut Client, needles: &[&str], until: impl Fn(&Value) -> bool) -> Value {
    loop {
        let msg = s.message().await;
        assert!(
            !leaks(&msg, needles),
            "leaked: {msg} {:?}",
            msg.get("init").map(|_| screen(&msg))
        );
        if until(&msg) {
            return msg;
        }
    }
}

#[tokio::test]
async fn forgotten_commands_of_an_unindexed_session_stay_masked() {
    const SECRETS: &[&str] = &["secretvm3x", "keyvm3x", "cat key"];
    let kit = Kit::new(8);
    let mut c = secret_session(&kit, 14);
    // Live, forgotten, never indexed (index.db missing): the registry and cast describe it.
    register(&kit, 10, 14, &c.path, RecState::Live, None, true);
    kit.cli_forget(ImTarget::Command { uuid: uuid(14), seq: 1 });
    assert!(!kit.ctx().db.index.exists());
    let st = kit.state.clone();
    let (indexed, mask) = tokio::task::spawn_blocking(move || follow::source_mask(&st, &uuid(14)).unwrap())
        .await
        .unwrap();
    assert!(!indexed);
    assert!(mask.seqs.contains(&1), "{mask:?}");
    assert_eq!(mask.spans.len(), 1, "{mask:?}");
    assert!(
        mask.spans[0].from_us <= 1_010_000 && mask.spans[0].to_us >= 1_020_000,
        "{mask:?}"
    );

    let mut s = Client::open(&kit, &live_uri(14)).await.unwrap();
    let init = s.message().await;
    assert!(!leaks(&init, SECRETS), "{:?}", screen(&init));
    assert!(
        shows(&init, "[redacted by cli]") && shows(&init, "fine"),
        "{:?}",
        screen(&init)
    );

    // A command forgotten while it runs: neither its label nor its output, now or later.
    c.o(3.0, "$ cat key\r\n");
    c.m(3.01, &cmd(3, 100, 3, "cat key"));
    c.o(3.02, "keyvm3x one\r\n");
    c.flush();
    // Until it is forgotten it streams like any command; the stream may already have sent
    // it (it polls the cast), so wait for that before the forget makes it secret.
    loop {
        let msg = s.message().await;
        if msg.to_string().contains("keyvm3x one") {
            break;
        }
    }
    kit.cli_forget(ImTarget::Command { uuid: uuid(14), seq: 3 });
    kit.state.bus.publish(forgotten_event(14, 3));
    let init = read_until(&mut s, SECRETS, |m| m.get("init").is_some()).await;
    assert!(shows(&init, "[redacted by cli]"), "{:?}", screen(&init));
    c.o(3.5, "keyvm3x two\r\n");
    c.m(3.6, &crate::api::testkit::done(3, 100, 3, 0));
    c.o(4.0, "after it\r\n");
    c.flush();
    // Its end is learnt from the markers: what follows is shown (a fresh join).
    let init = read_until(&mut s, SECRETS, |m| m.get("init").is_some_and(|_| shows(m, "after it"))).await;
    assert!(shows(&init, "fine"), "{:?}", screen(&init));

    // Indexed now: still masked, and the stream goes on.
    kit.index(&c.path, None);
    let st = kit.state.clone();
    let (indexed, mask) = tokio::task::spawn_blocking(move || follow::source_mask(&st, &uuid(14)).unwrap())
        .await
        .unwrap();
    assert!(indexed);
    assert!(mask.seqs.contains(&1) && mask.seqs.contains(&3), "{mask:?}");
    tokio::time::sleep(follow::MASK_EVERY + Duration::from_millis(300)).await;
    c.o(5.0, "tail\r\n");
    c.flush();
    read_until(&mut s, SECRETS, |m| *m == serde_json::json!([5.0, "o", "tail\r\n"])).await;
    c.exit(5.5);
    s.expect_done().await;
}

#[test]
fn marker_scans_continue_where_they_stopped() {
    let kit = Kit::new(8);
    let mut c = Cast::new(kit.dir.path(), 15, 80, 24, &Meta::default());
    c.command(1.0, 1, "true", &[], 0);
    c.o(2.0, "$ cat key\r\n");
    c.m(2.01, &cmd(2, 100, 2, "cat key"));
    c.o(2.02, "keyvm3x\r\n");
    c.flush();
    register(&kit, 11, 15, &c.path, RecState::Live, None, true);
    kit.cli_forget(ImTarget::Command { uuid: uuid(15), seq: 2 });
    let st = &kit.state;
    let (_, mask) = follow::source_mask(st, &uuid(15)).unwrap();
    // From the `done` before it, open while it runs.
    assert_eq!((mask.spans[0].from_us, mask.spans[0].to_us), (1_120_000, u64::MAX));
    let first = st.play.markers.position(&uuid(15)).unwrap();
    c.m(3.0, &crate::api::testkit::done(2, 100, 2, 0));
    c.flush();
    let (_, mask) = follow::source_mask(st, &uuid(15)).unwrap();
    assert_eq!((mask.spans[0].from_us, mask.spans[0].to_us), (1_120_000, 3_000_000));
    assert!(st.play.markers.position(&uuid(15)).unwrap() > first);
}

/// Output every 10 s for `secs` seconds (the indexer cuts a playback segment every 60 s).
fn long_session(kit: &Kit, n: u32, secs: u32) -> Cast {
    let mut c = Cast::new(kit.dir.path(), n, 80, 24, &Meta::default());
    for i in 0..secs / 10 {
        let t = f64::from(i * 10) + 0.5;
        c.o(t, &format!("line {i}\r\n"));
        if i == 13 {
            c.ev(t + 0.1, EventCode::Resize, "90x20");
        }
    }
    c.flush();
    c
}

#[test]
fn join_starts_at_the_newest_clean_segment() {
    let kit = Kit::new(8);
    let c = long_session(&kit, 12, 300);
    kit.index(&c.path, None);
    let st = &kit.state;
    let start_us = follow::checkpoint_us(st, &uuid(12)).unwrap();
    assert!(start_us >= 180_000_000, "joined at {start_us} µs");
    let init = follow::init_of(&follow::Follower::open(st, &uuid(12)).unwrap());
    // The same terminal as a replay from the start, at the size the resize left.
    let full = follow::replay_from_origin(st, &uuid(12)).unwrap();
    assert_eq!((init.cols, init.rows), (90, 20));
    assert_eq!((full.cols, full.rows), (90, 20));
    let text = |i: &crate::api::types::LiveInit| screen(&serde_json::to_value(i).unwrap());
    assert_eq!(text(&init), text(&full));
    assert_eq!(init.time, full.time);
    // The size scan of the next join continues from the cache.
    let at = st.live.cached_size(&uuid(12), (usize::MAX, 0)).unwrap();
    assert_eq!(at.1, (90, 20));

    // Something forgotten before the newest segments: they are not used.
    kit.cli_forget(ImTarget::Range {
        uuid: uuid(12),
        t_from: 200.0,
        t_to: 210.0,
    });
    let start_us = follow::checkpoint_us(st, &uuid(12)).unwrap();
    assert!(start_us < 200_000_000, "joined at {start_us} µs");
    let init = follow::init_of(&follow::Follower::open(st, &uuid(12)).unwrap());
    assert!(!text(&init).iter().any(|l| l.contains("line 20")));
}

#[tokio::test]
async fn masked_large_cast_joins_from_the_masked_plan() {
    // Threshold 0: every cast plays segmented.
    let kit = Kit::new(0);
    let mut c = long_session(&kit, 13, 300);
    c.o(301.0, "$ echo secretvm3x\r\n");
    c.m(301.01, &cmd(1, 100, 1, "echo secretvm3x"));
    c.o(301.02, "secretvm3x\r\n");
    c.m(301.1, &crate::api::testkit::done(1, 100, 1, 0));
    c.flush();
    kit.index(&c.path, None);
    kit.cli_forget(ImTarget::Range {
        uuid: uuid(13),
        t_from: 5.0,
        t_to: 6.0,
    });
    let mut s = Client::open(&kit, &live_uri(13)).await.unwrap();
    let init = s.message().await;
    assert!(shows(&init, "secretvm3x") && shows(&init, "line 29"));
    s.message().await; // offline: no recorder
    s.expect_done().await;
    let st = kit.state.clone();
    let start_us = tokio::task::spawn_blocking(move || follow::checkpoint_us(&st, &uuid(13)).unwrap())
        .await
        .unwrap();
    assert!(start_us > 0, "a plan segment, not the cast start");
}

#[tokio::test]
async fn legacy_sessions_show_their_final_screen() {
    use crate::api::testkit::legacy_gen::{Cmd, Gen};
    let kit = Kit::new(8);
    let mut g = Gen::new(3).with_size(60, 12);
    g.draw_prompt();
    g.run(&Cmd::new("echo legacy").out(&["legacy output"]));
    let legacy = g.finish(0, true);
    let dir = kit.dir.path().join("logs/old");
    std::fs::create_dir_all(&dir).unwrap();
    let path = legacy.write(&dir, "legacy.cast").unwrap();
    kit.index_legacy(&path);
    let page = kit.json("/sessions?legacy=true").await;
    let u = page["sessions"][0]["uuid"].as_str().unwrap().to_owned();
    let mut s = Client::open(&kit, &format!("/sessions/{u}/live")).await.unwrap();
    assert!(shows(&s.message().await, "legacy output"));
    s.expect_done().await;
}

#[tokio::test]
async fn shutdown_and_client_disconnect_end_the_follower() {
    let kit = Kit::new(8);
    let mut c = Cast::new(kit.dir.path(), 14, 80, 24, &Meta::default());
    c.o(0.5, "x\r\n");
    c.flush();
    register(&kit, 10, 14, &c.path, RecState::Live, None, true);
    let mut s = Client::open(&kit, &live_uri(14)).await.unwrap();
    s.message().await;
    assert_eq!(kit.state.live.live_streams.load(std::sync::atomic::Ordering::SeqCst), 1);
    kit.state.bus.close();
    assert!(s.next().await.is_none(), "shutdown ends the stream");
    drop(s);
    assert_eq!(kit.state.live.live_streams.load(std::sync::atomic::Ordering::SeqCst), 0);
}

#[tokio::test]
async fn at_most_64_streams_of_each_kind() {
    let kit = Kit::new(8);
    let guards: Vec<_> = (0..MAX_STREAMS)
        .map(|_| StreamGuard::acquire(&kit.state, Kind::Live).unwrap())
        .collect();
    let (status, body) = Client::open(&kit, &live_uri(1)).await.err().unwrap();
    assert_eq!(
        (status, body["error"].as_str()),
        (StatusCode::SERVICE_UNAVAILABLE, Some("unavailable"))
    );
    // The other kind has its own budget.
    let mut events = Client::open(&kit, "/events").await.unwrap();
    assert_eq!(events.named().await.0, "hello");
    drop(guards);
    assert!(StreamGuard::acquire(&kit.state, Kind::Live).is_ok());
}

// ------------------------------------------------------------------ watcher

#[test]
fn watcher_publishes_session_start_update_and_end() {
    let kit = Kit::new(8);
    let st = &kit.state;
    let mut rx = st.bus.subscribe();
    let mut w = Watcher::new(st, fast());
    let mut c = Cast::new(kit.dir.path(), 20, 80, 24, &Meta::default());
    c.o(0.5, "hi\r\n");
    c.flush();
    w.tick(st, Instant::now());
    assert!(drain(&mut rx).is_empty());

    register(&kit, 3, 20, &c.path, RecState::Live, None, true);
    w.tick(st, Instant::now());
    let evs = drain(&mut rx);
    let [BusEvent::SessionStart(l)] = evs.as_slice() else {
        panic!("{evs:?}")
    };
    assert_eq!(l.session.uuid, uuid(20));
    assert_eq!((l.status, l.reason.as_deref()), (RecStatus::Recording, None));
    assert_eq!((l.session.commands, l.session.duration), (0, None), "from the cast");
    assert_eq!(l.session.tmux.pane.as_deref(), Some("%3"));
    assert_eq!(l.session.state, SessionState::Live);
    assert!(l.since.is_some());
    assert_eq!(st.live.snapshot().len(), 1);
    w.tick(st, Instant::now());
    assert!(drain(&mut rx).is_empty(), "nothing changed");

    register(
        &kit,
        3,
        20,
        &c.path,
        RecState::Paused,
        Some("multiplexer: tmux: client"),
        true,
    );
    w.tick(st, Instant::now());
    let evs = drain(&mut rx);
    let [BusEvent::SessionUpdate(l)] = evs.as_slice() else {
        panic!("{evs:?}")
    };
    assert_eq!(l.status, RecStatus::Paused);
    assert_eq!(
        (l.reason.as_deref(), l.detail.as_deref()),
        (Some("multiplexer"), Some("tmux: client"))
    );
    let paused_since = l.since;

    // Indexed: updated once (the status, and so `since`, stays).
    c.command(1.0, 1, "ls", &["a"], 0);
    c.flush();
    kit.index(&c.path, None);
    w.tick(st, Instant::now());
    let evs = drain(&mut rx);
    let [BusEvent::SessionUpdate(l)] = evs.as_slice() else {
        panic!("{evs:?}")
    };
    assert!(l.session.duration.is_some());
    assert_eq!((l.session.commands, l.since), (1, paused_since));
    w.tick(st, Instant::now());
    assert!(drain(&mut rx).is_empty());
    assert_eq!(st.live.snapshot()[0].session.commands, 1, "the snapshot is current");

    // The recorder exits normally: `x` first, then its entry goes.
    c.exit(3.0);
    unregister(&kit, 3);
    w.tick(st, Instant::now());
    let evs = drain(&mut rx);
    let [BusEvent::SessionEnd(e)] = evs.as_slice() else {
        panic!("{evs:?}")
    };
    assert_eq!((e.session.as_str(), e.state), (uuid(20).as_str(), SessionState::Ended));
    assert!(st.live.snapshot().is_empty());
}

#[test]
fn a_nested_pause_names_its_child_from_the_marker() {
    let kit = Kit::new(8);
    let st = &kit.state;
    let mut rx = st.bus.subscribe();
    let mut w = Watcher::new(st, fast());
    let mut c = Cast::new(kit.dir.path(), 30, 80, 24, &Meta::default());
    c.o(0.5, "$ sudo -i\r\n");
    c.flush();
    let child = uuid(31);
    let reason = format!("nested: {child}");
    register(&kit, 3, 30, &c.path, RecState::Paused, Some(&reason), true);
    // The registry may change before the cast has the marker: no child yet.
    w.tick(st, Instant::now());
    let evs = drain(&mut rx);
    let [BusEvent::SessionStart(l)] = evs.as_slice() else {
        panic!("{evs:?}")
    };
    assert_eq!((l.detail.as_deref(), l.child_scope), (Some(child.as_str()), None));
    let since = l.since;

    // A TEAM `sudo -i` in this PERSONAL shell, served by the TEAM server.
    let url = format!("https://box.lan:10000/s/{child}");
    c.m(
        1.0,
        &format!(
            r#"{{"cl":1,"k":"nested","child_uuid":"{child}","scope":"team","user":"root","url":"{url}","mode":"pause"}}"#
        ),
    );
    c.m(
        1.0,
        &format!(r#"{{"cl":1,"k":"pause","reason":"nested","detail":"{child}"}}"#),
    );
    c.flush();
    w.tick(st, Instant::now());
    let evs = drain(&mut rx);
    let [BusEvent::SessionUpdate(l)] = evs.as_slice() else {
        panic!("{evs:?}")
    };
    assert_eq!(l.child_scope, Some(crate::api::types::Scope::Team));
    assert_eq!(l.child_url.as_deref(), Some(url.as_str()));
    assert_eq!(l.since, since, "the status did not change");
    w.tick(st, Instant::now());
    assert!(drain(&mut rx).is_empty(), "nothing changed");

    // Another pause: no child.
    register(&kit, 3, 30, &c.path, RecState::Paused, Some("user: lunch"), true);
    w.tick(st, Instant::now());
    let evs = drain(&mut rx);
    let [BusEvent::SessionUpdate(l)] = evs.as_slice() else {
        panic!("{evs:?}")
    };
    assert_eq!((l.child_scope, l.child_url.as_deref()), (None, None));
    assert!(recorders::nested_child(&c.path, &uuid(32)).is_none(), "another child");
}

#[test]
fn watcher_reports_crashes_degradation_and_forgotten_sessions() {
    let kit = Kit::new(8);
    let st = &kit.state;
    let mut rx = st.bus.subscribe();
    let mut w = Watcher::new(st, fast());
    let casts: Vec<Cast> = (21..24)
        .map(|n| {
            let mut c = Cast::new(kit.dir.path(), n, 80, 24, &Meta::default());
            c.o(0.5, "x\r\n");
            c.flush();
            c
        })
        .collect();
    register(&kit, 1, 21, &casts[0].path, RecState::Live, None, true);
    register(
        &kit,
        2,
        22,
        &casts[1].path,
        RecState::Degraded,
        Some("write: No space left on device"),
        true,
    );
    register(&kit, 3, 23, &casts[2].path, RecState::Live, None, true);
    w.tick(st, Instant::now());
    assert_eq!(drain(&mut rx).len(), 3);
    let snap = st.live.snapshot().clone();
    let order: Vec<&str> = snap.iter().map(|l| l.session.uuid.as_str()).collect();
    assert_eq!(order, [uuid(23), uuid(22), uuid(21)], "newest first");
    assert_eq!(snap[1].status, RecStatus::Degraded);
    assert_eq!(snap[1].reason.as_deref(), Some("write"));

    register(&kit, 1, 21, &casts[0].path, RecState::Live, None, false);
    register(&kit, 2, 22, &casts[1].path, RecState::Degraded, None, false);
    kit.cli_forget(ImTarget::Session { uuid: uuid(23) });
    w.tick(st, Instant::now());
    let mut ends: Vec<(String, SessionState)> = drain(&mut rx)
        .into_iter()
        .filter_map(|e| match e {
            BusEvent::SessionEnd(e) => Some((e.session, e.state)),
            _ => None,
        })
        .collect();
    ends.sort_by(|a, b| a.0.cmp(&b.0));
    assert_eq!(
        ends,
        [
            (uuid(21), SessionState::Crashed),
            (uuid(22), SessionState::Degraded),
            (uuid(23), SessionState::Ended),
        ]
    );
}

#[test]
fn watcher_announces_cli_forgets_once() {
    let kit = Kit::new(8);
    let st = &kit.state;
    kit.cli_forget(ImTarget::Session { uuid: uuid(30) });
    let mut rx = st.bus.subscribe();
    let mut w = Watcher::new(st, fast());
    w.tick(st, Instant::now());
    assert!(
        drain(&mut rx).is_empty(),
        "rows from before the server started are not news"
    );

    kit.cli_forget(ImTarget::Command { uuid: uuid(31), seq: 4 });
    w.tick(st, Instant::now());
    assert!(drain(&mut rx).is_empty(), "one poll of grace for the API's own event");
    w.tick(st, Instant::now());
    let evs = drain(&mut rx);
    let [BusEvent::Forgotten(f)] = evs.as_slice() else {
        panic!("{evs:?}")
    };
    assert_eq!(f.target, ForgetTarget::Command { uuid: uuid(31), seq: 4 });
    assert_eq!(f.by, "cli");

    // `POST /api/forget` writes meta.db and publishes: not repeated.
    kit.cli_forget(ImTarget::Command { uuid: uuid(31), seq: 5 });
    w.tick(st, Instant::now());
    st.bus.publish(forgotten_event(31, 5));
    w.tick(st, Instant::now());
    w.tick(st, Instant::now());
    let evs = drain(&mut rx);
    assert_eq!(evs.len(), 1, "only the API's event: {evs:?}");
}

#[test]
fn watcher_publishes_progress_only_to_listeners_and_on_change() {
    use crate::api::testkit::legacy_gen::{Cmd, Gen};
    let kit = Kit::new(8);
    let st = &kit.state;
    let mut c = Cast::new(kit.dir.path(), 40, 80, 24, &Meta::default());
    c.command(1.0, 1, "ls", &["a"], 0);
    c.exit(2.0);
    kit.index(&c.path, None);
    let mut rx = st.bus.subscribe();
    let mut w = Watcher::new(st, fast());
    w.tick(st, Instant::now());
    assert!(drain(&mut rx).is_empty(), "no /events stream open");

    let _guard = StreamGuard::acquire(st, Kind::Events).unwrap();
    w.tick(st, Instant::now());
    let evs = drain(&mut rx);
    let [BusEvent::IndexProgress(p)] = evs.as_slice() else {
        panic!("{evs:?}")
    };
    assert_eq!(p.commands, 1);
    w.tick(st, Instant::now());
    assert!(drain(&mut rx).is_empty(), "unchanged");

    let mut g = Gen::new(5).with_size(60, 12);
    g.draw_prompt();
    g.run(&Cmd::new("echo old").out(&["old"]));
    let dir = kit.dir.path().join("logs/old");
    std::fs::create_dir_all(&dir).unwrap();
    let path = g.finish(0, true).write(&dir, "old.cast").unwrap();
    kit.index_legacy(&path);
    w.tick(st, Instant::now());
    let evs = drain(&mut rx);
    assert!(
        evs.iter()
            .any(|e| matches!(e, BusEvent::IndexProgress(p) if p.commands == 2)),
        "{evs:?}"
    );
    assert!(
        evs.iter()
            .any(|e| matches!(e, BusEvent::ImportProgress(p) if *p != LegacyProgress::default())),
        "{evs:?}"
    );
}

// ------------------------------------------------------------------ /events

#[tokio::test]
async fn events_start_with_hello_and_carry_the_bus() {
    let kit = Kit::new(8);
    let mut c = Cast::new(kit.dir.path(), 50, 80, 24, &Meta::default());
    c.o(0.5, "x\r\n");
    c.flush();
    register(&kit, 3, 50, &c.path, RecState::Paused, Some("user: lunch"), true);
    let mut s = Client::open(&kit, "/events").await.unwrap();
    let (name, hello) = s.named().await;
    assert_eq!(name, "hello");
    assert_eq!(hello["version"], crate::VERSION);
    assert_eq!(hello["live"][0]["session"]["uuid"], uuid(50));
    assert_eq!(hello["live"][0]["status"], "paused");
    assert_eq!(hello["live"][0]["detail"], "lunch");
    kit.state.bus.publish(BusEvent::Tag(Tag {
        name: "incident".into(),
        color: None,
        count: 1,
    }));
    assert_eq!(
        s.named().await,
        (
            "tag".into(),
            serde_json::json!({"name": "incident", "color": null, "count": 1})
        )
    );
    kit.state.bus.close();
    assert!(s.next().await.is_none());
}

#[tokio::test]
async fn lagging_subscribers_get_a_fresh_hello() {
    let kit = Kit::new(8);
    let rx = kit.state.bus.subscribe();
    let tag = |n: usize| {
        BusEvent::Tag(Tag {
            name: format!("t{n}"),
            color: None,
            count: 0,
        })
    };
    for n in 0..BUS_CAPACITY + 10 {
        kit.state.bus.publish(tag(n));
    }
    let frames: Vec<Bytes> = {
        use futures_util::StreamExt;
        let stream = bus_stream(kit.state.clone(), rx, Vec::new());
        let mut stream = std::pin::pin!(stream);
        let mut out = Vec::new();
        for _ in 0..3 {
            out.push(stream.next().await.unwrap());
        }
        out
    };
    let name = |b: &Bytes| String::from_utf8_lossy(b).lines().next().unwrap().to_owned();
    assert_eq!(name(&frames[0]), "event: hello");
    assert_eq!(name(&frames[1]), "event: hello", "resync after the lag");
    assert_eq!(frames[2], sse::bus_event(&tag(10)));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn running_watcher_feeds_hello_and_session_events() {
    let kit = Kit::new(8);
    let mut a = Cast::new(kit.dir.path(), 60, 80, 24, &Meta::default());
    a.o(0.5, "a\r\n");
    a.flush();
    register(&kit, 1, 60, &a.path, RecState::Live, None, true);
    start(&kit.state);
    start(&kit.state);
    let mut s = Client::open(&kit, "/events").await.unwrap();
    let (name, hello) = s.named().await;
    assert_eq!(name, "hello");
    assert_eq!(hello["live"].as_array().unwrap().len(), 1, "{hello}");

    let mut b = Cast::new(kit.dir.path(), 61, 80, 24, &Meta::default());
    b.o(0.5, "b\r\n");
    b.flush();
    let started = Instant::now();
    register(&kit, 2, 61, &b.path, RecState::Live, None, true);
    loop {
        let (name, data) = s.named().await;
        if name == "session-start" {
            assert_eq!(data["session"]["uuid"], uuid(61));
            break;
        }
    }
    assert!(started.elapsed() < Duration::from_secs(1), "{:?}", started.elapsed());

    let ended = Instant::now();
    b.exit(1.0);
    unregister(&kit, 2);
    loop {
        let (name, data) = s.named().await;
        if name == "session-end" {
            assert_eq!(
                (data["session"].clone(), data["state"].clone()),
                (uuid(61).into(), "ended".into())
            );
            break;
        }
    }
    assert!(ended.elapsed() < Duration::from_secs(1), "{:?}", ended.elapsed());

    // A `cinelog forget` (meta.db only) reaches the bus within 2 s.
    let forgot = Instant::now();
    kit.cli_forget(ImTarget::Command { uuid: uuid(60), seq: 1 });
    loop {
        let (name, data) = s.named().await;
        if name == "forgotten" {
            assert_eq!(
                data["target"],
                serde_json::json!({"kind": "command", "uuid": uuid(60), "seq": 1})
            );
            break;
        }
    }
    assert!(forgot.elapsed() < Duration::from_secs(2), "{:?}", forgot.elapsed());
    kit.state.bus.close();
    assert!(s.next().await.is_none());
}

// ------------------------------------------------------------------ helpers

#[test]
fn registry_dirs_include_the_personal_fallback() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = crate::state::WebContext::for_tests(dir.path());
    let dirs = recorders::registry_dirs(&ctx);
    assert_eq!(dirs, [ctx.paths.runtime_dir.clone(), ctx.paths.state_dir.join("run")]);
    let mut team = ctx.clone();
    team.scope = Scope::Team;
    assert_eq!(recorders::registry_dirs(&team), [ctx.paths.runtime_dir]);
}

#[test]
fn exit_is_found_at_the_end_of_a_cast() {
    let kit = Kit::new(8);
    let mut c = Cast::new(kit.dir.path(), 70, 80, 24, &Meta::default());
    c.o(0.5, &"y".repeat(100_000));
    c.flush();
    assert!(!recorders::has_exit(&c.path));
    c.exit(1.0);
    assert!(recorders::has_exit(&c.path));
    assert!(!recorders::has_exit(&PathBuf::from("/nonexistent/x.cast")));
}
