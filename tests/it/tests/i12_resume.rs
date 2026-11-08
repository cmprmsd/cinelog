//! I12: an indexer killed (SIGKILL) in the middle of a backfill and restarted ends with
//! exactly the database a one-shot run builds (canonical row hash over every derived table,
//! [`cinelog_it::index::Dump`]), with consistent full-text indexes. At least one kill in each
//! test lands after a checkpoint committed inside a cast, so the restart resumes from that
//! checkpoint's terminal dump, open stack and pipeline state rather than starting it over.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use cinelog_it::index::{Dump, IndexDb, fts_integrity, remove_index};
use cinelog_it::synth::{CorpusSpec, corpus};
use cinelog_it::util::now_us;
use cinelog_it::{CliOutput, Need, Sandbox, Serve, need, scaled, wait_until};

/// Large enough that a debug build needs a few seconds, with one cast (about 10 MB) ten
/// times the others, so it commits checkpoints inside the cast (every 4 MiB of input). The
/// many small casts after it keep the second kill of the serve run from racing the end of
/// the backfill on a fast machine.
fn spec() -> CorpusSpec {
    CorpusSpec {
        seed: 12,
        casts: 24,
        commands: 120,
        max_lines: 160,
        newest_start_us: now_us() - 86_400_000_000,
    }
}

struct Corpus {
    sb: Sandbox,
    files: Vec<PathBuf>,
    bytes: u64,
    db: PathBuf,
}

fn setup() -> Corpus {
    let sb = Sandbox::new().unwrap();
    let files: Vec<PathBuf> = corpus(&sb.logs, &spec()).unwrap().into_iter().map(|(p, _)| p).collect();
    // The backfill takes the newest cast first: make that the large one, so its checkpoints
    // are committed while the small casts are still being indexed.
    let newest = files.iter().map(|p| mtime_us(p)).max().unwrap();
    cinelog_it::util::set_times(&files[0], newest + 1_000_000, newest + 1_000_000).unwrap();
    let bytes = files.iter().map(|p| std::fs::metadata(p).unwrap().len()).sum();
    let db = sb.index_db();
    Corpus { sb, files, bytes, db }
}

fn mtime_us(p: &std::path::Path) -> u64 {
    use std::os::unix::fs::MetadataExt;
    let m = std::fs::metadata(p).unwrap();
    u64::try_from(m.mtime()).unwrap() * 1_000_000 + u64::try_from(m.mtime_nsec()).unwrap() / 1_000
}

/// Committed progress: bytes covered by checkpoints.
fn progress(db: &std::path::Path) -> u64 {
    IndexDb::open(db)
        .and_then(|d| d.int("SELECT coalesce(sum(byte_offset), 0) FROM index_state"))
        .map_or(0, |v| u64::try_from(v).unwrap_or(0))
}

/// Casts whose committed checkpoint lies strictly inside the file.
fn inside(db: &std::path::Path) -> i64 {
    IndexDb::open(db)
        .and_then(|d| d.int("SELECT count(*) FROM index_state WHERE byte_offset > 0 AND byte_offset < file_size"))
        .unwrap_or(0)
}

/// Every cast has a session whose checkpoint reached the end of the file.
fn complete(c: &Corpus) -> bool {
    let Ok(db) = IndexDb::open(&c.db) else { return false };
    let done = db
        .int(
            "SELECT count(*) FROM sessions s JOIN index_state i ON i.session_id = s.id \
             WHERE s.state = 'ended' AND i.byte_offset = i.file_size",
        )
        .unwrap_or(0);
    usize::try_from(done).is_ok_and(|d| d == c.files.len())
}

fn dump(c: &Corpus) -> Dump {
    IndexDb::open(&c.db).unwrap().dump(&[]).unwrap()
}

fn assert_same(what: &str, got: &Dump, want: &Dump) {
    if let Some(d) = got.diff(want, 5) {
        panic!("{what}: the database differs from the one-shot run\n{d}");
    }
    assert_eq!(got.hash(), want.hash());
}

/// One-shot `cinelog index --once` on a fresh index: the reference image.
fn reference(c: &Corpus) -> (Dump, Duration) {
    let t0 = Instant::now();
    let out = c.sb.cinelog(&["index", "--once"]).unwrap().expect_ok().unwrap();
    let took = t0.elapsed();
    assert!(
        out.stdout
            .contains(&format!("indexed {} of {} casts", c.files.len(), c.files.len())),
        "{out}"
    );
    let d = dump(c);
    assert_eq!(d.count("sessions"), c.files.len(), "{}", d.summary());
    assert_eq!(d.count("index_state"), c.files.len(), "{}", d.summary());
    fts_integrity(&c.db, &c.sb.work).unwrap();
    eprintln!(
        "I12 reference: {} bytes in {:.2}s; {}",
        c.bytes,
        took.as_secs_f64(),
        d.summary()
    );
    (d, took)
}

