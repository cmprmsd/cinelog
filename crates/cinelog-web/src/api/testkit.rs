//! Test fixtures of M3-B: cinelog casts written with the real cast writer, indexed into a
//! real index.db by the M2 pipeline and writer, and the M3-B routes behind a fixed
//! principal.

use std::fs::File;
use std::io::BufWriter;
use std::path::{Path, PathBuf};

use axum::Router;
use axum::body::Body;
use axum::http::{HeaderMap, Request, StatusCode, header};
use cinelog_cast::{CastWriter, EventCode, Header, V3Writer};
use cinelog_core::config::IndexConfig;
use cinelog_index::IndexWriter;
use cinelog_index::model::{PollBudget, RenderOptions, SessionState};
use http_body_util::BodyExt;
use serde_json::Value;
use tower::ServiceExt;

use crate::state::{AppState, Bound, Principal, WebContext};

/// The synthetic legacy (old plugin) cast generator shared with cinelog-index's tests.
#[path = "../../../../tests/fixtures/legacy-gen/legacy_gen.rs"]
pub(crate) mod legacy_gen;

pub(crate) const START_US: i64 = 1_731_070_000_000_000;

pub(crate) fn uuid(n: u32) -> String {
    format!("0193a1b2-0000-7000-8000-{n:012}")
}

/// A cinelog cast being written.
pub(crate) struct Cast {
    w: V3Writer<BufWriter<File>>,
    pub path: PathBuf,
}

pub(crate) struct Meta<'a> {
    pub user: &'a str,
    pub pane: &'a str,
    pub parent: Option<&'a str>,
    pub holder: Option<&'a str>,
}

impl Default for Meta<'_> {
    fn default() -> Self {
        Self {
            user: "u",
            pane: "%3",
            parent: None,
            holder: None,
        }
    }
}

