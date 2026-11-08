//! `cinelog status` (§6.4 "Status"), `cinelog import --report` (§11.8) and the summary of
//! `cinelog index`. Everything printed here is aggregate numbers, paths and states, never
//! cast content.

use std::io::Write;

use cinelog_core::server::ServerInfo;
use cinelog_index::IndexError;
use cinelog_index::model::{
    DbPaths, IndexStatus, LegacyImportRow, LegacyProgress, LegacyStatus, LiveLag, OnceSummary, SessionCounts, info_keys,
};
use serde::Serialize;

use super::text::{self, BOLD, DIM, RED, YELLOW};
use super::{Env, Store, code};
use crate::cli::{ImportArgs, StatusArgs};

/// Process state of this scope's indexer and server, probed by the caller.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Daemon {
    /// `cinelog serve` runs (it holds `serve.lock` for its lifetime).
    pub serve_lock_held: bool,
    /// Some process holds `index.db.writer.lock` (serve, or a one-shot `cinelog index`).
    pub writer_lock_held: bool,
    /// The web server (`server.json`), from M3.
    pub server: Option<ServerInfo>,
}

// ------------------------------------------------------------------ index

/// Prints the outcome of `cinelog index`: 0 all casts indexed, 1 some failed, 2 error.
pub fn index_summary(
    result: Result<OnceSummary, IndexError>,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> anyhow::Result<u8> {
    let s = match result {
        Ok(s) => s,
        Err(IndexError::Locked(path)) => {
            writeln!(
                err,
                "cinelog: index: another process holds the writer lock {}: a running `cinelog serve` indexes new casts by itself (stop it to run --rebuild)",
                path.display()
            )?;
            return Ok(code::ERROR);
        }
        Err(e) => return Err(anyhow::Error::new(e)),
    };
    writeln!(
        out,
        "indexed {} of {} casts ({} unchanged, {} failed): {} commands, {} output chunks, {} in {}",
        s.indexed,
        s.casts_seen,
        s.unchanged,
        s.failed,
        s.commands,
        s.chunks,
        text::fmt_bytes(s.bytes),
        text::fmt_duration(s.elapsed_secs)
    )?;
    for e in &s.errors {
        writeln!(err, "cinelog: index: {}", text::one_line(e, 1000))?;
    }
    Ok(if s.failed == 0 { code::OK } else { code::NONE })
}

// ------------------------------------------------------------------ status

#[derive(Serialize)]
struct StatusJson<'a> {
    index_path: &'a std::path::Path,
    index_exists: bool,
    /// `None` without an index, or with `index_error`.
    index: Option<&'a IndexStatus>,
    index_error: Option<String>,
    daemon: &'a Daemon,
}

pub fn status(
    args: &StatusArgs,
    store: &dyn Store,
    db: &DbPaths,
    daemon: &Daemon,
    env: &Env,
    out: &mut dyn Write,
) -> anyhow::Result<u8> {
    let exists = store.has_index();
    let read = exists.then(|| store.reader().and_then(|r| Ok(r.status()?)));
    let (index, index_error) = match &read {
        Some(Ok(s)) => (Some(s), None),
        Some(Err(e)) => (None, Some(format!("{e:#}"))),
        None => (None, None),
    };
    if args.json {
        let v = StatusJson {
            index_path: &db.index,
            index_exists: exists,
            index,
            index_error: index_error.clone(),
            daemon,
        };
        writeln!(out, "{}", serde_json::to_string(&v)?)?;
    } else {
        out.write_all(render_status(db, exists, index, index_error.as_deref(), daemon, env).as_bytes())?;
    }
    // PROTOCOL §6.1: status is a diagnostic and exits 0; a broken index is part of its report.
    Ok(code::OK)
}

/// Row label column.
fn row(s: &mut String, env: &Env, label: &str, value: &str) {
    use std::fmt::Write as _;
    let _ = writeln!(s, "{} {value}", env.style.paint(BOLD, &format!("{label:<8}")));
}