/// Waits until committed progress is strictly between `after` and the end (and, with
/// `in_cast`, a checkpoint inside a cast is committed), then SIGKILLs the indexer; returns
/// the progress it was killed at.
fn kill_midway(c: &Corpus, after: u64, in_cast: bool, kill: impl FnOnce(), log: impl Fn() -> String) -> u64 {
    let what = if in_cast {
        "a checkpoint committed inside a cast"
    } else {
        "committed progress in the middle of the backfill"
    };
    let at = wait_until(what, scaled(120.0), || {
        if complete(c) {
            panic!(
                "the backfill finished before it could be interrupted (progress {} of {} bytes); \
                 enlarge the corpus\n{}",
                progress(&c.db),
                c.bytes,
                log()
            );
        }
        let p = progress(&c.db);
        (p > after && p < c.bytes && (!in_cast || inside(&c.db) > 0)).then_some(p)
    })
    .unwrap_or_else(|e| panic!("{e:#}\n{}", log()));
    kill();
    // The kill cannot undo a commit: what was observed before it is still there.
    let partial = inside(&c.db);
    eprintln!(
        "I12 killed at {at} of {} bytes, {partial} cast(s) checkpointed inside the file",
        c.bytes
    );
    assert!(!in_cast || partial > 0, "no checkpoint inside a cast at the kill");
    at
}

#[test]
fn i12_serve_killed_during_backfill_resumes_to_the_one_shot_result() {
    need!(Need::Index);
    let c = setup();
    let (want, _) = reference(&c);
    remove_index(&c.db).unwrap();

    let mut at = 0;
    for round in 0..2 {
        let serve = Serve::start(&c.sb, &format!("serve-{round}"), &[]).unwrap();
        let log = serve.log.clone();
        let mut slot = Some(serve);
        at = kill_midway(
            &c,
            at,
            round == 0,
            || slot.take().unwrap().kill_hard().unwrap(),
            || std::fs::read_to_string(&log).unwrap_or_default(),
        );
        let partial = dump(&c);
        assert!(partial.count("commands") < want.count("commands") || progress(&c.db) < c.bytes);
    }

    let serve = Serve::start(&c.sb, "serve-final", &[]).unwrap();
    wait_until("the resumed backfill to finish", scaled(300.0), || {
        complete(&c).then_some(())
    })
    .unwrap_or_else(|e| panic!("{e:#}\n{}", serve.log_text()));
    // Idle again: no backlog left, nothing live.
    wait_until("no backlog", scaled(30.0), || {
        let s = c.sb.status_json().ok()?;
        (s["index"]["backlog_bytes"] == 0).then_some(())
    })
    .unwrap();
    let st = serve.stop(scaled(15.0)).unwrap();
    assert!(st.success(), "{st:?}");

    assert_same("serve after two SIGKILLs", &dump(&c), &want);
    fts_integrity(&c.db, &c.sb.work).unwrap();
    // Search answers the same on the resumed index and on a rebuilt one.
    let terms = ["end of c0k1", "c3k7", "frame 2 of", "größe"];
    let resumed: Vec<Vec<String>> = terms.iter().map(|t| search(&c, t)).collect();
    assert!(resumed.iter().all(|h| !h.is_empty()), "{resumed:?}");
    let out = c.sb.cinelog(&["index", "--rebuild"]).unwrap().expect_ok().unwrap();
    assert!(out.stdout.contains("indexed "), "{out}");
    assert_same("index --rebuild", &dump(&c), &want);
    for (t, before) in terms.iter().zip(&resumed) {
        assert_eq!(&search(&c, t), before, "search {t:?}");
    }
}

#[test]
fn i12_index_once_killed_midway_resumes_to_the_one_shot_result() {
    need!(Need::Index);
    let c = setup();
    let (want, took) = reference(&c);
    remove_index(&c.db).unwrap();

    // `cinelog index --once` in its own process group, killed like a crash.
    let bin = c.sb.bin.clone().unwrap();
    let mut child = {
        use std::os::unix::process::CommandExt;
        std::process::Command::new(bin)
            .env_clear()
            .envs(c.sb.env())
            .args(["index", "--once"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .process_group(0)
            .spawn()
            .unwrap()
    };
    let pid = nix::unistd::Pid::from_raw(child.id() as i32);
    kill_midway(
        &c,
        0,
        true,
        || {
            let _ = nix::sys::signal::killpg(pid, nix::sys::signal::Signal::SIGKILL);
            let _ = child.wait();
        },
        || format!("(one-shot run took {took:?})"),
    );

    let out = c.sb.cinelog(&["index", "--once"]).unwrap().expect_ok().unwrap();
    assert!(out.stdout.contains("indexed "), "{out}");
    assert_same("index --once after SIGKILL", &dump(&c), &want);
    fts_integrity(&c.db, &c.sb.work).unwrap();

    // A further run finds nothing to do and changes nothing.
    let again = c.sb.cinelog(&["index", "--once"]).unwrap().expect_ok().unwrap();
    assert!(
        again
            .stdout
            .contains(&format!("({} unchanged, 0 failed)", c.files.len())),
        "{again}"
    );
    assert_same("idempotent rerun", &dump(&c), &want);
}

/// `(entry, kind, ts)` of every hit, in result order.
fn hits(out: &CliOutput) -> Vec<String> {
    out.json_lines()
        .unwrap()
        .iter()
        .map(|h| format!("{} {} {}", h["entry"], h["kind"], h["ts"]))
        .collect()
}

fn search(c: &Corpus, term: &str) -> Vec<String> {
    hits(
        &c.sb
            .cinelog(&["search", "--json", "-a", term])
            .unwrap()
            .expect(&[0, 1])
            .unwrap(),
    )
}
