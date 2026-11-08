//! End-to-end tests of the legacy importer over the synthetic corpus of
//! `tests/fixtures/legacy-gen` (with the plain-text [`PlainTranscript`] renderer).

use std::collections::BTreeSet;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use cinelog_proto::Scope;

use super::{TranscriptSource, pipelines_with};
use crate::api::{IndexWriter, PipelineFactory};
use crate::fake::{MemIndex, PlainTranscript};
use crate::model::{
    Checkpoint, Chunk, Command, IndexBatch, LegacyImportRow, LegacyOptions, LegacyStatus, PollBudget, RenderOptions,
    SessionMeta, SessionState,
};

#[path = "../../../../tests/fixtures/legacy-gen/legacy_gen.rs"]
mod legacy_gen;

use legacy_gen::{Cmd, Gen, Legacy};

fn plain() -> TranscriptSource {
    TranscriptSource {
        new: |cols, rows, opts| Ok(Box::new(PlainTranscript::new(cols, rows, opts))),
        restore: |state, opts| Ok(Box::new(PlainTranscript::restore(state, opts)?)),
    }
}

fn factory(root: &Path, scope: Scope) -> Box<dyn PipelineFactory> {
    factory_with(root, scope, plain())
}

/// The avt renderer once M2-A has landed (`None` while it is a stand-in).
fn avt() -> Option<TranscriptSource> {
    (TranscriptSource::RENDER.new)(80, 24, RenderOptions::default())
        .ok()
        .map(|_| TranscriptSource::RENDER)
}

fn factory_with(root: &Path, scope: Scope, tsrc: TranscriptSource) -> Box<dyn PipelineFactory> {
    pipelines_with(
        LegacyOptions {
            render: RenderOptions::default(),
            scope,
            logs_root: root.to_owned(),
            host: "box".into(),
        },
        tsrc,
    )
}

const ONE_SHOT: PollBudget = PollBudget {
    max_bytes: u64::MAX,
    live: false,
};

/// All batches of one import, polling with `budget`; with `restart`, every poll runs in a
/// freshly opened pipeline resumed from the previous checkpoint.
fn import(f: &dyn PipelineFactory, cast: &Path, budget: u64, restart: bool) -> Vec<IndexBatch> {
    let budget = PollBudget {
        max_bytes: budget,
        live: false,
    };
    let mut out: Vec<IndexBatch> = Vec::new();
    let mut p = f.open(cast, None).unwrap();
    loop {
        let b = p.poll(budget).unwrap().expect("a batch until finished");
        let done = b.finished;
        let cp: Option<Checkpoint> = b.checkpoint.clone();
        out.push(b);
        if done {
            assert!(p.is_finished());
            assert!(p.poll(budget).unwrap().is_none());
            return out;
        }
        if restart {
            p = f.open(cast, cp).unwrap();
        }
    }
}

/// Applies the batches, checking that chunks only refer to commands already emitted.
fn apply(batches: &[IndexBatch]) -> MemIndex {
    let mut mem = MemIndex::new();
    let mut known = BTreeSet::new();
    for b in batches {
        known.extend(b.commands.iter().map(|c| c.seq));
        for c in &b.chunks {
            if let Some(seq) = c.cmd_seq {
                assert!(known.contains(&seq), "chunk of command {seq} before the command");
            }
        }
        mem.apply(b).unwrap();
    }
    mem
}

struct Imported {
    session: SessionMeta,
    commands: Vec<Command>,
    chunks: Vec<Chunk>,
    row: LegacyImportRow,
    mem: MemIndex,
}

fn run(dir: &Path, name: &str, legacy: &Legacy) -> Imported {
    run_with(dir, name, legacy, plain())
}

fn run_with(dir: &Path, name: &str, legacy: &Legacy, tsrc: TranscriptSource) -> Imported {
    let cast = legacy.write(dir, name).unwrap();
    let f = factory_with(dir, Scope::Personal, tsrc);
    let batches = import(f.as_ref(), &cast, u64::MAX, false);
    let last = batches.last().unwrap();
    let row = last.legacy.clone().expect("import row in the final batch");
    let mem = apply(&batches);
    let uuid = last.session.uuid.clone();
    Imported {
        session: last.session.clone(),
        commands: mem.commands(&uuid),
        chunks: mem.chunks(&uuid),
        row,
        mem,
    }
}