fn render_status(
    db: &DbPaths,
    exists: bool,
    index: Option<&IndexStatus>,
    index_error: Option<&str>,
    daemon: &Daemon,
    env: &Env,
) -> String {
    let st = env.style;
    let mut s = String::new();
    let path = db.index.display();
    match (exists, index, index_error) {
        (false, ..) => row(
            &mut s,
            env,
            "index",
            &format!("none yet at {path} (start `cinelog serve` or run `cinelog index --once`)"),
        ),
        (true, _, Some(e)) => row(
            &mut s,
            env,
            "index",
            &format!("{path}: {}", st.paint(RED, &text::one_line(e, 500))),
        ),
        (true, Some(i), None) => {
            let built = i
                .info
                .get(info_keys::BUILT_VERSION)
                .map_or_else(String::new, |v| format!(", built by {v}"));
            row(
                &mut s,
                env,
                "index",
                &format!(
                    "{path} · {} · schema {}{built}",
                    text::fmt_bytes(i.index_bytes),
                    i.schema_version
                ),
            );
        }
        (true, None, None) => unreachable!("an existing index yields a status or an error"),
    }
    row(&mut s, env, "indexer", &indexer_line(daemon, index, env));
    row(&mut s, env, "server", &server_line(daemon));
    let Some(i) = index else {
        return s;
    };
    if let Some(started) = info_ts(i, info_keys::REBUILD_STARTED) {
        let since = text::fmt_ts(started, &env.tz);
        row(
            &mut s,
            env,
            "rebuild",
            &st.paint(YELLOW, &format!("in progress since {since}")),
        );
    }
    row(&mut s, env, "sessions", &sessions_line(&i.sessions));
    row(
        &mut s,
        env,
        "rows",
        &format!("{} commands, {} output chunks", i.commands, i.outputs),
    );
    row(
        &mut s,
        env,
        "backlog",
        &if i.backlog_bytes == 0 {
            "none".to_owned()
        } else {
            format!("{} not indexed yet", text::fmt_bytes(i.backlog_bytes))
        },
    );
    for lag in &i.live {
        row(&mut s, env, "live", &live_line(lag, env));
    }
    if i.legacy.files > 0 {
        row(&mut s, env, "legacy", &legacy_line(&i.legacy));
    }
    if let Some(e) = &i.last_error {
        row(&mut s, env, "error", &st.paint(RED, &text::one_line(e, 500)));
    }
    s
}

fn info_ts(i: &IndexStatus, key: &str) -> Option<f64> {
    i.info.get(key)?.parse().ok().filter(|v: &f64| v.is_finite())
}

fn indexer_line(d: &Daemon, index: Option<&IndexStatus>, env: &Env) -> String {
    let mut line = match (d.serve_lock_held, d.writer_lock_held) {
        (true, true) => "running (cinelog serve)".to_owned(),
        (true, false) => "cinelog serve runs, but nobody holds the writer lock".to_owned(),
        (false, true) => "a one-shot `cinelog index` holds the writer lock".to_owned(),
        (false, false) => "not running (start it with `cinelog serve`)".to_owned(),
    };
    if d.writer_lock_held
        && let Some(pid) = index.and_then(|i| i.info.get(info_keys::WRITER_PID))
    {
        line.push_str(&format!(", pid {}", text::one_line(pid, 20)));
    }
    if let Some(beat) = index.and_then(|i| info_ts(i, info_keys::WRITER_HEARTBEAT)) {
        let ago = env.now.as_microsecond() as f64 / 1e6 - beat;
        line.push_str(&format!(", last commit {} ago", text::fmt_duration(ago)));
    }
    line
}

fn server_line(d: &Daemon) -> String {
    match &d.server {
        Some(s) => {
            let fp = if s.fingerprint.is_empty() {
                String::new()
            } else {
                format!(" · cert {}", text::one_line(&s.fingerprint, 128))
            };
            format!("{} (pid {}){fp}", text::one_line(&s.url, 256), s.pid)
        }
        None => "no web server (`cinelog search` prints cast paths instead of links)".to_owned(),
    }
}

fn sessions_line(c: &SessionCounts) -> String {
    let mut parts = vec![format!("{} live", c.live), format!("{} ended", c.ended)];
    for (n, what) in [(c.crashed, "crashed"), (c.degraded, "degraded"), (c.error, "error")] {
        if n > 0 {
            parts.push(format!("{n} {what}"));
        }
    }
    let mut line = parts.join(", ");
    if c.legacy > 0 || c.hidden > 0 {
        line.push_str(&format!(" ({} legacy, {} hidden)", c.legacy, c.hidden));
    }
    line
}

