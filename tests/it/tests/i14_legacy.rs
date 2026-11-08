//! I14: legacy import (plan §11) through the real binary on synthetic legacy casts from
//! `tests/fixtures/legacy-gen` (old plugin output: plain, p10k-style with transient prompt,
//! wrapped, `\c`/`\n` log damage, banner-less periods, DST step, resize, no log, TERM=dumb,
//! and a long random cast). Asserts the alignment and confidences per command, the import
//! rows, the aggregate-only report, search over legacy commands, and zero writes to the
//! legacy files (content, times, inode, directory listing, inotify).

#[path = "../../fixtures/legacy-gen/legacy_gen.rs"]
mod legacy_gen;

use std::collections::BTreeSet;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use cinelog_it::index::IndexDb;
use cinelog_it::util::{now_us, set_times, sha256_file};
use cinelog_it::{Need, Sandbox, need};
use legacy_gen::Legacy;

const BANNER: &str = r"(Begin|End  ) \[\d{4}-\d\d-\d\d \d\d:\d\d:\d\d\]";

type Stamp = (u64, i64, i64, i64, i64, u64, String);

fn stamp(p: &Path) -> Stamp {
    let m = fs::symlink_metadata(p).unwrap();
    (
        m.len(),
        m.mtime(),
        m.mtime_nsec(),
        m.atime(),
        m.atime_nsec(),
        m.ino(),
        sha256_file(p).unwrap(),
    )
}

fn listing(dir: &Path) -> BTreeSet<PathBuf> {
    let mut out = BTreeSet::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in fs::read_dir(&d).unwrap().flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p.clone());
            }
            out.insert(p);
        }
    }
    out
}

/// inotify watch on a set of directories for anything but reads.
struct Watch(std::os::fd::OwnedFd);

impl Watch {
    fn new(dirs: &[&Path]) -> Self {
        use std::os::fd::FromRawFd;
        use std::os::unix::ffi::OsStrExt;
        // SAFETY: plain syscall; a negative result is checked below.
        let fd = unsafe { libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC) };
        assert!(fd >= 0, "inotify_init1: {}", std::io::Error::last_os_error());
        // SAFETY: `fd` is a fresh descriptor we own.
        let w = Watch(unsafe { std::os::fd::OwnedFd::from_raw_fd(fd) });
        let mask = libc::IN_MODIFY
            | libc::IN_ATTRIB
            | libc::IN_CLOSE_WRITE
            | libc::IN_MOVED_FROM
            | libc::IN_MOVED_TO
            | libc::IN_CREATE
            | libc::IN_DELETE
            | libc::IN_DELETE_SELF
            | libc::IN_MOVE_SELF;
        for d in dirs {
            let c = std::ffi::CString::new(d.as_os_str().as_bytes()).unwrap();
            // SAFETY: valid fd and C string.
            let wd = unsafe { libc::inotify_add_watch(fd, c.as_ptr(), mask) };
            assert!(
                wd >= 0,
                "inotify_add_watch {}: {}",
                d.display(),
                std::io::Error::last_os_error()
            );
        }
        w
    }

    /// Names of the files events were reported for so far.
    fn events(&self) -> Vec<String> {
        use std::os::fd::AsRawFd;
        let mut buf = vec![0u8; 64 * 1024];
        let mut out = Vec::new();
        loop {
            // SAFETY: the buffer is valid for its length.
            let n = unsafe { libc::read(self.0.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) };
            if n <= 0 {
                return out;
            }
            let mut off = 0;
            let n = n as usize;
            let head = std::mem::size_of::<libc::inotify_event>();
            while off + head <= n {
                // SAFETY: the kernel wrote a complete event header at `off`.
                let ev: libc::inotify_event = unsafe { std::ptr::read_unaligned(buf[off..].as_ptr().cast()) };
                let name = &buf[off + head..off + head + ev.len as usize];
                let name = String::from_utf8_lossy(name).trim_end_matches('\0').to_owned();
                out.push(format!("{name} (mask {:#x})", ev.mask));
                off += head + ev.len as usize;
            }
        }
    }
}

struct Fixture {
    sb: Sandbox,
    /// `(cast path, generated cast)`; some live in a subdirectory.
    casts: Vec<(PathBuf, String, Legacy)>,
    files: Vec<PathBuf>,
}