fn output_of(chunks: &[Chunk], seq: u64) -> String {
    chunks
        .iter()
        .filter(|c| c.cmd_seq == Some(seq))
        .map(|c| c.text.as_str())
        .collect::<Vec<_>>()
        .join("\n")
}

/// Every banner becomes exactly one command: it carries the log text (or the reconstructed
/// text when unlogged) at the right time and its output; logged commands without a banner
/// are not commands (§11 step 7).
fn check_scenario(name: &str, legacy: &Legacy, im: &Imported) {
    assert_eq!(im.commands.len(), legacy.banners(), "{name}: one command per banner");
    let mut cmds = im.commands.iter();
    let mut prev_t = 0.0;
    for e in legacy.expect.iter().filter(|e| e.banner) {
        let c = cmds.next().unwrap();
        assert!(c.t_start >= prev_t, "{name}: seq order is time order");
        prev_t = c.t_start;
        assert!(c.approx && c.exit.is_none() && c.cwd.is_none() && c.expanded.is_none());
        assert!(
            (c.t_start - e.t).abs() < 0.05,
            "{name}: {:?} at {} vs {}",
            e.typed,
            c.t_start,
            e.t
        );
        assert_eq!(c.ts_start, im.session.started_at + c.t_start);
        match &e.logged {
            Some(log) => {
                assert_eq!(&c.typed, log, "{name}");
                let conf = c.confidence.unwrap();
                assert!(conf >= 0.8, "{name}: {:?} confidence {conf}", e.typed);
                assert_eq!(c.typed_trunc, e.truncated);
            }
            None => {
                assert_eq!(c.confidence, None, "{name}");
                assert_eq!(c.typed, e.typed, "{name}: reconstructed");
            }
        }
        let out = output_of(&im.chunks, c.seq);
        for l in &e.output {
            assert!(
                out.contains(l.as_str()),
                "{name}: output {l:?} of {:?} in {out:?}",
                e.typed
            );
        }
        assert!(c.t_end.unwrap() >= c.t_start);
    }
    for ch in &im.chunks {
        let stripped = super::banner::strip_banner_lines(&ch.text, |_| true);
        assert_eq!(
            stripped.as_deref(),
            Some(ch.text.as_str()),
            "{name}: banner left in {ch:?}"
        );
    }
    let r = &im.row;
    assert_eq!(r.banners as usize, legacy.banners(), "{name}");
    assert_eq!(r.log_lines as usize, legacy.logged(), "{name}");
    let aligned = legacy.expect.iter().filter(|e| e.banner && e.logged.is_some()).count();
    assert_eq!(r.aligned as usize, aligned, "{name}");
    assert_eq!(r.mean_conf.is_some(), aligned > 0);
}

#[test]
fn synthetic_corpus_is_imported_and_aligned() {
    check_corpus(plain());
}

#[test]
fn synthetic_corpus_with_the_avt_renderer() {
    if let Some(avt) = avt() {
        check_corpus(avt);
    }
}

fn check_corpus(tsrc: TranscriptSource) {
    let d = tempfile::tempdir().unwrap();
    for (name, legacy) in legacy_gen::corpus() {
        let im = run_with(d.path(), name, &legacy, tsrc);
        let s = &im.session;
        assert!(s.legacy && !s.compressed, "{name}");
        assert_eq!(s.attribution.as_deref(), Some("legacy"));
        assert_eq!(s.state, SessionState::Ended);
        assert!(s.started_at >= 1_698_541_140.0, "{name}: header timestamp");
        assert_eq!(s.exit_status, Some(0));
        assert_eq!(
            im.row.status,
            if name == "dumb.cast" {
                LegacyStatus::Hidden
            } else {
                LegacyStatus::Imported
            }
        );
        assert_eq!(s.hidden, name == "dumb.cast");
        assert_eq!(im.row.commands_log.is_some(), name != "nolog.cast", "{name}");
        if name == "nolog.cast" {
            // Banner commands with reconstructed text, no alignment.
            assert_eq!(im.commands.len(), 1);
            assert_eq!(im.commands[0].typed, "ls");
            assert_eq!(im.row.mean_conf, None);
            continue;
        }
        check_scenario(name, &legacy, &im);
        assert_eq!(s.counters.bad_markers, 0, "{name}: DST steps are not drift");
        assert!(!im.mem.segments(&s.uuid).is_empty());
    }
}

