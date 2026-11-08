//! I25 (plan §10.3, version skew): after a rollback the older binary finds index.db and
//! meta.db at a newer `user_version`. `cinelog search` and `status` read them and say so;
//! `cinelog serve` serves the web UI read-only (logins in memory) and indexes nothing, so the
//! databases stay exactly as the newer binary left them.

use std::path::Path;
use std::process::Command;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use cinelog_it::synth::SynthCast;
use cinelog_it::util::{now_us, sha256_file};
use cinelog_it::{Need, Sandbox, Serve, need, scaled, wait_until};
use serde_json::Value;

const NEWER: u32 = 7;

fn curl(base: &str, path: &str, args: &[&str]) -> Result<(u16, String, String)> {
    let out = Command::new("curl")
        .args(["-sk", "-D", "/dev/stderr", "-o", "-", "-w", "\n%{http_code}"])
        .args(args)
        .arg(format!("{base}{path}"))
        .output()
        .context("curl")?;
    let stdout = String::from_utf8_lossy(&out.stdout);
    let (body, code) = stdout.rsplit_once('\n').context("curl wrote no status")?;
    Ok((
        code.trim().parse()?,
        String::from_utf8_lossy(&out.stderr).into_owned(),
        body.to_owned(),
    ))
}

fn login(base: &str, token: &str) -> Result<String> {
    let body = serde_json::json!({ "token": token }).to_string();
    let origin = format!("Origin: {base}");
    let (status, headers, _) = curl(
        base,
        "/api/login",
        &["-H", &origin, "-H", "Content-Type: application/json", "--data", &body],
    )?;
    if status != 204 {
        bail!("login: {status}");
    }
    let cookie = headers
        .lines()
        .find_map(|l| {
            l.strip_prefix("set-cookie: ")
                .or_else(|| l.strip_prefix("Set-Cookie: "))
        })
        .context("no Set-Cookie")?;
    Ok(cookie.split(';').next().unwrap_or_default().to_owned())
}

fn set_version(db: &Path, v: u32) {
    rusqlite::Connection::open(db)
        .unwrap()
        .pragma_update(None, "user_version", v)
        .unwrap();
}

fn version(db: &Path) -> u32 {
    rusqlite::Connection::open(db)
        .unwrap()
        .pragma_query_value(None, "user_version", |r| r.get(0))
        .unwrap()
}

#[test]
fn i25_a_newer_schema_is_served_read_only() {
    need!(Need::Index, Need::Curl);
    let sb = Sandbox::new().unwrap();
    let mut cast = SynthCast::new(now_us() - 3_600_000_000, 100, 30);
    cast.command("echo skewneedle", "skewneedle\n", 0, 10_000);
    cast.write(&sb.logs, 0).unwrap();
    sb.cinelog(&["index", "--once"]).unwrap().expect_ok().unwrap();
    let new = sb.cinelog(&["token", "new"]).unwrap().expect_ok().unwrap();
    let token = new
        .stdout
        .lines()
        .find_map(|l| l.split("(shown once): ").nth(1))
        .expect("token new prints the token")
        .trim()
        .to_owned();

    // The newer binary has migrated both databases.
    let index = sb.index_db();
    let meta = index.with_file_name("meta.db");
    let export = sb.work.join("meta.jsonl");
    sb.cinelog(&["meta", "export", &export.to_string_lossy()])
        .unwrap()
        .expect_ok()
        .unwrap();
    assert!(meta.is_file());
    set_version(&index, NEWER);
    set_version(&meta, NEWER);
    let (index_sha, meta_sha) = (sha256_file(&index).unwrap(), sha256_file(&meta).unwrap());

    let hits = sb.search_json(&["skewneedle"]).unwrap();
    assert_eq!(hits.len(), 1, "{hits:?}");
    let status = sb.cinelog(&["status"]).unwrap().expect_ok().unwrap();
    assert!(
        status.stdout.contains(&format!("schema {NEWER}")) && status.stdout.contains("read-only, not indexing"),
        "{status}"
    );

    let serve = Serve::start_web(&sb, "serve", &[]).unwrap();
    // Not started from a terminal: serve logs to `<state>/serve.log`.
    let serve_log = || std::fs::read_to_string(sb.state_dir().join("serve.log")).unwrap_or_default();
    wait_until("the read-only notice", scaled(20.0), || {
        serve_log().contains("serving read-only").then_some(())
    })
    .unwrap_or_else(|e| panic!("{e}\n{}{}", serve.log_text(), serve_log()));
    let info: Value = wait_until("server.json", scaled(20.0), || {
        serde_json::from_slice(&std::fs::read(sb.run.join("cinelog/server.json")).ok()?).ok()
    })
    .unwrap_or_else(|e| panic!("{e}\n{}{}", serve.log_text(), serve_log()));
    let port = info["url"]
        .as_str()
        .unwrap()
        .trim_end_matches('/')
        .rsplit(':')
        .next()
        .unwrap()
        .to_owned();
    let base = format!("https://127.0.0.1:{port}");

    let cookie = login(&base, &token).unwrap_or_else(|e| panic!("{e}\n{}{}", serve.log_text(), serve_log()));
    let cookie = format!("Cookie: {cookie}");
    let (code, _, body) = curl(&base, "/api/status", &["-H", &cookie]).unwrap();
    assert_eq!(code, 200, "{body}");
    let st: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(st["schema_version"], NEWER);
    assert!(
        st["last_error"].as_str().unwrap_or_default().contains("read-only"),
        "{st}"
    );
    let (code, _, body) = curl(&base, "/api/search?q=skewneedle", &["-H", &cookie]).unwrap();
    assert_eq!(code, 200, "{body}");
    assert!(body.contains("skewneedle"), "{body}");

    // A cast written now is not indexed, and nothing is written to either database.
    let mut later = SynthCast::new(now_us() - 60_000_000, 100, 30);
    later.command("echo laterneedle", "laterneedle\n", 0, 10_000);
    later.write(&sb.logs, 0).unwrap();
    std::thread::sleep(scaled(3.0).min(Duration::from_secs(10)));
    assert!(sb.search_json(&["laterneedle"]).unwrap().is_empty());
    assert!(serve.stop(scaled(15.0)).unwrap().success());
    assert_eq!((version(&index), version(&meta)), (NEWER, NEWER));
    assert_eq!(sha256_file(&index).unwrap(), index_sha, "index.db untouched");
    assert_eq!(sha256_file(&meta).unwrap(), meta_sha, "meta.db untouched");
}