/// Writes every scenario plus a long cast, flat in `logs/` like the old plugin (one in a
/// subdirectory), with times 400 days old so that any access-time update would be visible.
fn fixture() -> Fixture {
    let sb = Sandbox::new().unwrap();
    let sub = sb.logs.join("old-host");
    fs::create_dir_all(&sub).unwrap();
    let mut scenarios = legacy_gen::corpus();
    scenarios.push(("long.cast", legacy_gen::long(42, 300)));
    let old = now_us() - 400 * 86_400_000_000;
    let mut casts = Vec::new();
    let mut files = Vec::new();
    for (name, l) in scenarios {
        let dir = if name == "resize.cast" { &sub } else { &sb.logs };
        let cast = l.write(dir, name).unwrap();
        let log = dir.join(format!("{name}.commands.log"));
        for p in std::iter::once(cast.clone()).chain(log.is_file().then_some(log)) {
            set_times(&p, old, old).unwrap();
            files.push(p);
        }
        casts.push((cast, name.to_owned(), l));
    }
    Fixture { sb, casts, files }
}

/// A command row of the index.
#[derive(Debug)]
struct Row {
    seq: i64,
    t_start: f64,
    t_end: Option<f64>,
    ts_start: f64,
    typed: String,
    approx: bool,
    confidence: Option<f64>,
    typed_trunc: bool,
    exit_cwd_expanded_null: bool,
}

fn commands(db: &IndexDb, uuid: &str) -> Vec<Row> {
    db.strings(
        "SELECT c.seq, c.t_start, c.t_end, c.ts_start, c.typed, c.approx, c.confidence, c.typed_trunc, \
         c.exit IS NULL AND c.cwd IS NULL AND c.expanded IS NULL \
         FROM commands c JOIN sessions s ON s.id = c.session_id WHERE s.uuid = ?1 ORDER BY c.seq",
        [uuid],
    )
    .unwrap()
    .into_iter()
    .map(|r| {
        let f = |i: usize| r[i].as_deref().map(|v| v.parse::<f64>().unwrap());
        Row {
            seq: r[0].as_deref().unwrap().parse().unwrap(),
            t_start: f(1).unwrap(),
            t_end: f(2),
            ts_start: f(3).unwrap(),
            typed: r[4].clone().unwrap(),
            approx: r[5].as_deref() == Some("1"),
            confidence: f(6),
            typed_trunc: r[7].as_deref() == Some("1"),
            exit_cwd_expanded_null: r[8].as_deref() == Some("1"),
        }
    })
    .collect()
}

fn output_of(db: &IndexDb, uuid: &str, seq: i64) -> String {
    db.strings(
        "SELECT o.text FROM outputs o JOIN commands c ON c.id = o.command_id JOIN sessions s ON s.id = o.session_id \
         WHERE s.uuid = ?1 AND c.seq = ?2 ORDER BY o.t, o.id",
        rusqlite::params![uuid, seq],
    )
    .unwrap()
    .into_iter()
    .filter_map(|r| r[0].clone())
    .collect::<Vec<_>>()
    .join("\n")
}

/// Session uuid, `legacy`, `hidden`, `attribution`, `started_at`, `state` of a cast.
fn session(db: &IndexDb, cast: &Path) -> (String, String, String, Option<String>, f64, String) {
    let r = db
        .strings(
            "SELECT s.uuid, s.legacy, s.hidden, s.attribution, s.started_at, s.state \
             FROM legacy_import l JOIN sessions s ON s.id = l.session_id WHERE l.path = ?1",
            [cast.to_string_lossy()],
        )
        .unwrap();
    assert_eq!(r.len(), 1, "{}: one session via its import row", cast.display());
    let r = &r[0];
    (
        r[0].clone().unwrap(),
        r[1].clone().unwrap(),
        r[2].clone().unwrap(),
        r[3].clone(),
        r[4].as_deref().unwrap().parse().unwrap(),
        r[5].clone().unwrap(),
    )
}

/// `status, banners, log_lines, aligned, mean_conf, commands_log` of the import row.
fn import_row(db: &IndexDb, cast: &Path) -> Vec<Option<String>> {
    db.strings(
        "SELECT status, banners, log_lines, aligned, mean_conf, commands_log, error FROM legacy_import WHERE path = ?1",
        [cast.to_string_lossy()],
    )
    .unwrap()
    .pop()
    .unwrap_or_else(|| panic!("{}: no legacy_import row", cast.display()))
}

