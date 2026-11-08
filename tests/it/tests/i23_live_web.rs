//! I23 (M4 accept 1, 3, 4 and 5 against the real stack): a shell recorded in a sandbox tmux
//! while `cinelog serve` runs with its web server.
//!
//! - The UI bus announces the live session within 2 s and drops it within 2 s of a recorder
//!   crash.
//! - A late joiner of `/live` gets the current screen in `LiveInit`, appended output follows
//!   within 2 s, the stream has no input path, and a crash ends it with `offline` + `done`.
//! - An annotation reaches a second bus subscriber within 2 s and survives `index --rebuild`
//!   and a meta.db export, delete and import.
//! - The three exports of a command (`cast`, `txt`, `html`) and its timeline block.

use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use cinelog_it::{Need, Sandbox, Serve, need, scaled, wait_until};
use serde_json::Value;

const UA: &str = "it-agent-m4";

struct Web {
    base: String,
    cookie: String,
}

#[derive(Debug)]
struct Reply {
    status: u16,
    body: String,
}

impl Web {
    fn login(base: String, token: &str) -> Result<Self> {
        let body = serde_json::json!({ "token": token }).to_string();
        let out = Command::new("curl")
            .args(["-sk", "-o", "/dev/null", "-D", "-", "-A", UA])
            .args(["-H", &format!("Origin: {base}"), "-H", "Content-Type: application/json"])
            .args(["--data", &body, &format!("{base}/api/login")])
            .output()
            .context("curl login")?;
        let headers = String::from_utf8_lossy(&out.stdout);
        let cookie = headers
            .lines()
            .find_map(|l| {
                l.strip_prefix("set-cookie: ")
                    .or_else(|| l.strip_prefix("Set-Cookie: "))
            })
            .with_context(|| format!("login set no cookie:\n{headers}"))?;
        Ok(Self {
            base,
            cookie: cookie.split(';').next().unwrap_or_default().to_owned(),
        })
    }

    fn request(&self, method: &str, path: &str, json: Option<&Value>) -> Result<Reply> {
        let mut cmd = Command::new("curl");
        cmd.args(["-sk", "-X", method, "-o", "-", "-w", "\n%{http_code}", "-A", UA])
            .args(["-H", &format!("Cookie: {}", self.cookie)])
            .args(["-H", &format!("Origin: {}", self.base)]);
        if let Some(body) = json {
            cmd.args(["-H", "Content-Type: application/json", "--data", &body.to_string()]);
        }
        let out = cmd.arg(format!("{}{path}", self.base)).output().context("curl")?;
        let stdout = String::from_utf8_lossy(&out.stdout);
        let (body, code) = stdout.rsplit_once('\n').context("curl wrote no status")?;
        Ok(Reply {
            status: code.trim().parse().with_context(|| format!("status {code:?}"))?,
            body: body.to_owned(),
        })
    }

    fn get(&self, path: &str) -> Result<Reply> {
        self.request("GET", path, None)
    }

    fn get_json(&self, path: &str) -> Result<Value> {
        let r = self.get(path)?;
        if r.status != 200 {
            bail!("GET {path}: {r:?}");
        }
        Ok(serde_json::from_str(&r.body)?)
    }

    /// An SSE stream read by a thread; every line is kept with the time it arrived.
    fn stream(&self, path: &str) -> Result<Stream> {
        let mut child = Command::new("curl")
            .args(["-skN", "-A", UA, "-H", &format!("Cookie: {}", self.cookie)])
            .arg(format!("{}{path}", self.base))
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .context("curl stream")?;
        let lines = Arc::new(Mutex::new(Vec::new()));
        let out = child.stdout.take().context("curl stdout")?;
        let sink = Arc::clone(&lines);
        thread::spawn(move || {
            for line in BufReader::new(out).lines() {
                let Ok(line) = line else { break };
                sink.lock().unwrap().push((Instant::now(), line));
            }
        });
        Ok(Stream { child, lines })
    }
}

struct Stream {
    child: Child,
    lines: Arc<Mutex<Vec<(Instant, String)>>>,
}

impl Stream {
    fn lines(&self) -> Vec<(Instant, String)> {
        self.lines.lock().unwrap().clone()
    }

    /// The first `data:` line of SSE event `event` (`None`: unnamed messages) that matches.
    fn find(&self, event: Option<&str>, pred: impl Fn(&str) -> bool) -> Option<(Instant, String)> {
        let lines = self.lines();
        let mut name: Option<String> = None;
        for (at, line) in lines {
            if let Some(n) = line.strip_prefix("event: ") {
                name = Some(n.to_owned());
            } else if let Some(data) = line.strip_prefix("data:") {
                let data = data.strip_prefix(' ').unwrap_or(data);
                if name.as_deref() == event && pred(data) {
                    return Some((at, data.to_owned()));
                }
            } else if line.is_empty() {
                name = None;
            }
        }
        None
    }