#[test]
fn alignment_ratio_of_a_long_cast() {
    let d = tempfile::tempdir().unwrap();
    let legacy = legacy_gen::long(42, 300);
    let im = run(d.path(), "long.cast", &legacy);
    check_scenario("long", &legacy, &im);
    let good = im
        .commands
        .iter()
        .filter(|c| c.confidence.is_some_and(|c| c >= 0.8))
        .count();
    assert_eq!(good, legacy.logged());
}

#[test]
fn resumed_import_equals_one_shot() {
    check_resume(plain());
}

#[test]
fn resumed_import_equals_one_shot_with_avt() {
    if let Some(avt) = avt() {
        check_resume(avt);
    }
}

fn check_resume(tsrc: TranscriptSource) {
    let d = tempfile::tempdir().unwrap();
    let legacy = legacy_gen::long(7, 120);
    let cast = legacy.write(d.path(), "resume.cast").unwrap();
    let f = factory_with(d.path(), Scope::Personal, tsrc);
    let one = apply(&import(f.as_ref(), &cast, u64::MAX, false));
    for budget in [512, 4096] {
        let batches = import(f.as_ref(), &cast, budget, true);
        assert!(batches.len() > 10, "{budget}");
        let many = apply(&batches);
        let uuid = one.sessions()[0].uuid.clone();
        assert_eq!(many.commands(&uuid), one.commands(&uuid), "{budget}");
        assert_eq!(many.chunks(&uuid), one.chunks(&uuid), "{budget}");
        assert_eq!(many.segments(&uuid), one.segments(&uuid), "{budget}");
        assert_eq!(many.sessions(), one.sessions(), "{budget}");
        assert_eq!(many.legacy_report_rows(), one.legacy_report_rows());
    }
}

trait Rows {
    fn legacy_report_rows(&self) -> Vec<LegacyImportRow>;
}

impl Rows for MemIndex {
    fn legacy_report_rows(&self) -> Vec<LegacyImportRow> {
        use crate::api::Searcher;
        self.legacy_report().unwrap()
    }
}

#[test]
fn changed_file_resets_the_checkpoint() {
    let d = tempfile::tempdir().unwrap();
    let legacy = legacy_gen::long(3, 20);
    let cast = legacy.write(d.path(), "c.cast").unwrap();
    let f = factory(d.path(), Scope::Personal);
    let mut p = f.open(&cast, None).unwrap();
    let b = p
        .poll(PollBudget {
            max_bytes: 2048,
            live: false,
        })
        .unwrap()
        .unwrap();
    assert!(!b.reset && !b.finished);
    let mut cp = b.checkpoint.unwrap();
    cp.file.size += 1;
    let mut p = f.open(&cast, Some(cp.clone())).unwrap();
    let b = p.poll(ONE_SHOT).unwrap().unwrap();
    assert!(b.reset && b.finished);
    // A state from an older importer is ignored the same way.
    cp.file.size -= 1;
    cp.pipeline_state = "{}".into();
    let b = f.open(&cast, Some(cp)).unwrap().poll(ONE_SHOT).unwrap().unwrap();
    assert!(b.reset);
}

