//! I22 (M3 accept 1 and 4): `cinelog serve` with its web server in a sandbox. A shell recorded
//! while it runs gets the session URL: `cinelog link` prints it, the window title shows it
//! (`shell.title_link`), and it opens that session. The request matrix over HTTP/1.1 and
//! HTTP/2: no cookie 401, bad Host 400, Origin of another port 403, a cookie replayed with
//! another User-Agent 401 (and its session ended).

use std::path::Path;
use std::process::Command;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use cinelog_it::{Need, Sandbox, SandboxOptions, Serve, need, scaled, wait_until};
use serde_json::Value;

struct Web {
    base: String,
    port: u16,
}

#[derive(Debug)]
struct Reply {
    status: u16,
    headers: String,
    body: String,
}

impl Web {
    /// `curl -k` over `http` (`--http1.1` or `--http2`) with extra arguments.
    fn curl(&self, http: &str, path: &str, args: &[&str]) -> Result<Reply> {
        let out = Command::new("curl")
            .args(["-sk", http, "-D", "/dev/stderr", "-o", "-", "-w", "\n%{http_code}"])
            .args(args)
            .arg(format!("{}{path}", self.base))
            .output()
            .context("curl")?;
        let stdout = String::from_utf8_lossy(&out.stdout);
        let (body, code) = stdout.rsplit_once('\n').context("curl wrote no status")?;
        let status = code.trim().parse().with_context(|| format!("status {code:?}"))?;
        Ok(Reply {
            status,
            headers: String::from_utf8_lossy(&out.stderr).into_owned(),
            body: body.to_owned(),
        })
    }

    /// Logs in with `token` as User-Agent `ua`; returns the `Cookie` header value.
    fn login(&self, http: &str, token: &str, ua: &str) -> Result<String> {
        let body = serde_json::json!({ "token": token }).to_string();
        // Writes need the server's own Origin, as a browser sends it.
        let origin = format!("Origin: {}", self.base);
        let r = self.curl(
            http,
            "/api/login",
            &[
                "-A",
                ua,
                "-H",
                &origin,
                "-H",
                "Content-Type: application/json",
                "--data",
                &body,
            ],
        )?;
        if r.status != 204 {
            bail!("login: {r:?}");
        }
        let cookie = r
            .headers
            .lines()
            .find_map(|l| {
                l.strip_prefix("set-cookie: ")
                    .or_else(|| l.strip_prefix("Set-Cookie: "))
            })
            .context("no Set-Cookie")?;
        Ok(cookie.split(';').next().unwrap_or_default().to_owned())
    }
}

fn server_url(run: &Path) -> Option<String> {
    let info: Value = serde_json::from_slice(&std::fs::read(run.join("cinelog/server.json")).ok()?).ok()?;
    info["url"].as_str().map(str::to_owned)
}