fn live_line(lag: &LiveLag, env: &Env) -> String {
    let behind = match lag.file_bytes {
        Some(size) => format!("{} behind", text::fmt_bytes(size.saturating_sub(lag.indexed_bytes))),
        None => "cast missing".to_owned(),
    };
    let updated = match lag.updated_at {
        Some(at) => {
            let ago = env.now.as_microsecond() as f64 / 1e6 - at;
            format!("indexed {} ago", text::fmt_duration(ago))
        }
        None => "not indexed yet".to_owned(),
    };
    format!(
        "{} · {behind} · {updated} · {}",
        lag.uuid,
        env.style
            .paint(DIM, &text::one_line(&lag.cast_path.to_string_lossy(), 4096))
    )
}

fn legacy_line(l: &LegacyProgress) -> String {
    let done = l.imported + l.hidden + l.skipped + l.failed;
    format!(
        "{} files: {} imported, {} hidden, {} skipped, {} failed{} · {} banners, {} log lines, {}",
        l.files,
        l.imported,
        l.hidden,
        l.skipped,
        l.failed,
        if done < l.files { " (importing)" } else { "" },
        l.banners,
        l.log_lines,
        aligned_share(l.aligned, l.log_lines)
    )
}

fn aligned_share(aligned: u64, log_lines: u64) -> String {
    if log_lines == 0 {
        format!("{aligned} aligned")
    } else {
        format!("{aligned} aligned ({:.1}%)", aligned as f64 * 100.0 / log_lines as f64)
    }
}

// ------------------------------------------------------------------ import --report

#[derive(Debug, Default, PartialEq, Serialize)]
struct Totals {
    files: u64,
    imported: u64,
    hidden: u64,
    skipped: u64,
    failed: u64,
    banners: u64,
    log_lines: u64,
    aligned: u64,
    /// Mean of the per-file means, weighted by aligned commands.
    mean_conf: Option<f64>,
}

fn totals(rows: &[LegacyImportRow]) -> Totals {
    let mut t = Totals::default();
    let (mut conf_sum, mut conf_n) = (0.0, 0u64);
    for r in rows {
        t.files += 1;
        match r.status {
            LegacyStatus::Imported => t.imported += 1,
            LegacyStatus::Hidden => t.hidden += 1,
            LegacyStatus::Skipped => t.skipped += 1,
            LegacyStatus::Failed => t.failed += 1,
        }
        t.banners += r.banners;
        t.log_lines += r.log_lines;
        t.aligned += r.aligned;
        if let Some(c) = r.mean_conf.filter(|c| c.is_finite() && r.aligned > 0) {
            conf_sum += c * r.aligned as f64;
            conf_n += r.aligned;
        }
    }
    t.mean_conf = (conf_n > 0).then(|| conf_sum / conf_n as f64);
    t
}