#[test]
fn files_are_never_touched() {
    let d = tempfile::tempdir().unwrap();
    let legacy = legacy_gen::long(5, 30);
    let cast = legacy.write(d.path(), "ro.cast").unwrap();
    let log = PathBuf::from(format!("{}.commands.log", cast.display()));
    // An old atime that relatime would refresh on any normal read.
    let old = nix::sys::time::TimeSpec::new(1_600_000_000, 0);
    for p in [&cast, &log] {
        nix::sys::stat::utimensat(
            nix::fcntl::AT_FDCWD,
            p.as_path(),
            &old,
            &nix::sys::time::TimeSpec::UTIME_OMIT,
            nix::sys::stat::UtimensatFlags::FollowSymlink,
        )
        .unwrap();
    }
    // The snapshot's own read must not touch the atime either.
    let snapshot = |p: &Path| {
        let mut body = Vec::new();
        std::io::Read::read_to_end(&mut crate::classify::open_noatime(p).unwrap(), &mut body).unwrap();
        let md = std::fs::metadata(p).unwrap();
        (md.atime(), md.mtime(), md.mtime_nsec(), md.ctime_nsec(), body)
    };
    let before: Vec<_> = [&cast, &log].iter().map(|p| snapshot(p)).collect();
    let listing = |d: &Path| {
        let mut v: Vec<_> = std::fs::read_dir(d).unwrap().map(|e| e.unwrap().file_name()).collect();
        v.sort();
        v
    };
    let names = listing(d.path());
    let f = factory(d.path(), Scope::Personal);
    import(f.as_ref(), &cast, 1024, true);
    let after: Vec<_> = [&cast, &log].iter().map(|p| snapshot(p)).collect();
    for (b, a) in before.iter().zip(&after) {
        assert_eq!((b.1, b.2, b.3), (a.1, a.2, a.3), "mtime/ctime unchanged");
        assert_eq!(b.4, a.4, "content unchanged");
        assert_eq!(a.0, 1_600_000_000, "atime untouched (O_NOATIME)");
    }
    assert_eq!(listing(d.path()), names, "nothing written next to the files");
}

#[test]
fn users_by_scope() {
    let d = tempfile::tempdir().unwrap();
    let alice = d.path().join("alice").join("2023");
    std::fs::create_dir_all(&alice).unwrap();
    let legacy = legacy_gen::long(1, 2);
    let cast = legacy.write(&alice, "t.cast").unwrap();
    let team = factory(d.path(), Scope::Team);
    let b = team.open(&cast, None).unwrap().poll(ONE_SHOT).unwrap().unwrap();
    assert_eq!(
        (b.session.user.as_str(), b.session.euid, b.session.scope),
        ("alice", Some(0), Scope::Team)
    );
    let personal = factory(d.path(), Scope::Personal);
    let b = personal.open(&cast, None).unwrap().poll(ONE_SHOT).unwrap().unwrap();
    let uid = std::fs::metadata(&cast).unwrap().uid();
    assert_eq!(b.session.euid, Some(uid));
    assert_eq!(b.session.user, super::source::user_name(uid));
    assert_eq!(b.session.host, "box");
    let name = cast.file_name().unwrap().to_str().unwrap();
    let raw = std::fs::read(&cast).unwrap();
    let sha = {
        use sha2::Digest;
        data_encoding::HEXLOWER.encode(&sha2::Sha256::digest(&raw[..raw.len().min(1 << 20)]))
    };
    assert_eq!(b.session.uuid, crate::model::legacy_uuid(name, raw.len() as u64, &sha));
    assert_eq!(b.legacy.unwrap().sha256, sha);
}

#[test]
fn cinelog_casts_are_refused() {
    let d = tempfile::tempdir().unwrap();
    let p = d.path().join("new.cast");
    let session = r#"{\"cl\":1,\"k\":\"session\",\"uuid\":\"01934f5e-7b2a-7c3d-8e4f-0123456789ab\",\"part\":1,\"start_epoch_us\":1,\"scope\":\"personal\",\"user\":\"u\",\"attribution\":\"ancestor\"}"#;
    std::fs::write(
        &p,
        format!("{{\"version\":3,\"term\":{{\"cols\":80,\"rows\":24}}}}\n[0.0, \"m\", \"{session}\"]\n"),
    )
    .unwrap();
    assert!(factory(d.path(), Scope::Personal).open(&p, None).is_err());
}