#[test]
fn i22_link_title_and_the_request_matrix() {
    need!(
        Need::Zsh,
        Need::Tmux,
        Need::Plugin,
        Need::Recorder,
        Need::Index,
        Need::Curl
    );
    let sb = Sandbox::with(SandboxOptions {
        shell: vec![("title".into(), "true".into()), ("title_link".into(), "true".into())],
        ..SandboxOptions::default()
    })
    .unwrap();
    let new = sb.cinelog(&["token", "new"]).unwrap().expect_ok().unwrap();
    let token = new
        .stdout
        .lines()
        .find_map(|l| l.split("(shown once): ").nth(1))
        .expect("token new prints the token")
        .trim()
        .to_owned();
    let serve = Serve::start_web(&sb, "serve", &[]).unwrap();
    let url = wait_until("server.json", scaled(20.0), || server_url(&sb.run))
        .unwrap_or_else(|e| panic!("{e}\n{}", serve.log_text()));
    let port: u16 = url.rsplit(':').next().unwrap().trim_end_matches('/').parse().unwrap();
    let web = Web {
        base: format!("https://127.0.0.1:{port}"),
        port,
    };

    // A shell started while the server runs: link, title and the URL that opens it.
    let t = sb.tmux().unwrap();
    let r = sb.start_recorded(&t, "web").unwrap();
    let uuid = r.uuid().unwrap();
    let bin = sb.bin.as_ref().unwrap().display().to_string();
    r.run(&format!("{bin} link")).unwrap();
    let line = r.pane.wait_text("url https://", scaled(5.0)).unwrap();
    let link = line
        .lines()
        .find_map(|l| l.trim().strip_prefix("url "))
        .expect("link prints the URL")
        .to_owned();
    assert_eq!(link, format!("{}/s/{uuid}", url.trim_end_matches('/')), "{line}");
    let title = wait_until("the session link in the title", scaled(5.0), || {
        let title = r.pane.display("#{pane_title}").ok()?;
        title.contains(&link).then_some(title)
    })
    .unwrap();
    assert!(title.contains(&format!("[{link}]")), "{title}");

    let word = format!("webneedle{}", std::process::id());
    r.run(&format!("echo {word}")).unwrap();
    for http in ["--http1.1", "--http2"] {
        // No cookie, bad Host, Origin of another port.
        assert_eq!(web.curl(http, "/api/me", &[]).unwrap().status, 401, "{http}");
        let host = web.curl(http, "/api/me", &["-H", "Host: evil.example"]).unwrap();
        assert_eq!(host.status, 400, "{http}: {host:?}");
        let origin = format!("Origin: https://127.0.0.1:{}", web.port + 1);
        let body = serde_json::json!({ "token": token }).to_string();
        let cross = web
            .curl(
                http,
                "/api/login",
                &["-H", &origin, "-H", "Content-Type: application/json", "--data", &body],
            )
            .unwrap();
        assert_eq!(cross.status, 403, "{http}: {cross:?}");

        let cookie = web.login(http, &token, "it-agent-a").unwrap();
        let me = web
            .curl(
                http,
                "/api/me",
                &["-A", "it-agent-a", "-H", &format!("Cookie: {cookie}")],
            )
            .unwrap();
        assert_eq!(me.status, 200, "{http}: {me:?}");
        // The link opens the session page, whose data the API serves.
        let page = web
            .curl(
                http,
                &format!("/s/{uuid}"),
                &["-A", "it-agent-a", "-H", &format!("Cookie: {cookie}")],
            )
            .unwrap();
        assert_eq!(page.status, 200, "{http}");
        assert!(
            page.body.contains("<div id=\"app\"") || page.body.contains("<script"),
            "{http}: the SPA"
        );
        // The live session reaches the index within a few seconds.
        let detail = wait_until("the live session over the API", scaled(10.0), || {
            let r = web
                .curl(
                    http,
                    &format!("/api/sessions/{uuid}"),
                    &["-A", "it-agent-a", "-H", &format!("Cookie: {cookie}")],
                )
                .ok()?;
            (r.status == 200)
                .then(|| serde_json::from_str::<Value>(&r.body).ok())
                .flatten()
        })
        .unwrap();
        assert_eq!(detail["session"]["state"], "live", "{http}: {detail}");
        let found = wait_until("the typed command over the API", scaled(10.0), || {
            let r = web
                .curl(
                    http,
                    &format!("/api/search?q={word}"),
                    &["-A", "it-agent-a", "-H", &format!("Cookie: {cookie}")],
                )
                .ok()?;
            let v: Value = serde_json::from_str(&r.body).ok()?;
            v["hits"]
                .as_array()?
                .iter()
                .find(|h| h["session"] == uuid.as_str())
                .cloned()
        })
        .unwrap();
        assert_eq!(found["typed"], format!("echo {word}"), "{found}");

        // The same cookie from another User-Agent: 401, and the session is gone.
        let replay = web
            .curl(
                http,
                "/api/me",
                &["-A", "it-agent-b", "-H", &format!("Cookie: {cookie}")],
            )
            .unwrap();
        assert_eq!(replay.status, 401, "{http}");
        let again = web
            .curl(
                http,
                "/api/me",
                &["-A", "it-agent-a", "-H", &format!("Cookie: {cookie}")],
            )
            .unwrap();
        assert_eq!(again.status, 401, "{http}: the replay ended the session");
    }
    serve.stop(Duration::from_secs(10)).unwrap();
    assert!(server_url(&sb.run).is_none(), "server.json is removed on exit");
}