/// Every banner is one command at the banner's time with the log text (confidence ≥ 0.8) or
/// the reconstructed text, and its output; banner lines never reach the output text.
fn check_scenario(db: &IndexDb, name: &str, uuid: &str, started_at: f64, l: &Legacy) {
    let rows = commands(db, uuid);
    let banners: Vec<_> = l.expect.iter().filter(|e| e.banner).collect();
    assert_eq!(rows.len(), banners.len(), "{name}: one command per banner: {rows:?}");
    let mut prev = 0.0;
    for (row, e) in rows.iter().zip(&banners) {
        assert!(row.t_start >= prev, "{name}: seq order is time order");
        prev = row.t_start;
        assert!(row.approx && row.exit_cwd_expanded_null, "{name}: {row:?}");
        assert!(
            (row.t_start - e.t).abs() < 0.05,
            "{name}: {:?} at {} vs {}",
            e.typed,
            row.t_start,
            e.t
        );
        assert!(
            (row.ts_start - (started_at + row.t_start)).abs() < 1e-6,
            "{name}: ts_start"
        );
        assert!(row.t_end.is_some_and(|t| t >= row.t_start), "{name}: {row:?}");
        match &e.logged {
            Some(log) => {
                assert_eq!(&row.typed, log, "{name}");
                let conf = row.confidence.unwrap_or_default();
                assert!(conf >= 0.8, "{name}: {:?} confidence {conf}", e.typed);
                assert_eq!(row.typed_trunc, e.truncated, "{name}: {:?}", e.typed);
            }
            None => {
                assert_eq!(row.confidence, None, "{name}: {:?}", e.typed);
                assert_eq!(row.typed, e.typed, "{name}: reconstructed");
            }
        }
        let out = output_of(db, uuid, row.seq);
        for line in &e.output {
            assert!(
                out.contains(line.as_str()),
                "{name}: output {line:?} of {:?} in {out:?}",
                e.typed
            );
        }
    }
}