#[test]
fn damaged_casts_count_bad_lines_and_replayed_banners() {
    let d = tempfile::tempdir().unwrap();
    let mut g = Gen::new(11);
    g.draw_prompt();
    g.run(&Cmd::new("cat old.cast").out(&[
        // Banners from another day, replayed as output: not commands.
        "                    Begin [2019-01-01 10:00:00]",
        "                    End   [2019-01-01 10:00:05]",
    ]));
    g.run(&Cmd::new("ls").out(&["x"]));
    let legacy = g.finish(0, true);
    let mut cast = legacy.cast.clone();
    let cut = cast.rfind("\n[").unwrap();
    cast.insert_str(cut + 1, "\0\0\0garbage line\n");
    let name = "damaged.cast";
    std::fs::write(d.path().join(name), cast).unwrap();
    std::fs::write(
        d.path().join(format!("{name}.commands.log")),
        legacy.log.as_ref().unwrap(),
    )
    .unwrap();
    let f = factory(d.path(), Scope::Personal);
    let batches = import(f.as_ref(), &d.path().join(name), u64::MAX, false);
    let last = batches.last().unwrap();
    assert!(last.session.counters.bad_lines >= 1);
    assert_eq!(last.session.counters.bad_markers, 2);
    let mem = apply(&batches);
    let cmds = mem.commands(&last.session.uuid);
    assert_eq!(
        cmds.iter().map(|c| c.typed.as_str()).collect::<Vec<_>>(),
        ["cat old.cast", "ls"]
    );
    assert!(output_of(&mem.chunks(&last.session.uuid), 1).contains("Begin [2019-01-01 10:00:00]"));
}

#[test]
fn clock_drift_is_flagged() {
    let d = tempfile::tempdir().unwrap();
    let mut g = Gen::new(12);
    g.draw_prompt();
    g.run(&Cmd::new("echo a").out(&["a"]));
    // A suspend: the wall clock moves on, the cast clock does not.
    g.clock_step(100);
    g.run(&Cmd::new("echo b").out(&["b"]));
    g.run(&Cmd::new("echo c").out(&["c"]));
    let im = run(d.path(), "drift.cast", &g.finish(0, true));
    assert_eq!(im.session.counters.bad_markers, 2);
    assert_eq!(im.commands.len(), 3);
}

#[test]
fn finish_before_the_end_closes_everything() {
    let d = tempfile::tempdir().unwrap();
    let legacy = legacy_gen::long(9, 40);
    let cast = legacy.write(d.path(), "f.cast").unwrap();
    let f = factory(d.path(), Scope::Personal);
    let mut p = f.open(&cast, None).unwrap();
    let first = p
        .poll(PollBudget {
            max_bytes: 4096,
            live: false,
        })
        .unwrap()
        .unwrap();
    let b = p.finish(SessionState::Error).unwrap();
    assert!(b.finished && p.is_finished());
    assert_eq!(b.session.state, SessionState::Error);
    assert_eq!(b.provisional, Some(Vec::new()));
    let row = b.legacy.clone().unwrap();
    assert!(row.banners >= 1 && (row.banners as usize) < legacy.banners());
    // A partly read cast is not a successful import.
    assert_eq!(row.status, LegacyStatus::Failed);
    let err = row.error.as_deref().unwrap();
    assert!(
        err.starts_with("read stopped at offset ") && err.ends_with("(error)"),
        "{err}"
    );
    assert!(!err.contains('\n'));
    assert!(b.commands.iter().all(|c| c.t_end.is_some()));
    let again = p.finish(SessionState::Ended).unwrap();
    assert!(again.finished && again.commands.is_empty() && again.legacy.is_none());
    apply(&[first, b]);
}

const HEADER: &str = r#"{"version":3,"term":{"cols":80,"rows":24,"type":"xterm"},"timestamp":1700000000}"#;
/// Banner wall clocks plausible for [`HEADER`] (UTC+1).
const BEGIN: &str = "Begin [2023-11-14 23:13:21]";
const END: &str = "End   [2023-11-14 23:13:22]";

/// Writes a v3 cast from `(interval, code, data)` events (and a commands.log).
fn write_cast(dir: &Path, name: &str, header: &str, events: &[(f64, &str, &str)], log: Option<&str>) -> PathBuf {
    let mut cast = format!("{header}\n");
    for (dt, code, data) in events {
        cast.push_str(&format!("[{dt:.6}, \"{code}\", {}]\n", legacy_gen::json_string(data)));
    }
    let path = dir.join(name);
    std::fs::write(&path, cast).unwrap();
    if let Some(log) = log {
        std::fs::write(super::source::log_path(&path), log).unwrap();
    }
    path
}

fn import_one(dir: &Path, cast: &Path) -> (IndexBatch, MemIndex) {
    let f = factory(dir, Scope::Personal);
    let batches = import(f.as_ref(), cast, u64::MAX, false);
    (batches.last().unwrap().clone(), apply(&batches))
}