    fn wait(
        &self,
        what: &str,
        timeout: Duration,
        event: Option<&str>,
        pred: impl Fn(&str) -> bool,
    ) -> (Instant, String) {
        wait_until(what, timeout, || self.find(event, &pred))
            .unwrap_or_else(|e| panic!("{e}\nstream so far:\n{}", self.text()))
    }

    fn text(&self) -> String {
        self.lines().into_iter().map(|(_, l)| l + "\n").collect()
    }
}

impl Drop for Stream {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn server_base(run: &Path) -> Option<String> {
    let info: Value = serde_json::from_slice(&std::fs::read(run.join("cinelog/server.json")).ok()?).ok()?;
    let url = info["url"].as_str()?;
    let port = url.rsplit(':').next()?.trim_end_matches('/');
    Some(format!("https://127.0.0.1:{port}"))
}

fn start_web(sb: &Sandbox, name: &str, token: &str) -> (Serve, Web) {
    let serve = Serve::start_web(sb, name, &[]).unwrap();
    let base = wait_until("server.json", scaled(20.0), || server_base(&sb.run))
        .unwrap_or_else(|e| panic!("{e}\n{}", serve.log_text()));
    let web = Web::login(base, token).unwrap();
    (serve, web)
}

/// Within `limit` of `since` (an event that came earlier counts as on time).
fn within(at: Instant, since: Instant, limit: Duration) -> bool {
    at.saturating_duration_since(since) <= limit
}

#[test]
fn i23_live_view_annotations_exports_and_the_bus() {
    need!(
        Need::Zsh,
        Need::Tmux,
        Need::Plugin,
        Need::Recorder,
        Need::Index,
        Need::Curl
    );
    let sb = Sandbox::new().unwrap();
    let new = sb.cinelog(&["token", "new"]).unwrap().expect_ok().unwrap();
    let token = new
        .stdout
        .lines()
        .find_map(|l| l.split("(shown once): ").nth(1))
        .expect("token new prints the token")
        .trim()
        .to_owned();
    let (serve, web) = start_web(&sb, "serve", &token);
    let bus = web.stream("/api/events").unwrap();
    bus.wait("the bus hello", scaled(5.0), Some("hello"), |_| true);
    let watcher = web.stream("/api/events").unwrap();
    watcher.wait("the second subscriber's hello", scaled(5.0), Some("hello"), |_| true);

    // Accept 5: the session appears on the bus within 2 s of its cast.
    let t = sb.tmux().unwrap();
    let r = sb.start_recorded(&t, "live").unwrap();
    let recorded_at = Instant::now();
    let uuid = r.uuid().unwrap();
    let (at, _) = bus.wait("session-start", scaled(5.0), Some("session-start"), |d| {
        d.contains(&uuid)
    });
    assert!(
        within(at, recorded_at, Duration::from_secs(2)),
        "session-start came late"
    );

    r.run("echo before-join-$((6*7))").unwrap();
    // Accept 1: a late joiner gets the current screen at once.
    let live = web.stream(&format!("/api/sessions/{uuid}/live")).unwrap();
    let (_, init) = live.wait("LiveInit", scaled(5.0), None, |d| d.starts_with('{'));
    let init: Value = serde_json::from_str(&init).unwrap();
    assert!(
        init["init"].as_str().unwrap().contains("before-join-42"),
        "the init screen: {init}"
    );
    let typed_at = Instant::now();
    r.pane.type_line("echo after-join-$((6*8))").unwrap();
    let (at, _) = live.wait("appended output", scaled(5.0), None, |d| {
        d.starts_with('[') && d.contains("after-join-48")
    });
    assert!(within(at, typed_at, Duration::from_secs(2)), "output came late");
    // Read-only: the stream has no input path.
    let post = web
        .request(
            "POST",
            &format!("/api/sessions/{uuid}/live"),
            Some(&serde_json::json!({"data": "x"})),
        )
        .unwrap();
    assert_eq!(post.status, 405, "{post:?}");

    // The command reaches the index; annotate it.
    let seq = wait_until("the indexed command", scaled(15.0), || {
        let v = web.get_json(&format!("/api/sessions/{uuid}/commands")).ok()?;
        let cmds = v["commands"].as_array()?.clone();
        cmds.iter()
            .find(|c| c["typed"] == "echo before-join-$((6*7))")
            .and_then(|c| c["seq"].as_u64())
    })
    .unwrap();
    let created_at = Instant::now();
    let created = web
        .request(
            "POST",
            "/api/annotations",
            Some(&serde_json::json!({
                "session_uuid": uuid,
                "cmd_seq": seq,
                "body": "it note https://example.org/x",
                "tags": ["it-m4"],
            })),
        )
        .unwrap();
    assert_eq!(created.status, 201, "{created:?}");
    let note: Value = serde_json::from_str(&created.body).unwrap();
    let note_uuid = note["uuid"].as_str().unwrap().to_owned();
    // Accept 3: another browser hears of it within 2 s.
    let (at, _) = watcher.wait("the annotation event", scaled(5.0), Some("annotation"), |d| {
        d.contains(&note_uuid)
    });
    assert!(
        within(at, created_at, Duration::from_secs(2)),
        "annotation event came late"
    );

    // Accept 4: the three exports of the command.
    let export = |format: &str| {
        let r = web
            .get(&format!("/api/sessions/{uuid}/export?format={format}&cmd={seq}"))
            .unwrap();
        assert_eq!(r.status, 200, "{format}: {r:?}");
        r.body
    };
    let cast = export("cast");
    let header: Value = serde_json::from_str(cast.lines().next().unwrap()).unwrap();
    assert_eq!(header["version"], 3, "{header}");
    assert!(cast.contains("before-join-42"), "cast slice");
    assert!(!cast.contains("after-join-48"), "the slice ends with the command");
    for line in cast.lines().skip(1).filter(|l| !l.is_empty() && !l.starts_with('#')) {
        let ev: Value = serde_json::from_str(line).unwrap_or_else(|e| panic!("{e}: {line}"));
        assert!(ev.as_array().is_some_and(|a| a.len() == 3), "{line}");
    }
    let txt = export("txt");
    assert!(txt.contains("before-join-42"), "txt: {txt}");
    let html = export("html");
    assert!(html.contains("before-join-42"), "html");
    assert!(!html.to_lowercase().contains("<script"), "html has no script");
    assert!(html.contains("default-src 'none'"), "html carries its own CSP");

    // Its timeline block, and the annotation on it.
    let tl = web.get_json("/api/timeline").unwrap();
    let blocks: Vec<&Value> = tl["lanes"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|l| l["rows"].as_array().unwrap())
        .flat_map(|r| r["blocks"].as_array().unwrap())
        .filter(|b| b["session"] == uuid.as_str())
        .collect();
    assert!(
        blocks
            .iter()
            .any(|b| b["seq"] == seq && b["typed"] == "echo before-join-$((6*7))"),
        "timeline: {tl}"
    );
    assert!(
        tl["annotations"]
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a["uuid"] == note_uuid.as_str()),
        "timeline annotations: {}",
        tl["annotations"]
    );