pub fn import_report(args: &ImportArgs, store: &dyn Store, out: &mut dyn Write) -> anyhow::Result<u8> {
    let rows = store.reader()?.legacy_report()?;
    let total = totals(&rows);
    if args.json {
        #[derive(Serialize)]
        struct Report<'a> {
            files: &'a [LegacyImportRow],
            total: &'a Totals,
        }
        writeln!(
            out,
            "{}",
            serde_json::to_string(&Report {
                files: &rows,
                total: &total
            })?
        )?;
        return Ok(code::OK);
    }
    if rows.is_empty() {
        writeln!(out, "no legacy casts imported (yet)")?;
        return Ok(code::OK);
    }
    writeln!(
        out,
        "{:<8} {:>7} {:>7} {:>7} {:>5}  path",
        "status", "banners", "log", "aligned", "conf"
    )?;
    for r in &rows {
        let conf = r.mean_conf.map_or_else(|| "-".to_owned(), |c| format!("{c:.2}"));
        let mut line = format!(
            "{:<8} {:>7} {:>7} {:>7} {:>5}  {}",
            r.status.as_str(),
            r.banners,
            r.log_lines,
            r.aligned,
            conf,
            text::one_line(&r.path.to_string_lossy(), 4096)
        );
        if let Some(e) = &r.error {
            line.push_str(&format!("  ({})", text::one_line(e, 300)));
        }
        writeln!(out, "{line}")?;
    }
    let conf = total
        .mean_conf
        .map_or_else(String::new, |c| format!(", mean confidence {c:.2}"));
    writeln!(
        out,
        "total: {} files ({} imported, {} hidden, {} skipped, {} failed), {} banners, {} log lines, {}{conf}",
        total.files,
        total.imported,
        total.hidden,
        total.skipped,
        total.failed,
        total.banners,
        total.log_lines,
        aligned_share(total.aligned, total.log_lines)
    )?;
    Ok(code::OK)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use cinelog_index::model::{Checkpoint, FileStamp, IndexBatch, TranscriptState};
    use cinelog_index::{IndexWriter, Searcher};

    use super::super::testutil::*;
    use super::*;
    use crate::search::text::Style;

    fn legacy_row(
        name: &str,
        status: LegacyStatus,
        banners: u64,
        log: u64,
        aligned: u64,
        conf: Option<f64>,
    ) -> LegacyImportRow {
        LegacyImportRow {
            path: PathBuf::from(format!("/home/alice/logs/{name}")),
            file: FileStamp::default(),
            sha256: "00".repeat(32),
            commands_log: None,
            status,
            session_uuid: None,
            banners,
            log_lines: log,
            aligned,
            mean_conf: conf,
            error: (status == LegacyStatus::Failed).then(|| "unreadable header\x1b[2J".to_owned()),
        }
    }

    fn with_legacy(store: &MemStore) {
        let mut w = store.writer().unwrap().unwrap();
        w.record_legacy(&legacy_row("a.cast", LegacyStatus::Imported, 10, 8, 8, Some(0.9)))
            .unwrap();
        w.record_legacy(&legacy_row("b.cast", LegacyStatus::Imported, 4, 2, 2, Some(0.6)))
            .unwrap();
        w.record_legacy(&legacy_row("c.cast", LegacyStatus::Failed, 0, 0, 0, None))
            .unwrap();
    }

    fn db() -> DbPaths {
        DbPaths::in_dir("/home/alice/.local/state/cinelog/box")
    }

    fn run_status(store: &MemStore, daemon: &Daemon, json: bool) -> (u8, String) {
        let (c, out, _) = capture(|o, _| status(&StatusArgs { json }, store, &db(), daemon, &env(), o));
        (c, out)
    }

    #[test]
    fn index_summary_codes() {
        let ok = OnceSummary {
            casts_seen: 3,
            indexed: 2,
            unchanged: 1,
            bytes: 2048,
            commands: 5,
            chunks: 7,
            elapsed_secs: 1.25,
            ..OnceSummary::default()
        };
        let (c, out, err) = capture(|o, e| index_summary(Ok(ok.clone()), o, e));
        assert_eq!(c, 0);
        assert_eq!(
            out,
            "indexed 2 of 3 casts (1 unchanged, 0 failed): 5 commands, 7 output chunks, 2.0 KiB in 1.2 s\n"
        );
        assert!(err.is_empty());
        let failed = OnceSummary {
            failed: 1,
            errors: vec!["/x.cast: bad header".into()],
            ..ok
        };
        let (c, _, err) = capture(|o, e| index_summary(Ok(failed), o, e));
        assert_eq!(c, 1);
        assert_eq!(err, "cinelog: index: /x.cast: bad header\n");
        let (c, out, err) =
            capture(|o, e| index_summary(Err(IndexError::Locked("/s/index.db.writer.lock".into())), o, e));
        assert_eq!((c, out.as_str()), (2, ""));
        assert!(
            err.contains("cinelog serve") && err.contains("/s/index.db.writer.lock"),
            "{err}"
        );
        let (mut o, mut e) = (Vec::new(), Vec::new());
        let r = index_summary(Err(IndexError::Unavailable("the index engine")), &mut o, &mut e);
        assert!(r.unwrap_err().to_string().contains("not available"));
    }

    #[test]
    fn status_without_index() {
        let mut store = store();
        store.has_index = false;
        let (c, out) = run_status(&store, &Daemon::default(), false);
        assert_eq!(c, 0);
        assert_eq!(
            out,
            "index    none yet at /home/alice/.local/state/cinelog/box/index.db (start `cinelog serve` or run `cinelog index --once`)\n\
             indexer  not running (start it with `cinelog serve`)\n\
             server   no web server (`cinelog search` prints cast paths instead of links)\n"
        );
        let (_, out) = run_status(&store, &Daemon::default(), true);
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["index_exists"], false);
        assert!(v["index"].is_null());
    }

    #[test]
    fn status_reports_lag_backlog_legacy_and_errors() {
        let store = store();
        let dir = tempfile::tempdir().unwrap();
        let cast = dir.path().join("live.cast");
        std::fs::write(&cast, vec![b'x'; 3000]).unwrap();
        let mut sess = session(U2, "bob", T0 + 100.0);
        sess.cast_path = cast.clone();
        let mut b = IndexBatch::new(sess);
        b.checkpoint = Some(Checkpoint {
            part: 1,
            byte_offset: 1000,
            t: 1.0,
            transcript: TranscriptState::default(),
            open_stack: Vec::new(),
            pipeline_state: String::new(),
            file: FileStamp::default(),
        });
        let mut w = store.writer().unwrap().unwrap();
        w.apply(&b).unwrap();
        w.set_info(info_keys::LAST_ERROR, Some("disk \x1b[31mfull")).unwrap();
        w.set_info(info_keys::WRITER_PID, Some("4242")).unwrap();
        w.set_info(info_keys::WRITER_HEARTBEAT, Some(&(T0 + 3598.0).to_string()))
            .unwrap();
        with_legacy(&store);
        let daemon = Daemon {
            serve_lock_held: true,
            writer_lock_held: true,
            server: None,
        };
        let (c, out) = run_status(&store, &daemon, false);
        assert_eq!(c, 0, "{out}");
        let line = |label: &str| {
            out.lines()
                .find(|l| l.starts_with(&format!("{label:<8} ")))
                .unwrap_or_else(|| panic!("{label} in {out}"))
                .to_owned()
        };
        assert_eq!(
            line("indexer"),
            "indexer  running (cinelog serve), pid 4242, last commit 2.0 s ago"
        );
        assert_eq!(line("sessions"), "sessions 2 live, 0 ended");
        assert_eq!(line("rows"), "rows     4 commands, 3 output chunks");
        assert!(line("backlog").contains("not indexed yet"), "{out}");
        let live: Vec<&str> = out.lines().filter(|l| l.starts_with("live ")).collect();
        assert_eq!(live.len(), 2, "{out}");
        let bob = live.iter().find(|l| l.contains(U2)).unwrap();
        assert!(
            bob.contains("2.0 KiB behind") && bob.contains(&cast.display().to_string()),
            "{bob}"
        );
        assert_eq!(
            line("legacy"),
            "legacy   3 files: 2 imported, 0 hidden, 0 skipped, 1 failed · 14 banners, 10 log lines, 10 aligned (100.0%)"
        );
        assert_eq!(line("error"), "error    disk ^[[31mfull");
        assert!(!out.contains('\x1b'));

        let (_, json) = run_status(&store, &daemon, true);
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["daemon"]["serve_lock_held"], true);
        assert_eq!(v["index"]["legacy"]["files"], 3);
        assert_eq!(v["index"]["last_error"], "disk \x1b[31mfull");
    }

    #[test]
    fn status_colours_only_with_style() {
        let store = store();
        let mut env = env();
        env.style = Style::COLOR;
        let (_, out, _) =
            capture(|o, _| status(&StatusArgs { json: false }, &store, &db(), &Daemon::default(), &env, o));
        assert!(out.starts_with(&format!("{BOLD}index   {}", text::RESET)), "{out:?}");
    }

    #[test]
    fn status_of_an_unreadable_index_is_reported_with_exit_0() {
        struct Broken;
        impl Store for Broken {
            fn reader(&self) -> anyhow::Result<Box<dyn Searcher>> {
                anyhow::bail!("schema version 9 is newer than this cinelog supports (1)")
            }
            fn meta(&self) -> anyhow::Result<Box<dyn cinelog_index::MetaStore>> {
                unreachable!()
            }
            fn writer(&self) -> anyhow::Result<Option<Box<dyn IndexWriter>>> {
                Ok(None)
            }
            fn has_index(&self) -> bool {
                true
            }
        }
        let (c, out, _) = capture(|o, _| {
            status(
                &StatusArgs { json: false },
                &Broken,
                &db(),
                &Daemon::default(),
                &env(),
                o,
            )
        });
        assert_eq!(c, 0, "PROTOCOL §6.1: status exits 0");
        assert!(
            out.starts_with("index    /home/alice/.local/state/cinelog/box/index.db: schema version 9"),
            "{out}"
        );
        assert!(out.contains("\nserver   "), "the daemon state is still shown: {out}");
        let (c, out, _) = capture(|o, _| {
            status(
                &StatusArgs { json: true },
                &Broken,
                &db(),
                &Daemon::default(),
                &env(),
                o,
            )
        });
        assert_eq!(c, 0);
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert!(v["index"].is_null() && v["index_exists"] == true, "{v}");
        assert!(v["index_error"].as_str().unwrap().contains("schema version 9"), "{v}");
    }

    #[test]
    fn server_and_rebuild_lines() {
        let store = store();
        store
            .writer()
            .unwrap()
            .unwrap()
            .set_info(info_keys::REBUILD_STARTED, Some(&T0.to_string()))
            .unwrap();
        let daemon = Daemon {
            serve_lock_held: false,
            writer_lock_held: true,
            server: Some(ServerInfo {
                pid: 7,
                url: "https://box:11000".into(),
                port: 11000,
                fingerprint: "AB:CD".into(),
                version: "0.11.0".into(),
            }),
        };
        let (_, out) = run_status(&store, &daemon, false);
        assert!(
            out.contains("\nindexer  a one-shot `cinelog index` holds the writer lock\n"),
            "{out}"
        );
        assert!(
            out.contains("\nserver   https://box:11000 (pid 7) · cert AB:CD\n"),
            "{out}"
        );
        assert!(
            out.contains("\nrebuild  in progress since 2026-11-08 12:00:00\n"),
            "{out}"
        );
    }

    #[test]
    fn import_report_lists_files_and_totals() {
        let store = store();
        let (c, out, _) = capture(|o, _| {
            import_report(
                &ImportArgs {
                    report: true,
                    json: false,
                },
                &store,
                o,
            )
        });
        assert_eq!((c, out.as_str()), (0, "no legacy casts imported (yet)\n"));
        with_legacy(&store);
        let (c, out, _) = capture(|o, _| {
            import_report(
                &ImportArgs {
                    report: true,
                    json: false,
                },
                &store,
                o,
            )
        });
        assert_eq!(c, 0);
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines[0], "status   banners     log aligned  conf  path");
        assert_eq!(
            lines[1],
            "imported      10       8       8  0.90  /home/alice/logs/a.cast"
        );
        assert_eq!(
            lines[3],
            "failed         0       0       0     -  /home/alice/logs/c.cast  (unreadable header^[[2J)"
        );
        assert_eq!(
            lines[4],
            "total: 3 files (2 imported, 0 hidden, 0 skipped, 1 failed), 14 banners, 10 log lines, 10 aligned (100.0%), mean confidence 0.84"
        );
        let (_, json, _) = capture(|o, _| {
            import_report(
                &ImportArgs {
                    report: true,
                    json: true,
                },
                &store,
                o,
            )
        });
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["files"].as_array().unwrap().len(), 3);
        assert_eq!(v["total"]["aligned"], 10);
        assert_eq!(v["files"][0]["status"], "imported");
    }

    #[test]
    fn totals_weight_confidence_by_aligned_commands() {
        let rows = [
            legacy_row("a", LegacyStatus::Imported, 0, 0, 3, Some(1.0)),
            legacy_row("b", LegacyStatus::Hidden, 0, 0, 1, Some(0.0)),
            legacy_row("c", LegacyStatus::Skipped, 0, 0, 0, Some(0.5)),
        ];
        let t = totals(&rows);
        assert_eq!(t.mean_conf, Some(0.75));
        assert_eq!((t.imported, t.hidden, t.skipped), (1, 1, 1));
        assert_eq!(totals(&[]).mean_conf, None);
    }
}