fn pad(s: &str) -> String {
    format!("{s:>80}\r\n")
}

/// A banner carried over from an earlier event keeps that event's time for both the
/// command and the transcript boundary.
#[test]
fn carried_banner_keeps_its_event_time() {
    let d = tempfile::tempdir().unwrap();
    let begin = pad(BEGIN);
    let (head, tail) = begin.split_at(begin.len() - 26);
    let end = pad(END) + "$ ";
    let cast = write_cast(
        d.path(),
        "split.cast",
        HEADER,
        &[
            (0.5, "o", "$ "),
            (0.5, "o", "ls"),
            (0.2, "o", "\r\n"),
            (0.1, "o", head),
            (2.0, "o", &format!("{tail}file-a\r\n")),
            (0.5, "o", &end),
            (0.5, "o", "\r\n"),
            (0.1, "x", "0"),
        ],
        None,
    );
    let (last, mem) = import_one(d.path(), &cast);
    let cmds = mem.commands(&last.session.uuid);
    assert_eq!(cmds.len(), 1);
    assert!((cmds[0].t_start - 1.3).abs() < 1e-9, "{}", cmds[0].t_start);
    assert_eq!(cmds[0].ts_start, last.session.started_at + cmds[0].t_start);
    let chunks = mem.chunks(&last.session.uuid);
    assert!(output_of(&chunks, 1).contains("file-a"));
    // Chunk times are kept in milliseconds.
    assert!(
        chunks
            .iter()
            .filter(|c| c.cmd_seq == Some(1))
            .all(|c| c.t >= cmds[0].t_start - 1e-3)
    );
}

/// A banner whose bytes span a resize event is still a banner (and stripped from chunks).
#[test]
fn banner_split_by_a_resize_is_kept() {
    let d = tempfile::tempdir().unwrap();
    let begin = pad(BEGIN);
    let (head, tail) = begin.split_at(begin.len() - 10);
    let end = pad(END) + "$ ";
    let cast = write_cast(
        d.path(),
        "resize.cast",
        HEADER,
        &[
            (0.5, "o", "alice@box:~$ ls"),
            (0.2, "o", &format!("\r\n{head}")),
            (0.1, "r", "90x30"),
            (0.1, "o", &format!("{tail}file-a\r\n")),
            (0.5, "o", &end),
            (0.5, "o", "\r\n"),
            (0.1, "x", "0"),
        ],
        None,
    );
    let (last, mem) = import_one(d.path(), &cast);
    let cmds = mem.commands(&last.session.uuid);
    assert_eq!(cmds.len(), 1, "{cmds:?}");
    assert_eq!(cmds[0].typed, "ls");
    // Timed at the resize, after which its bytes are rendered.
    assert!((cmds[0].t_start - 0.8).abs() < 1e-9, "{}", cmds[0].t_start);
    let chunks = mem.chunks(&last.session.uuid);
    assert!(output_of(&chunks, 1).contains("file-a"));
    assert!(chunks.iter().all(|c| !c.text.contains("Begin [")), "{chunks:?}");
    assert_eq!(last.session.cols, 80);
}

/// Without banners a commands.log yields no commands, only gap chunks (§11 step 7); the
/// log lines are counted in the import row. A small `TERM=dumb` cast stays hidden.
#[test]
fn log_without_banners_yields_no_commands() {
    let d = tempfile::tempdir().unwrap();
    let log = "[-1.5, \"m\", \"ls -la\"]\n[-0.5, \"m\", \"make test\"]\n";
    let events = [
        (0.5, "o", "$ ls -la\r\nfile-a\r\n$ "),
        (0.5, "o", "make test\r\nok\r\n$ "),
        (0.1, "x", "0"),
    ];
    for (name, header, status) in [
        ("plain.cast", HEADER.to_owned(), LegacyStatus::Imported),
        ("dumb.cast", HEADER.replace("xterm", "dumb"), LegacyStatus::Hidden),
    ] {
        let cast = write_cast(d.path(), name, &header, &events, Some(log));
        let (last, mem) = import_one(d.path(), &cast);
        let uuid = &last.session.uuid;
        assert!(mem.commands(uuid).is_empty(), "{name}");
        let chunks = mem.chunks(uuid);
        assert!(
            !chunks.is_empty() && chunks.iter().all(|c| c.cmd_seq.is_none()),
            "{name}"
        );
        let row = last.legacy.unwrap();
        assert_eq!((row.banners, row.log_lines, row.aligned), (0, 2, 0), "{name}");
        assert_eq!(row.status, status, "{name}");
        assert_eq!(last.session.hidden, status == LegacyStatus::Hidden);
    }
}