    // Accept 1 and 5: a recorder crash ends the stream with offline, and the bus drops it.
    let entry = sb
        .registry()
        .into_iter()
        .find(|(_, v)| v["uuid"] == uuid.as_str())
        .expect("registry entry of the session")
        .1;
    let pid = entry["pid"].as_u64().unwrap() as i32;
    let killed_at = Instant::now();
    nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), nix::sys::signal::Signal::SIGKILL).unwrap();
    live.wait("offline", scaled(10.0), None, |d| d == r#"{"state":"offline"}"#);
    live.wait("done", scaled(5.0), Some("done"), |_| true);
    let (at, end) = bus.wait("session-end", scaled(10.0), Some("session-end"), |d| d.contains(&uuid));
    assert!(
        within(at, killed_at, Duration::from_secs(2)),
        "session-end came late: {end}"
    );
    drop((live, bus, watcher));
    serve.stop(Duration::from_secs(10)).unwrap();

    // Accept 3: the annotation survives a rebuild and a meta.db restore.
    sb.cinelog(&["index", "--rebuild"]).unwrap().expect_ok().unwrap();
    let backup = sb.work.join("meta.jsonl");
    sb.cinelog(&["meta", "export", backup.to_str().unwrap()])
        .unwrap()
        .expect_ok()
        .unwrap();
    let metas: Vec<_> = walk(&sb.state_dir())
        .into_iter()
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("meta.db"))
        })
        .collect();
    assert!(!metas.is_empty(), "meta.db under {}", sb.state_dir().display());
    for p in metas {
        std::fs::remove_file(p).unwrap();
    }
    sb.cinelog(&["meta", "import", backup.to_str().unwrap()])
        .unwrap()
        .expect_ok()
        .unwrap();
    let (serve, web) = start_web(&sb, "serve2", &token);
    let notes = web.get_json(&format!("/api/annotations?session={uuid}")).unwrap();
    let back = notes["annotations"]
        .as_array()
        .unwrap_or_else(|| panic!("annotation page: {notes}"))
        .iter()
        .find(|a| a["uuid"] == note_uuid.as_str())
        .cloned()
        .unwrap_or_else(|| panic!("the annotation is back: {notes}"));
    assert_eq!(back["cmd_seq"], seq, "{back}");
    assert_eq!(back["body"], "it note https://example.org/x", "{back}");
    assert_eq!(back["tags"], serde_json::json!(["it-m4"]), "{back}");
    serve.stop(Duration::from_secs(10)).unwrap();
}

fn walk(dir: &Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(dir) else { return out };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            out.extend(walk(&p));
        } else {
            out.push(p);
        }
    }
    out
}