#[test]
fn i14_legacy_import_aligns_commands_and_never_writes() {
    need!(Need::Index);
    let fx = fixture();
    let sb = &fx.sb;
    let before: Vec<Stamp> = fx.files.iter().map(|p| stamp(p)).collect();
    let listing_before = listing(&sb.logs);
    let watch = Watch::new(&[&sb.logs, &sb.logs.join("old-host")]);

    let out = sb.cinelog(&["index", "--once"]).unwrap().expect_ok().unwrap();
    let n = fx.casts.len();
    assert!(out.stdout.contains(&format!("indexed {n} of {n} casts")), "{out}");

    let db = IndexDb::open(&sb.index_db()).unwrap();
    let (mut logged, mut good) = (0usize, 0usize);
    for (cast, name, l) in &fx.casts {
        let row = import_row(&db, cast);
        let status = if name == "dumb.cast" { "hidden" } else { "imported" };
        assert_eq!(row[0].as_deref(), Some(status), "{name}: {row:?}");
        assert_eq!(row[5].is_some(), name != "nolog.cast", "{name}: commands_log {row:?}");
        assert_eq!(row[6], None, "{name}: no error");
        let (uuid, legacy, hidden, attribution, started_at, state) = session(&db, cast);
        assert_eq!(legacy, "1", "{name}");
        assert_eq!(hidden == "1", name == "dumb.cast", "{name}: hidden");
        assert_eq!(attribution.as_deref(), Some("legacy"), "{name}");
        assert_eq!(state, "ended", "{name}");
        assert!(started_at >= 1_698_541_140.0, "{name}: header timestamp {started_at}");
        if name == "nolog.cast" {
            let rows = commands(&db, &uuid);
            assert_eq!(rows.len(), 1, "{name}: {rows:?}");
            assert_eq!((rows[0].typed.as_str(), rows[0].confidence), ("ls", None));
            assert_eq!(row[4], None, "{name}: no mean confidence");
            continue;
        }
        let num = |i: usize| row[i].as_deref().unwrap_or("0").parse::<usize>().unwrap();
        let aligned = l.expect.iter().filter(|e| e.banner && e.logged.is_some()).count();
        assert_eq!(num(1), l.banners(), "{name}: banners");
        assert_eq!(num(2), l.logged(), "{name}: log lines");
        assert_eq!(num(3), aligned, "{name}: aligned");
        assert_eq!(row[4].is_some(), aligned > 0, "{name}: mean confidence {row:?}");
        check_scenario(&db, name, &uuid, started_at, l);
        logged += aligned;
        good += commands(&db, &uuid)
            .iter()
            .filter(|r| r.confidence.is_some_and(|c| c >= 0.8))
            .count();
    }
    // §11 acceptance: at least 95 % of the (parsable, bannered) log lines aligned at ≥ 0.8.
    assert!(logged > 300 && good * 100 >= logged * 95, "{good} of {logged} aligned");

    let banner = regex::Regex::new(BANNER).unwrap();
    let leaked = db
        .strings("SELECT text FROM outputs", [])
        .unwrap()
        .into_iter()
        .filter_map(|r| r[0].clone())
        .filter(|t| banner.is_match(t))
        .count();
    assert_eq!(leaked, 0, "banner lines in output chunks");
    drop(db);

    // Legacy commands are searchable, flagged approximate with their confidence.
    let hits = sb.search_json(&["git checkout"]).unwrap();
    let h = hits
        .iter()
        .find(|h| h["kind"] == "command" && h["typed"] == "git checkout feature/x")
        .unwrap_or_else(|| panic!("{hits:?}"));
    assert_eq!(h["approx"], true);
    assert!(h["confidence"].as_f64().is_some_and(|c| c >= 0.8), "{h}");
    let out_hits = sb.search_json(&["--out", "Switched to branch"]).unwrap();
    assert!(
        out_hits.iter().any(|h| h["kind"] == "output" && h["approx"] == true),
        "{out_hits:?}"
    );

    // The report: one line per file with numbers only, never command text.
    let report = sb.cinelog(&["import", "--report"]).unwrap().expect_ok().unwrap();
    for (_, name, l) in &fx.casts {
        assert!(report.stdout.contains(name.as_str()), "{name} in the report:\n{report}");
        for e in l.expect.iter().filter(|e| e.typed.len() >= 8) {
            assert!(
                !report.stdout.contains(&e.typed),
                "the report leaks {:?}:\n{report}",
                e.typed
            );
        }
    }

    // A second run finds every file unchanged and imports nothing again.
    let db_before = IndexDb::open(&sb.index_db()).unwrap().dump(&[]).unwrap();
    let again = sb.cinelog(&["index", "--once"]).unwrap().expect_ok().unwrap();
    assert!(again.stdout.contains(&format!("({n} unchanged, 0 failed)")), "{again}");
    let db_after = IndexDb::open(&sb.index_db()).unwrap().dump(&[]).unwrap();
    if let Some(d) = db_after.diff(&db_before, 5) {
        panic!("re-import changed the index:\n{d}");
    }

    // Zero writes: content, times (atime too: O_NOATIME), inodes, no new or removed files.
    let after: Vec<Stamp> = fx.files.iter().map(|p| stamp(p)).collect();
    for ((p, b), a) in fx.files.iter().zip(&before).zip(&after) {
        assert_eq!(a, b, "{} was touched", p.display());
    }
    assert_eq!(
        listing(&sb.logs),
        listing_before,
        "files appeared or vanished next to legacy casts"
    );
    assert_eq!(
        watch.events(),
        Vec::<String>::new(),
        "inotify saw writes in the legacy dirs"
    );
    let probe = sb.logs.join("old-host/probe");
    fs::write(&probe, b"x").unwrap();
    assert!(!watch.events().is_empty(), "the inotify watch itself works");
    fs::remove_file(probe).unwrap();
}

/// `index --rebuild` keeps the legacy session uuids (they derive from name, size and the
/// first MiB), so annotations keyed by uuid survive it.
#[test]
fn i14_legacy_uuids_survive_a_rebuild() {
    need!(Need::Index);
    let fx = fixture();
    let sb = &fx.sb;
    sb.cinelog(&["index", "--once"]).unwrap().expect_ok().unwrap();
    let uuids = |sb: &Sandbox| -> Vec<String> {
        let db = IndexDb::open(&sb.index_db()).unwrap();
        fx.casts.iter().map(|(c, _, _)| session(&db, c).0).collect()
    };
    let first = uuids(sb);
    let dump = IndexDb::open(&sb.index_db()).unwrap().dump(&[]).unwrap();
    let before: Vec<Stamp> = fx.files.iter().map(|p| stamp(p)).collect();
    sb.cinelog(&["index", "--rebuild"]).unwrap().expect_ok().unwrap();
    assert_eq!(uuids(sb), first);
    for (p, b) in fx.files.iter().zip(&before) {
        assert_eq!(&stamp(p), b, "the rebuild touched {}", p.display());
    }
    let rebuilt = IndexDb::open(&sb.index_db()).unwrap().dump(&[]).unwrap();
    if let Some(d) = rebuilt.diff(&dump, 5) {
        panic!("rebuilt legacy index differs:\n{d}");
    }
}