/// A zstd legacy cast is recorded as compressed with its uncompressed size, and its offsets
/// are positions in the decompressed stream.
#[test]
fn zstd_cast_is_imported_as_compressed() {
    let d = tempfile::tempdir().unwrap();
    let legacy = legacy_gen::long(4, 30);
    let plain_dir = d.path().join("plain");
    let zst_dir = d.path().join("zst");
    std::fs::create_dir_all(&plain_dir).unwrap();
    std::fs::create_dir_all(&zst_dir).unwrap();
    let plain_cast = legacy.write(&plain_dir, "z.cast").unwrap();
    let zst = zst_dir.join("z.cast.zst");
    std::fs::write(&zst, zstd::encode_all(legacy.cast.as_bytes(), 3).unwrap()).unwrap();
    std::fs::write(zst_dir.join("z.cast.commands.log"), legacy.log.as_ref().unwrap()).unwrap();

    let (pl, pmem) = import_one(&plain_dir, &plain_cast);
    let f = factory(&zst_dir, Scope::Personal);
    let batches = import(f.as_ref(), &zst, 2048, true);
    let last = batches.last().unwrap();
    let zmem = apply(&batches);
    let s = &last.session;
    let size = legacy.cast.len() as u64;
    assert!(s.compressed && s.legacy);
    assert_eq!(s.cast_bytes, Some(size));
    assert!(batches[0].session.cast_bytes.is_none(), "unknown until read to the end");
    let part = last.parts.last().unwrap();
    assert!(part.compressed);
    assert_eq!(part.bytes, Some(size));
    assert!(std::fs::metadata(&zst).unwrap().len() < size);
    assert!(last.legacy.as_ref().unwrap().commands_log.is_some());
    assert_eq!(last.legacy.as_ref().unwrap().status, LegacyStatus::Imported);
    // Same commands and segment offsets as the plain cast: decompressed positions.
    assert_eq!(zmem.commands(&s.uuid), pmem.commands(&pl.session.uuid));
    let zseg = zmem.segments(&s.uuid);
    assert_eq!(zseg, pmem.segments(&pl.session.uuid));
    for seg in &zseg {
        let at = seg.byte_offset as usize;
        assert!(at == HEADER.len() + 1 || legacy.cast.as_bytes()[at - 1] == b'\n');
        assert!(legacy.cast[at..].starts_with('['), "event line at {at}");
    }
    assert!(!zseg.is_empty());
}

/// §11 acceptance: an inotify watch (`modify, attrib, move, delete` and more) on a copy of
/// the fixture corpus sees nothing while the whole corpus is imported.
#[test]
fn import_raises_no_inotify_events() {
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::os::unix::ffi::OsStrExt;

    let d = tempfile::tempdir().unwrap();
    let mut casts = Vec::new();
    for (name, legacy) in legacy_gen::corpus() {
        casts.push(legacy.write(d.path(), name).unwrap());
    }
    // SAFETY: plain syscall; the returned descriptor is owned below.
    let raw = unsafe { libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC) };
    assert!(raw >= 0, "inotify_init1: {}", std::io::Error::last_os_error());
    // SAFETY: `raw` is a fresh descriptor nobody else owns.
    let fd = unsafe { OwnedFd::from_raw_fd(raw) };
    let mask = libc::IN_MODIFY
        | libc::IN_ATTRIB
        | libc::IN_MOVE
        | libc::IN_DELETE
        | libc::IN_CREATE
        | libc::IN_CLOSE_WRITE
        | libc::IN_DELETE_SELF
        | libc::IN_MOVE_SELF;
    let mut watched: Vec<PathBuf> = vec![d.path().to_owned()];
    for e in std::fs::read_dir(d.path()).unwrap() {
        watched.push(e.unwrap().path());
    }
    assert!(watched.len() > casts.len(), "casts and their logs");
    for p in &watched {
        let c = std::ffi::CString::new(p.as_os_str().as_bytes()).unwrap();
        // SAFETY: valid descriptor and NUL-terminated path.
        let wd = unsafe { libc::inotify_add_watch(fd.as_raw_fd(), c.as_ptr(), mask) };
        assert!(wd >= 0, "inotify_add_watch: {}", std::io::Error::last_os_error());
    }
    let f = factory(d.path(), Scope::Personal);
    for cast in &casts {
        import(f.as_ref(), cast, 4096, true);
    }
    let mut buf = vec![0u8; 64 * 1024];
    // SAFETY: `buf` is valid for `buf.len()` bytes.
    let n = unsafe { libc::read(fd.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) };
    let err = std::io::Error::last_os_error();
    assert!(
        n < 0 && err.kind() == std::io::ErrorKind::WouldBlock,
        "inotify saw {n} bytes of events ({err})"
    );
}