pub(crate) fn cmd(seq: u64, sh: u32, n: u64, typed: &str) -> String {
    let typed = serde_json::to_string(typed).unwrap();
    format!(r#"{{"cl":1,"k":"cmd","seq":{seq},"sh":{sh},"n":{n},"typed":{typed},"cwd":"/tmp","user":"u","euid":1000}}"#)
}

pub(crate) fn done(seq: u64, sh: u32, n: u64, exit: i32) -> String {
    format!(r#"{{"cl":1,"k":"done","seq":{seq},"sh":{sh},"n":{n},"exit":{exit},"pipestatus":[{exit}]}}"#)
}

fn stem(n: u32) -> String {
    format!("h_20261108T101500.000000_{n:08}")
}

impl Cast {
    pub fn new(dir: &Path, n: u32, cols: u16, rows: u16, meta: &Meta<'_>) -> Self {
        Self::part(dir, n, 1, 0.0, cols, rows, meta)
    }

    /// Rotation part `part` of session `n`, starting `offset` seconds into the session.
    pub fn part(dir: &Path, n: u32, part: u32, offset: f64, cols: u16, rows: u16, meta: &Meta<'_>) -> Self {
        let uuid = uuid(n);
        let logs = dir.join("logs/2026-11");
        std::fs::create_dir_all(&logs).unwrap();
        let path = logs.join(cinelog_cast::part_file_name(&stem(n), part, false));
        let mut w = cinelog_cast::create(&path).unwrap();
        let offset_us = (offset * 1e6).round() as i64;
        let start = START_US + i64::from(n) * 1_000_000 + offset_us;
        let boot = 5_000_000 + offset_us;
        w.write_header(&Header::new_v3(
            cols,
            rows,
            Some("xterm-256color".into()),
            start / 1_000_000,
            None,
        ))
        .unwrap();
        let parent = meta
            .parent
            .map_or(String::new(), |p| format!(r#","parent_uuid":"{p}""#));
        let holder = meta
            .holder
            .map_or(String::new(), |h| format!(r#","holder_comm":"{h}""#));
        let tmux = if meta.pane.is_empty() {
            String::new()
        } else {
            format!(
                r#","tmux":{{"socket":"/tmp/s","server_pid":42,"server_start":7,"session_id":"$1","pane":"{}"}}"#,
                meta.pane
            )
        };
        let session = format!(
            r#"{{"cl":1,"k":"session","uuid":"{uuid}","part":{part},"start_epoch_us":{start},"boottime_us":{boot},"tz":"UTC","scope":"personal","host":"testhost","user":"{}","euid":1000,"attribution":"ancestor","tty":"/dev/pts/3"{tmux}{parent}{holder}}}"#,
            meta.user
        );
        w.write_event(0, EventCode::Marker, &session).unwrap();
        Self { w, path }
    }

    pub fn ev(&mut self, t: f64, code: EventCode, data: &str) -> &mut Self {
        self.w.write_event((t * 1e6).round() as u64, code, data).unwrap();
        self
    }

    pub fn o(&mut self, t: f64, data: &str) -> &mut Self {
        self.ev(t, EventCode::Output, data)
    }

    pub fn m(&mut self, t: f64, data: &str) -> &mut Self {
        self.ev(t, EventCode::Marker, data)
    }

    /// One command at `t`: prompt, `cmd`, its output lines, `done` 0.1 s after the last.
    pub fn command(&mut self, t: f64, seq: u64, typed: &str, out: &[&str], exit: i32) -> f64 {
        self.o(t, &format!("$ {typed}\r\n"));
        self.m(t + 0.01, &cmd(seq, 100, seq, typed));
        let mut at = t + 0.02;
        for line in out {
            self.o(at, &format!("{line}\r\n"));
            at += 0.01;
        }
        self.m(at + 0.1, &done(seq, 100, seq, exit));
        at + 0.1
    }

    pub fn exit(&mut self, t: f64) {
        self.ev(t, EventCode::Exit, "0");
        self.flush();
    }

    /// Ends this part (session `n`, part `part`) with the recorder's `rotate` marker.
    pub fn rotate(&mut self, t: f64, n: u32, part: u32) {
        let next = cinelog_cast::part_file_name(&stem(n), part + 1, false);
        self.m(
            t,
            &format!(r#"{{"cl":1,"k":"rotate","part":{part},"next":"{next}","reason":"size"}}"#),
        );
        self.flush();
    }

    /// Compresses the finished part at `path` into small zstd frames and removes the plain
    /// file; returns the compressed path.
    pub fn compress(path: &Path) -> PathBuf {
        let zst = path.with_file_name(format!("{}.zst", path.file_name().unwrap().to_string_lossy()));
        let opts = cinelog_cast::CompressOptions {
            frame_bytes: 2048,
            ..Default::default()
        };
        let out = cinelog_cast::compress_file(path, &zst, opts).unwrap();
        assert!(out.frames.len() > 1, "several frames");
        std::fs::remove_file(path).unwrap();
        zst
    }

    pub fn flush(&mut self) {
        self.w.flush().unwrap();
    }
}

/// A server state below a temp dir; `threshold_mb` is `index.playback_segment_threshold_mb`.
pub(crate) struct Kit {
    pub dir: tempfile::TempDir,
    pub state: AppState,
}

impl Kit {
    pub fn new(threshold_mb: u64) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let mut ctx = WebContext::for_tests(dir.path());
        ctx.config.index.playback_segment_threshold_mb = threshold_mb;
        std::fs::create_dir_all(&ctx.paths.db_dir).unwrap();
        let hosts = crate::hosts::HostAllowlist::new(&ctx);
        let state = AppState::new(
            ctx,
            Bound {
                port: 10443,
                fingerprint: "AB:CD".into(),
                url: "https://testhost:10443".into(),
            },
            hosts,
        );
        Self { dir, state }
    }

    pub fn ctx(&self) -> &WebContext {
        &self.state.ctx
    }

    pub fn writer(&self) -> Box<dyn IndexWriter> {
        cinelog_index::db::open_writer(&self.ctx().db, &IndexConfig::default()).unwrap()
    }

    /// Indexes `path` completely (`max_bytes`: at most one poll of that size when `Some`).
    pub fn index(&self, path: &Path, max_bytes: Option<u64>) {
        let factory = cinelog_index::render::cast_pipelines(RenderOptions::default()).unwrap();
        let mut w = self.writer();
        let mut p = factory.open(path, None).unwrap();
        let budget = PollBudget {
            max_bytes: max_bytes.unwrap_or(u64::MAX),
            live: false,
        };
        let mut finished = false;
        while let Some(b) = p.poll(budget).unwrap() {
            finished |= b.finished;
            w.apply(&b).unwrap();
            if max_bytes.is_some() {
                return;
            }
        }
        if !finished && p.is_finished() {
            w.apply(&p.finish(SessionState::Ended).unwrap()).unwrap();
        }
    }

    /// Imports a legacy (asciinema v2 plus commands.log) cast completely.
    pub fn index_legacy(&self, path: &Path) {
        let factory = cinelog_index::legacy::pipelines(cinelog_index::model::LegacyOptions {
            render: RenderOptions::default(),
            scope: cinelog_proto::Scope::Personal,
            logs_root: self.dir.path().join("logs"),
            host: "testhost".into(),
        })
        .unwrap();
        let mut w = self.writer();
        let mut p = factory.open(path, None).unwrap();
        let budget = PollBudget {
            max_bytes: u64::MAX,
            live: false,
        };
        while let Some(b) = p.poll(budget).unwrap() {
            let done = b.finished;
            w.apply(&b).unwrap();
            if done {
                break;
            }
        }
    }

    /// Records a forget the way `cinelog forget` does (meta.db only).
    pub fn cli_forget(&self, target: cinelog_index::model::ForgetTarget) {
        let mut meta = cinelog_index::db::open_meta(&self.ctx().db).unwrap();
        meta.forget(&target, "cli").unwrap();
    }

    /// The serve daemon's forget sync: purges what meta.db lists from index.db.
    pub fn purge(&self) {
        let meta = cinelog_index::db::open_meta(&self.ctx().db).unwrap();
        let list = meta.forgotten().unwrap();
        self.writer().apply_forgotten(&list).unwrap();
    }

    pub fn router(&self) -> Router {
        Router::new()
            .merge(super::routes())
            .merge(crate::play::routes())
            .merge(crate::export::routes())
            .layer(axum::middleware::from_fn(
                |mut req: Request<Body>, next: axum::middleware::Next| async move {
                    req.extensions_mut().insert(Principal("alice".into()));
                    next.run(req).await
                },
            ))
            .with_state(self.state.clone())
    }

    pub async fn request(&self, req: Request<Body>) -> (StatusCode, HeaderMap, Vec<u8>) {
        let resp = self.router().oneshot(req).await.unwrap();
        let (parts, body) = resp.into_parts();
        let bytes = body.collect().await.unwrap().to_bytes().to_vec();
        (parts.status, parts.headers, bytes)
    }

    pub async fn get(&self, uri: &str) -> (StatusCode, HeaderMap, Vec<u8>) {
        self.request(Request::get(uri).body(Body::empty()).unwrap()).await
    }

    /// GET expecting 200 and JSON.
    pub async fn json(&self, uri: &str) -> Value {
        let (status, _, body) = self.get(uri).await;
        assert_eq!(status, StatusCode::OK, "{uri}: {}", String::from_utf8_lossy(&body));
        serde_json::from_slice(&body).unwrap()
    }

    pub async fn post(&self, uri: &str, body: &Value) -> (StatusCode, Value) {
        let req = Request::post(uri)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(serde_json::to_vec(body).unwrap()))
            .unwrap();
        let (status, _, body) = self.request(req).await;
        (status, serde_json::from_slice(&body).unwrap_or(Value::Null))
    }
}

/// The player's checks of a segmented recording (asciinema-player 3.18 `validateIndex` and
/// `normalizeSegment`, with its millisecond arithmetic), applied to an index and all of its
/// segments.
pub(crate) fn validate_segmented(index: &Value, segments: &[Value]) {
    let ms = |v: &Value| v.as_f64().unwrap() * 1000.0;
    assert_eq!(index["version"], 1);
    let duration = ms(&index["duration"]);
    let segs = index["segments"].as_array().unwrap();
    assert!(!segs.is_empty());
    assert_eq!(segs.len(), segments.len());
    let mut prev = -1.0;
    for (i, s) in segs.iter().enumerate() {
        let start = ms(&s["start"]);
        if i == 0 {
            assert_eq!(start, 0.0);
        } else {
            assert!(start > prev && start < duration, "segment {i} start");
        }
        assert!(!s["url"].as_str().unwrap().is_empty());
        prev = start;
    }
    let markers = index["markers"].as_array().unwrap();
    let mut prev = -1.0;
    for m in markers {
        let t = ms(&m[0]);
        assert!(m[1].is_string());
        assert!(t >= prev && t <= duration, "marker order");
        prev = t;
    }
    for (i, payload) in segments.iter().enumerate() {
        let start = ms(&segs[i]["start"]);
        let end = segs.get(i + 1).map_or(duration, |s| ms(&s["start"]));
        let snap = &payload["snapshot"];
        assert!(snap["cols"].as_u64().unwrap() > 0 && snap["rows"].as_u64().unwrap() > 0);
        assert!(snap["init"].is_string());
        let events = payload["events"].as_array().unwrap();
        assert!(!events.is_empty(), "segment {i} has events");
        let mut marker_index = markers.iter().position(|m| ms(&m[0]) >= start).unwrap_or(markers.len());
        let mut prev = -1.0;
        for e in events {
            let t = ms(&e[0]);
            assert!(e[1].is_string());
            let last = i + 1 == segs.len();
            assert!(
                t >= prev && t >= start && if last { t <= end } else { t < end },
                "segment {i}: event {e} out of [{start}, {end})"
            );
            prev = t;
            if e[1] == "m" {
                assert_eq!(e[2], markers[marker_index][1], "segment {i}: marker {marker_index}");
                marker_index += 1;
            }
        }
        if i > 0 {
            assert_eq!(ms(&events[0][0]), start, "segment {i} first event");
        }
        if i + 1 == segs.len() {
            assert_eq!(ms(&events[events.len() - 1][0]), duration, "final event");
        }
    }
}