/// Aggregate-only import statistics over a real legacy corpus (§11 acceptance), with the
/// plain-text renderer standing in for avt. Prints numbers, never content:
/// `CINELOG_LEGACY_CORPUS=~/logs cargo test -p cinelog-index --release --lib legacy::tests::corpus -- --ignored --nocapture`
#[test]
#[ignore = "needs CINELOG_LEGACY_CORPUS"]
fn corpus_statistics() {
    let Some(root) = std::env::var_os("CINELOG_LEGACY_CORPUS").map(PathBuf::from) else {
        return;
    };
    let tsrc = match std::env::var("CINELOG_LEGACY_RENDER").as_deref() {
        Ok("avt") => avt().expect("render::transcript is available"),
        _ => plain(),
    };
    let f = factory_with(&root, Scope::Personal, tsrc);
    let started = std::time::Instant::now();
    let (mut casts, mut bytes, mut failed, mut hidden) = (0u64, 0u64, 0u64, 0u64);
    let (mut banners, mut log_lines, mut aligned, mut good, mut unplaced, mut bad_markers) = (0u64, 0, 0, 0, 0, 0);
    let mut paths: Vec<PathBuf> = walkdir::WalkDir::new(&root)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file() && e.path().extension().is_some_and(|x| x == "cast"))
        .map(|e| e.into_path())
        .collect();
    paths.sort();
    for p in &paths {
        if crate::classify::classify(p).ok() != Some(crate::classify::CastKind::Legacy) {
            continue;
        }
        casts += 1;
        let mut pl = match f.open(p, None) {
            Ok(pl) => pl,
            Err(_) => {
                failed += 1;
                continue;
            }
        };
        let mut last = None;
        loop {
            match pl.poll(PollBudget {
                max_bytes: 8 << 20,
                live: false,
            }) {
                Ok(Some(b)) => {
                    let done = b.finished;
                    last = Some(b);
                    if done {
                        break;
                    }
                }
                Ok(None) => break,
                Err(_) => {
                    failed += 1;
                    break;
                }
            }
        }
        let Some(b) = last.filter(|b| b.finished) else { continue };
        let row = b.legacy.unwrap();
        bytes += row.file.size;
        hidden += u64::from(row.status == LegacyStatus::Hidden);
        banners += row.banners;
        log_lines += row.log_lines;
        aligned += row.aligned;
        bad_markers += b.session.counters.bad_markers;
        good += b
            .commands
            .iter()
            .filter(|c| c.confidence.is_some_and(|x| x >= 0.8))
            .count() as u64;
        unplaced += row.log_lines - row.aligned;
    }
    let pct = |a: u64, b: u64| if b == 0 { 0.0 } else { 100.0 * a as f64 / b as f64 };
    println!(
        "casts {casts} ({:.1} MB) failed {failed} hidden {hidden} | banners {banners} log_lines {log_lines} \
         aligned {aligned} ({:.1}%) conf>=0.8 {good} ({:.1}% of log lines) unplaced {unplaced} \
         bad_markers {bad_markers} | {:.1} s",
        bytes as f64 / 1e6,
        pct(aligned, log_lines),
        pct(good, log_lines),
        started.elapsed().as_secs_f64()
    );
}
