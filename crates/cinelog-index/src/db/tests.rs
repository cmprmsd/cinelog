//! Writer, reader and meta store against real SQLite files in a temp dir.

use std::path::PathBuf;
use std::time::Duration;

use cinelog_core::config::IndexConfig;
use cinelog_proto::Scope;
use rusqlite::Connection;

use super::query::{SqliteSearcher, candidate_sql, scan_sql};
use super::writer::REDACTED;
use super::*;
use crate::api::{IndexWriter, MetaStore, Searcher};
use crate::model::*;

const U1: &str = "01934f5e-7b2a-7c3d-8e4f-0123456789ab";
const U2: &str = "01934f5e-7b2a-7c3d-8e4f-0123456789cd";
const U3: &str = "01934f5e-7b2a-7c3d-8e4f-0123456789ef";
const T0: f64 = 1_794_139_200.0;

struct Env {
    _dir: tempfile::TempDir,
    paths: DbPaths,
}

impl Env {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let paths = DbPaths::in_dir(dir.path().join("host"));
        Self { _dir: dir, paths }
    }

    fn writer(&self) -> Box<dyn IndexWriter> {
        open_writer(&self.paths, &IndexConfig::default()).unwrap()
    }

    fn reader(&self) -> Box<dyn Searcher> {
        open_reader(&self.paths).unwrap()
    }

    fn meta(&self) -> Box<dyn MetaStore> {
        open_meta(&self.paths).unwrap()
    }

    fn raw(&self) -> Connection {
        Connection::open(&self.paths.index).unwrap()
    }

    fn cast(&self, name: &str) -> PathBuf {
        self.paths.dir.join(name)
    }
}

fn session(env: &Env, uuid: &str) -> SessionMeta {
    SessionMeta {
        uuid: uuid.into(),
        host: "h1".into(),
        scope: Scope::Personal,
        user: "alice".into(),
        euid: Some(1000),
        sudo_user: None,
        claimed_user: None,
        attribution: Some("ancestor".into()),
        login_via: None,
        tty: Some("/dev/pts/3".into()),
        holder_comm: Some("tmux: server".into()),
        tmux: TmuxLocation {
            server: Some("10:20".into()),
            session_id: Some("$1".into()),
            session: Some("main".into()),
            window: Some("0".into()),
            window_name: Some("zsh".into()),
            pane: Some("%3".into()),
        },
        parent_uuid: None,
        cast_path: env.cast(&format!("{uuid}.cast")),
        parts: 1,
        compressed: false,
        cast_bytes: None,
        started_at: T0,
        ended_at: None,
        exit_status: None,
        tz: Some("Europe/Berlin".into()),
        cols: 80,
        rows: 24,
        term: Some("xterm-256color".into()),
        legacy: false,
        hidden: false,
        state: SessionState::Live,
        counters: SessionCounters::default(),
    }
}

fn command(seq: Seq, typed: &str, t: f64) -> Command {
    Command {
        seq,
        parent_seq: None,
        depth: 0,
        shell_pid: Some(42),
        t_start: t,
        t_end: Some(t + 1.0),
        ts_start: T0 + t,
        ts_end: Some(T0 + t + 1.0),
        exit: Some(0),
        pipestatus: vec![0],
        cwd: Some("/home/alice".into()),
        user: Some("alice".into()),
        euid: Some(1000),
        typed: typed.into(),
        expanded: None,
        typed_trunc: false,
        approx: false,
        confidence: None,
        out_bytes: 0,
        out_truncated: false,
    }
}

fn chunk(cmd_seq: Option<Seq>, t: f64, text: &str) -> Chunk {
    Chunk {
        kind: if cmd_seq.is_some() {
            ChunkKind::Primary
        } else {
            ChunkKind::Gap
        },
        cmd_seq,
        t,
        ts: T0 + t,
        line0: 0,
        text: text.into(),
    }
}

/// U1: three commands with output and one gap chunk.
fn fixture(env: &Env, w: &mut dyn IndexWriter) {
    let mut b = IndexBatch::new(session(env, U1));
    b.commands = vec![
        command(1, "grep needle file.txt", 10.0),
        command(2, "make 50%_off", 20.0),
        command(3, "cat ärger.txt", 30.0),
    ];
    b.commands[1].exit = Some(2);
    b.commands[2].cwd = Some("/tmp".into());
    b.chunks = vec![
        chunk(None, 1.0, "welcome banner"),
        chunk(Some(1), 10.5, "file.txt: a NEEDLE in the haystack"),
        chunk(Some(2), 20.5, "error: 50% off is not a target"),
        chunk(Some(3), 30.5, "viel ÄRGER hier"),
    ];
    w.apply(&b).unwrap();
}

fn search(r: &dyn Searcher, term: &str, target: SearchTarget) -> SearchPage {
    let mut q = SearchQuery::new(term);
    q.target = target;
    r.search(&q).unwrap()
}

fn seqs(p: &SearchPage) -> Vec<(HitKind, Option<Seq>)> {
    p.hits.iter().map(|h| (h.kind, h.cmd_seq)).collect()
}

fn count(conn: &Connection, sql: &str) -> i64 {
    conn.query_row(sql, [], |r| r.get(0)).unwrap()
}

fn fts_integrity(conn: &Connection) {
    conn.execute_batch(
        "INSERT INTO cmd_fts(cmd_fts, rank) VALUES('integrity-check', 1);
         INSERT INTO out_fts(out_fts, rank) VALUES('integrity-check', 1);",
    )
    .unwrap();
}

#[test]
fn writer_lock_is_exclusive() {
    let env = Env::new();
    let w = env.writer();
    assert!(matches!(
        open_writer(&env.paths, &IndexConfig::default()),
        Err(IndexError::Locked(p)) if p == env.paths.writer_lock
    ));
    drop(w);
    let _again = env.writer();
}

#[test]
fn reader_needs_an_index() {
    let env = Env::new();
    assert!(open_reader(&env.paths).is_err());
    drop(env.writer());
    let r = env.reader();
    assert_eq!(r.status().unwrap().schema_version, crate::schema::INDEX_SCHEMA_VERSION);
}

#[test]
fn command_and_output_search() {
    let env = Env::new();
    let mut w = env.writer();
    fixture(&env, w.as_mut());
    let r = env.reader();

    let p = search(r.as_ref(), "NEEDLE", SearchTarget::Commands);
    assert_eq!(seqs(&p), [(HitKind::Command, Some(1))]);
    let h = &p.hits[0];
    assert_eq!(
        (h.session_uuid.as_str(), h.user.as_str(), h.host.as_str()),
        (U1, "alice", "h1")
    );
    assert_eq!((h.t, h.ts, h.exit), (10.0, T0 + 10.0, Some(0)));
    assert_eq!(h.typed.as_deref(), Some("grep needle file.txt"));

    let p = search(r.as_ref(), "needle", SearchTarget::All);
    assert_eq!(
        seqs(&p),
        [(HitKind::Output, Some(1)), (HitKind::Command, Some(1))],
        "newest first"
    );
    let out = &p.hits[0];
    assert_eq!(out.chunk_kind, Some(ChunkKind::Primary));
    assert_eq!(out.typed.as_deref(), Some("grep needle file.txt"));
    assert_eq!(
        out.snippet.as_deref(),
        Some("file.txt: a \u{E000}NEEDLE\u{E001} in the haystack")
    );
    assert_eq!(
        out.entry(),
        EntryRef::Command {
            uuid: U1.into(),
            seq: 1
        }
    );

    // Gap output hits address the point in time.
    let p = search(r.as_ref(), "banner", SearchTarget::Output);
    assert_eq!(seqs(&p), [(HitKind::Output, None)]);
    assert_eq!(
        p.hits[0].entry(),
        EntryRef::At {
            uuid: U1.into(),
            t: 1.0
        }
    );
    assert_eq!(p.hits[0].chunk_kind, Some(ChunkKind::Gap));

    // cwd is searched too.
    assert_eq!(
        seqs(&search(r.as_ref(), "/tmp", SearchTarget::Commands)),
        [(HitKind::Command, Some(3))]
    );
    assert!(
        search(r.as_ref(), "nothing-like-this", SearchTarget::All)
            .hits
            .is_empty()
    );
}

#[test]
fn like_wildcards_are_post_filtered() {
    let env = Env::new();
    let mut w = env.writer();
    fixture(&env, w.as_mut());
    let r = env.reader();
    // `50%_off` as LIKE would also match "50% off"; the post-filter keeps literal matches.
    let p = search(r.as_ref(), "50%_off", SearchTarget::All);
    assert_eq!(seqs(&p), [(HitKind::Command, Some(2))]);
    let p = search(r.as_ref(), "50% off", SearchTarget::All);
    assert_eq!(seqs(&p), [(HitKind::Output, Some(2))]);
}

#[test]
fn unicode_case_folding() {
    let env = Env::new();
    let mut w = env.writer();
    fixture(&env, w.as_mut());
    let r = env.reader();
    let p = search(r.as_ref(), "ärger", SearchTarget::All);
    assert_eq!(seqs(&p), [(HitKind::Output, Some(3)), (HitKind::Command, Some(3))]);
    assert_eq!(p.hits[0].snippet.as_deref(), Some("viel \u{E000}ÄRGER\u{E001} hier"));
    let p = search(r.as_ref(), "ÄRGER", SearchTarget::Commands);
    assert_eq!(seqs(&p), [(HitKind::Command, Some(3))]);
}

#[test]
fn case_sensitive_glob() {
    let env = Env::new();
    let mut w = env.writer();
    let mut b = IndexBatch::new(session(&env, U1));
    b.commands = vec![
        command(1, "echo Main[0]", 1.0),
        command(2, "echo main[0]", 2.0),
        command(3, "echo Main0", 3.0),
    ];
    w.apply(&b).unwrap();
    let r = env.reader();
    let mut q = SearchQuery::new("Main[0]");
    q.case_sensitive = true;
    assert_eq!(seqs(&r.search(&q).unwrap()), [(HitKind::Command, Some(1))]);
    q.case_sensitive = false;
    assert_eq!(
        seqs(&r.search(&q).unwrap()),
        [(HitKind::Command, Some(2)), (HitKind::Command, Some(1))]
    );
}

#[test]
fn short_terms_scan_newest_first() {
    let env = Env::new();
    let mut w = env.writer();
    fixture(&env, w.as_mut());
    let mut r = SqliteSearcher::open(&env.paths).unwrap();
    let mut q = SearchQuery::new("ma");
    q.target = SearchTarget::All;
    let p = r.search(&q).unwrap();
    assert!(!p.partial);
    assert_eq!(seqs(&p), [(HitKind::Command, Some(2))]);
    // Short, wildcard and cased non-ASCII terms all take the scan.
    q.term = "_o".into();
    // `_` is literal: only `50%_off` matches, not every `o` after some character.
    assert_eq!(seqs(&r.search(&q).unwrap()), [(HitKind::Command, Some(2))]);
    q.term = "x_".into();
    assert!(r.search(&q).unwrap().hits.is_empty(), "literal underscore");
    q.term = "äR".into();
    assert_eq!(
        seqs(&r.search(&q).unwrap()),
        [(HitKind::Output, Some(3)), (HitKind::Command, Some(3))]
    );

    // The scan stops at its deadline and flags the page as partial.
    let mut b = IndexBatch::new(session(&env, U2));
    b.chunks = (0..3000)
        .map(|i| chunk(None, f64::from(i), &format!("line {i} {}", "x".repeat(200))))
        .collect();
    w.apply(&b).unwrap();
    r.set_short_scan(Duration::ZERO);
    q.term = "zz".into();
    let p = r.search(&q).unwrap();
    assert!(p.partial && p.hits.is_empty());
    // The deadline is gone afterwards.
    q.term = "needle".into();
    assert_eq!(r.search(&q).unwrap().hits.len(), 2);
}

#[test]
fn filters() {
    let env = Env::new();
    let mut w = env.writer();
    fixture(&env, w.as_mut());
    let mut s2 = session(&env, U2);
    s2.host = "h2".into();
    s2.user = "bob".into();
    s2.hidden = true;
    s2.tmux.pane = Some("%9".into());
    let mut b = IndexBatch::new(s2);
    let mut c = command(1, "grep needle other", 40.0);
    c.user = None;
    b.commands = vec![c];
    w.apply(&b).unwrap();
    let r = env.reader();

    let run = |f: SearchFilters| {
        let mut q = SearchQuery::new("needle");
        q.target = SearchTarget::All;
        q.filters = f;
        r.search(&q)
            .unwrap()
            .hits
            .iter()
            .map(|h| (h.session_uuid.clone(), h.kind))
            .collect::<Vec<_>>()
    };
    let all = SearchFilters {
        include_hidden: true,
        ..Default::default()
    };
    assert_eq!(run(SearchFilters::default()).len(), 2, "hidden session excluded");
    assert_eq!(run(all.clone()).len(), 3);
    let only_u2 = vec![(U2.to_owned(), HitKind::Command)];
    assert_eq!(
        run(SearchFilters {
            user: Some("bob".into()),
            ..all.clone()
        }),
        only_u2,
        "falls back to the session user"
    );
    assert_eq!(
        run(SearchFilters {
            host: Some("h2".into()),
            ..all.clone()
        }),
        only_u2
    );
    assert_eq!(
        run(SearchFilters {
            pane: Some("%9".into()),
            ..all.clone()
        }),
        only_u2
    );
    assert_eq!(
        run(SearchFilters {
            session: Some(U2.into()),
            ..all.clone()
        }),
        only_u2
    );
    assert_eq!(
        run(SearchFilters {
            since: Some(T0 + 35.0),
            ..all.clone()
        }),
        only_u2
    );
    assert_eq!(
        run(SearchFilters {
            until: Some(T0 + 10.0),
            ..all.clone()
        }),
        vec![(U1.to_owned(), HitKind::Command)]
    );
    assert_eq!(
        run(SearchFilters {
            cwd: Some("/home/alice".into()),
            ..SearchFilters::default()
        })
        .len(),
        2
    );

    let mut q = SearchQuery::new("");
    q.filters.exit = Some(ExitFilter::Failure);
    assert_eq!(seqs(&r.search(&q).unwrap()), [(HitKind::Command, Some(2))]);
    q.filters.exit = Some(ExitFilter::Code(0));
    assert_eq!(r.search(&q).unwrap().hits.len(), 2);
    q.filters.exit = Some(ExitFilter::Success);
    q.target = SearchTarget::Output;
    assert!(r.search(&q).unwrap().hits.is_empty(), "empty term has no output hits");
}

#[test]
fn empty_term_lists_newest_commands_with_pagination() {
    let env = Env::new();
    let mut w = env.writer();
    let mut b = IndexBatch::new(session(&env, U1));
    b.commands = (1..=7).map(|i| command(i, &format!("cmd {i}"), i as f64)).collect();
    // Same ts for two commands: the id breaks the tie.
    b.commands[6].ts_start = b.commands[5].ts_start;
    w.apply(&b).unwrap();
    let r = env.reader();
    let mut q = SearchQuery::new("");
    q.limit = 3;
    let mut seen = Vec::new();
    loop {
        let p = r.search(&q).unwrap();
        seen.extend(p.hits.iter().map(|h| h.cmd_seq.unwrap()));
        match p.next {
            Some(c) => q.cursor = Some(c.to_string().parse().unwrap()),
            None => break,
        }
    }
    assert_eq!(seen, [7, 6, 5, 4, 3, 2, 1]);
}

#[test]
fn mixed_pagination_matches_one_page() {
    let env = Env::new();
    let mut w = env.writer();
    let mut b = IndexBatch::new(session(&env, U1));
    b.commands = (1..=6)
        .map(|i| command(i, &format!("needle {i}"), i as f64 * 10.0))
        .collect();
    b.chunks = (1..=6)
        .map(|i| chunk(Some(i), i as f64 * 10.0, &format!("out needle {i}")))
        .collect();
    w.apply(&b).unwrap();
    let r = env.reader();
    let mut q = SearchQuery::new("needle");
    q.target = SearchTarget::All;
    let full: Vec<_> = r.search(&q).unwrap().hits.iter().map(|h| (h.kind, h.id)).collect();
    assert_eq!(full.len(), 12);
    assert_eq!(full[0].0, HitKind::Output, "same ts: output before command");
    q.limit = 5;
    let mut paged = Vec::new();
    loop {
        let p = r.search(&q).unwrap();
        paged.extend(p.hits.iter().map(|h| (h.kind, h.id)));
        match p.next {
            Some(c) => q.cursor = Some(c),
            None => break,
        }
    }
    assert_eq!(paged, full);
}

#[test]
fn candidate_cap() {
    let env = Env::new();
    let mut w = env.writer();
    let mut b = IndexBatch::new(session(&env, U1));
    b.commands = (1..=CANDIDATE_CAP as u64 + 1)
        .map(|i| command(i, &format!("needle {i}"), i as f64))
        .collect();
    w.apply(&b).unwrap();
    let r = env.reader();
    let p = search(r.as_ref(), "needle", SearchTarget::Commands);
    assert!(p.capped);
    assert_eq!(p.hits.len(), 200);
    assert_eq!(p.hits[0].cmd_seq, Some(CANDIDATE_CAP as u64 + 1));
    let p = search(r.as_ref(), "needle 17", SearchTarget::Commands);
    assert!(!p.capped);
}

/// §6.5: the LIKE and GLOB candidate queries are served by the trigram index, never by a
/// full scan of the virtual table, and the short-term scans walk the ts index in order.
#[test]
fn explain_query_plans() {
    let env = Env::new();
    drop(env.writer());
    let conn = env.raw();
    let plan = |sql: &str| -> Vec<String> {
        let mut st = conn.prepare(&format!("EXPLAIN QUERY PLAN {sql}")).unwrap();
        let n = st.parameter_count();
        let args: Vec<String> = (0..n).map(|_| "%needle%".to_owned()).collect();
        st.query_map(rusqlite::params_from_iter(args), |r| r.get::<_, String>(3))
            .unwrap()
            .map(|r| r.unwrap())
            .collect()
    };
    let trigram = regex::Regex::new(r"VIRTUAL TABLE INDEX \d+:[LG]\d").unwrap();
    let table_scan = regex::Regex::new(r"^SCAN [cos]( |$)").unwrap();
    for kind in [HitKind::Command, HitKind::Output] {
        for op in ["LIKE", "GLOB"] {
            for filters in ["", " WHERE s.hidden = 0 AND s.uuid = ?2"] {
                let p = plan(&candidate_sql(kind, op, filters));
                let fts: Vec<&String> = p.iter().filter(|l| l.contains("_fts VIRTUAL TABLE")).collect();
                assert!(!fts.is_empty(), "{kind:?} {op}: {p:?}");
                assert!(fts.iter().all(|l| trigram.is_match(l)), "{kind:?} {op}: {p:?}");
                if kind == HitKind::Command {
                    assert_eq!(fts.len(), 3, "one index lookup per column: {p:?}");
                }
                // The matches drive the join: rows are looked up by id, never scanned.
                assert!(
                    p.iter().all(|l| !table_scan.is_match(l)),
                    "{kind:?} {op} {filters:?}: {p:?}"
                );
            }
        }
        let p = plan(&scan_sql(kind, "LIKE", " AND s.hidden = 0"));
        assert!(
            p.iter().all(|l| !l.contains("TEMP B-TREE")),
            "{kind:?} scan sorts: {p:?}"
        );
        assert!(p.iter().any(|l| l.contains("_ts")), "{kind:?} scan: {p:?}");
    }
    // The defect the rules exist for: a LIKE on the table name is a full scan (`INDEX 0:`).
    let bad = plan("SELECT rowid FROM cmd_fts WHERE cmd_fts LIKE ?1");
    assert!(bad.iter().any(|l| l.ends_with("INDEX 0:")), "{bad:?}");
}

#[test]
fn upserts_provisional_and_finish() {
    let env = Env::new();
    let mut w = env.writer();
    let mut b = IndexBatch::new(session(&env, U1));
    let mut c = command(1, "tail -f log", 1.0);
    c.t_end = None;
    c.ts_end = None;
    c.exit = None;
    b.commands = vec![c.clone()];
    b.provisional = Some(vec![Chunk {
        kind: ChunkKind::Provisional,
        ..chunk(Some(1), 1.5, "first provisional")
    }]);
    w.apply(&b).unwrap();
    let r = env.reader();
    assert_eq!(
        search(r.as_ref(), "first provisional", SearchTarget::Output).hits.len(),
        1
    );

    // The next tick replaces the provisional rows; the closed command updates in place.
    let mut b = IndexBatch::new(session(&env, U1));
    c.t_end = Some(9.0);
    c.exit = Some(130);
    c.typed = "tail -F log".into();
    b.commands = vec![c];
    b.provisional = Some(vec![Chunk {
        kind: ChunkKind::Provisional,
        ..chunk(Some(1), 1.5, "second provisional")
    }]);
    b.chunks = vec![chunk(Some(1), 1.5, "final line")];
    w.apply(&b).unwrap();
    assert!(
        search(r.as_ref(), "first provisional", SearchTarget::Output)
            .hits
            .is_empty()
    );
    let p = search(r.as_ref(), "provisional", SearchTarget::Output);
    assert_eq!(p.hits[0].chunk_kind, Some(ChunkKind::Provisional));
    let p = search(r.as_ref(), "tail -", SearchTarget::Commands);
    assert_eq!(p.hits.len(), 1, "updated in place, not duplicated");
    assert_eq!(
        (p.hits[0].typed.as_deref(), p.hits[0].exit),
        (Some("tail -F log"), Some(130))
    );

    // Finishing clears provisional rows and ignores new ones.
    let mut b = IndexBatch::new(SessionMeta {
        state: SessionState::Ended,
        ended_at: Some(T0 + 10.0),
        ..session(&env, U1)
    });
    b.provisional = Some(vec![chunk(Some(1), 2.0, "late provisional")]);
    b.finished = true;
    w.apply(&b).unwrap();
    assert!(search(r.as_ref(), "provisional", SearchTarget::Output).hits.is_empty());
    assert_eq!(search(r.as_ref(), "final line", SearchTarget::Output).hits.len(), 1);
    assert_eq!(r.session(U1).unwrap().unwrap().state, SessionState::Ended);
    let raw = env.raw();
    assert_eq!(count(&raw, "SELECT count(*) FROM commands"), 1);
    fts_integrity(&raw);
}

#[test]
fn reset_and_replaced_cast() {
    let env = Env::new();
    let mut w = env.writer();
    fixture(&env, w.as_mut());
    let mut b = IndexBatch::new(session(&env, U1));
    b.reset = true;
    b.commands = vec![command(1, "echo fresh", 1.0)];
    w.apply(&b).unwrap();
    let r = env.reader();
    assert!(search(r.as_ref(), "needle", SearchTarget::All).hits.is_empty());
    assert_eq!(search(r.as_ref(), "fresh", SearchTarget::All).hits.len(), 1);

    // Another session now owns the same file: the old rows go.
    let mut b = IndexBatch::new(SessionMeta {
        cast_path: env.cast(&format!("{U1}.cast")),
        ..session(&env, U2)
    });
    b.commands = vec![command(1, "echo other", 1.0)];
    w.apply(&b).unwrap();
    assert!(r.session(U1).unwrap().is_none());
    assert!(search(r.as_ref(), "fresh", SearchTarget::All).hits.is_empty());
    fts_integrity(&env.raw());
}

#[test]
fn checkpoints_known_casts_and_segments() {
    let env = Env::new();
    let mut w = env.writer();
    let mut b = IndexBatch::new(session(&env, U1));
    let cp = Checkpoint {
        part: 2,
        byte_offset: 4096,
        t: 12.5,
        transcript: TranscriptState {
            vt_dump: "\u{1b}[1mdump".into(),
            evicted: 77,
            state: r#"{"rows":[1,2]}"#.into(),
        },
        open_stack: vec![OpenCommand {
            seq: 3,
            sh: 42,
            n: 9,
            depth: 0,
        }],
        pipeline_state: r#"{"clock":1}"#.into(),
        file: FileStamp {
            size: 8192,
            mtime: 1.25,
            ino: u64::MAX - 5,
        },
    };
    b.checkpoint = Some(cp.clone());
    b.parts = vec![
        CastPart {
            part: 1,
            path: env.cast("a.cast"),
            compressed: false,
            bytes: Some(10),
            t_offset: 0.0,
            frames: None,
        },
        CastPart {
            part: 2,
            path: env.cast("a.2.cast"),
            compressed: false,
            bytes: None,
            t_offset: 100.0,
            frames: None,
        },
    ];
    b.segments = vec![Segment {
        seq: 0,
        part: 1,
        t_start: 0.0,
        t_end: None,
        byte_offset: 0,
        vt_dump: String::new(),
    }];
    b.markers = vec![MarkerRow {
        t: 3.0,
        ts: T0 + 3.0,
        kind: "pause".into(),
        data_json: r#"{"cl":1,"k":"pause"}"#.into(),
    }];
    w.apply(&b).unwrap();
    assert_eq!(w.checkpoint(U1).unwrap(), Some(cp));
    assert_eq!(w.checkpoint(U2).unwrap(), None);

    let mut b = IndexBatch::new(session(&env, U1));
    b.segments = vec![Segment {
        seq: 0,
        part: 1,
        t_start: 0.0,
        t_end: Some(60.0),
        byte_offset: 0,
        vt_dump: String::new(),
    }];
    w.apply(&b).unwrap();
    let raw = env.raw();
    assert_eq!(count(&raw, "SELECT count(*) FROM segments WHERE t_end = 60"), 1);
    assert_eq!(count(&raw, "SELECT count(*) FROM markers"), 1);
    assert_eq!(count(&raw, "SELECT count(*) FROM cast_parts"), 2);

    let known = w.known_casts().unwrap();
    assert_eq!(known.len(), 1);
    let k = &known[0];
    assert_eq!((k.uuid.as_str(), k.state, k.legacy), (U1, SessionState::Live, false));
    assert_eq!((k.indexed_part, k.indexed_offset), (Some(2), Some(4096)));
    assert_eq!(k.indexed_file.unwrap().ino, u64::MAX - 5);
}

#[test]
fn compression_and_candidates() {
    let env = Env::new();
    let mut w = env.writer();
    let ended = |uuid: &str, at: f64| SessionMeta {
        state: SessionState::Ended,
        ended_at: Some(at),
        ..session(&env, uuid)
    };
    w.apply(&IndexBatch::new(ended(U1, 100.0))).unwrap();
    let mut legacy = ended(U2, 50.0);
    legacy.legacy = true;
    w.apply(&IndexBatch::new(legacy)).unwrap();
    let live = "01934f5e-7b2a-7c3d-8e4f-0123456789ef";
    w.apply(&IndexBatch::new(session(&env, live))).unwrap();

    assert!(w.compress_candidates(100.0).unwrap().is_empty(), "strictly before");
    let c = w.compress_candidates(101.0).unwrap();
    assert_eq!(c.len(), 1, "legacy and live are never compressed: {c:?}");
    assert_eq!((c[0].uuid.as_str(), c[0].part), (U1, 1));
    let from = c[0].path.clone();
    let mut to = from.clone().into_os_string();
    to.push(".zst");
    let to = PathBuf::from(to);
    w.record_compression(&CompressionUpdate {
        uuid: U1.into(),
        part: 1,
        from,
        to: to.clone(),
        uncompressed_bytes: 1000,
        compressed_bytes: 50,
        frames: vec![ZstFrameRow {
            seq: 0,
            c_offset: 0,
            c_len: 50,
            u_offset: 0,
            u_len: 1000,
        }],
    })
    .unwrap();
    assert!(w.compress_candidates(1e12).unwrap().is_empty());
    let known = w.known_casts().unwrap();
    let k = known.iter().find(|k| k.uuid == U1).unwrap();
    assert!(k.compressed);
    assert_eq!(k.cast_path, to);
    let raw = env.raw();
    assert_eq!(count(&raw, "SELECT count(*) FROM zst_frames"), 1);
    assert!(
        w.record_compression(&CompressionUpdate {
            uuid: U1.into(),
            part: 5,
            from: env.cast("x"),
            to: env.cast("y"),
            uncompressed_bytes: 0,
            compressed_bytes: 0,
            frames: Vec::new(),
        })
        .is_err()
    );
}

fn legacy_row(env: &Env, name: &str, status: LegacyStatus) -> LegacyImportRow {
    LegacyImportRow {
        path: env.cast(name),
        file: FileStamp {
            size: 100,
            mtime: 5.5,
            ino: 9,
        },
        sha256: "ab".repeat(32),
        commands_log: Some(env.cast("commands.log")),
        status,
        session_uuid: None,
        banners: 4,
        log_lines: 5,
        aligned: 3,
        mean_conf: Some(0.75),
        error: None,
    }
}

#[test]
fn legacy_rows_and_status() {
    use std::os::unix::ffi::OsStrExt;
    let env = Env::new();
    let mut w = env.writer();
    let mut s = SessionMeta {
        legacy: true,
        hidden: true,
        state: SessionState::Ended,
        ..session(&env, U1)
    };
    s.cast_path = env.cast("old.cast");
    let mut b = IndexBatch::new(s);
    b.legacy = Some(legacy_row(&env, "old.cast", LegacyStatus::Hidden));
    w.apply(&b).unwrap();
    // A non-UTF-8 file name keeps its bytes.
    let odd = PathBuf::from(std::ffi::OsStr::from_bytes(b"bad\xff.cast"));
    let mut failed = legacy_row(&env, "x", LegacyStatus::Failed);
    failed.path = env.paths.dir.join(&odd);
    failed.error = Some("unreadable header".into());
    w.record_legacy(&failed).unwrap();

    let got = w.legacy_entry(&env.cast("old.cast")).unwrap().unwrap();
    assert_eq!(got.session_uuid.as_deref(), Some(U1));
    assert_eq!(got.status, LegacyStatus::Hidden);
    assert_eq!(w.legacy_entry(&failed.path).unwrap(), Some(failed.clone()));
    assert!(w.legacy_entry(&env.cast("nope")).unwrap().is_none());

    // A live session with a partly indexed cast on disk.
    let live = SessionMeta {
        cast_path: env.cast("live.cast"),
        ..session(&env, U2)
    };
    std::fs::write(&live.cast_path, vec![b'x'; 1000]).unwrap();
    let mut b = IndexBatch::new(live);
    b.commands = vec![command(1, "ls", 1.0)];
    b.checkpoint = Some(Checkpoint {
        part: 1,
        byte_offset: 400,
        t: 1.0,
        transcript: TranscriptState::default(),
        open_stack: Vec::new(),
        pipeline_state: String::new(),
        file: FileStamp::default(),
    });
    w.apply(&b).unwrap();
    w.set_info(info_keys::LAST_ERROR, Some("boom")).unwrap();

    let r = env.reader();
    let report = r.legacy_report().unwrap();
    assert_eq!(report.len(), 2);
    assert!(report.windows(2).all(|p| p[0].path <= p[1].path));
    let st = r.status().unwrap();
    assert_eq!(
        (
            st.sessions.live,
            st.sessions.ended,
            st.sessions.legacy,
            st.sessions.hidden
        ),
        (1, 1, 1, 1)
    );
    assert_eq!((st.commands, st.outputs), (1, 0));
    assert_eq!(st.live.len(), 1);
    assert_eq!((st.live[0].indexed_bytes, st.live[0].file_bytes), (400, Some(1000)));
    assert!(st.live[0].updated_at.is_some());
    assert_eq!(st.backlog_bytes, 600);
    assert_eq!(
        (st.legacy.files, st.legacy.hidden, st.legacy.failed, st.legacy.banners),
        (2, 1, 1, 8)
    );
    assert_eq!(st.last_error.as_deref(), Some("boom"));
    assert!(st.info.contains_key(info_keys::WRITER_PID));
    assert!(st.info.contains_key(info_keys::WRITER_HEARTBEAT));
    assert!(st.index_bytes > 0);

    w.set_info(info_keys::LAST_ERROR, None).unwrap();
    assert_eq!(r.status().unwrap().last_error, None);
}

/// A legacy cast that grew is imported under a new uuid; the import from the shorter file
/// goes with its rows (same `cast_path`).
#[test]
fn a_grown_legacy_cast_replaces_its_earlier_session() {
    let env = Env::new();
    let mut w = env.writer();
    let import = |w: &mut Box<dyn IndexWriter>, uuid: &str, name: &str, typed: &str| {
        let mut s = SessionMeta {
            legacy: true,
            state: SessionState::Ended,
            ..session(&env, uuid)
        };
        s.cast_path = env.cast(name);
        let mut b = IndexBatch::new(s);
        b.commands = vec![command(1, typed, 1.0)];
        let mut row = legacy_row(&env, name, LegacyStatus::Imported);
        row.session_uuid = Some(uuid.to_owned());
        b.legacy = Some(row);
        b.finished = true;
        w.apply(&b).unwrap();
    };
    import(&mut w, U1, "old.cast", "first_version");
    import(&mut w, U3, "other.cast", "unrelated");
    import(&mut w, U2, "old.cast", "grown_version");
    let r = env.reader();
    assert!(r.session(U1).unwrap().is_none(), "the earlier import is gone");
    assert!(r.session(U2).unwrap().is_some());
    assert!(r.session(U3).unwrap().is_some(), "other paths are untouched");
    assert_eq!(
        search(r.as_ref(), "first_version", SearchTarget::Commands).hits.len(),
        0
    );
    assert_eq!(
        search(r.as_ref(), "grown_version", SearchTarget::Commands).hits.len(),
        1
    );
    let raw = env.raw();
    assert_eq!(count(&raw, "SELECT count(*) FROM sessions WHERE legacy = 1"), 2);
    fts_integrity(&raw);
}

#[test]
fn show_entries() {
    let env = Env::new();
    let mut w = env.writer();
    fixture(&env, w.as_mut());
    let mut b = IndexBatch::new(session(&env, U1));
    b.chunks = vec![
        chunk(Some(1), 10.8, "second chunk of one"),
        chunk(None, 15.0, "prompt between"),
    ];
    w.apply(&b).unwrap();
    let r = env.reader();

    let d = r
        .show(
            &EntryRef::Command {
                uuid: U1.into(),
                seq: 1,
            },
            1 << 20,
        )
        .unwrap()
        .unwrap();
    assert_eq!(d.command.as_ref().unwrap().typed, "grep needle file.txt");
    assert_eq!(d.command.as_ref().unwrap().pipestatus, [0]);
    assert_eq!(d.output, "file.txt: a NEEDLE in the haystack\nsecond chunk of one");
    assert!(!d.output_truncated);
    assert_eq!(d.session.commands, 3);
    assert_eq!(d.session.tmux.pane.as_deref(), Some("%3"));

    let d = r
        .show(
            &EntryRef::Command {
                uuid: U1.into(),
                seq: 1,
            },
            6,
        )
        .unwrap()
        .unwrap();
    assert_eq!((d.output.as_str(), d.output_truncated), ("file.t", true));

    // Inside a command: that command. Between commands: the gap span there.
    let d = r
        .show(
            &EntryRef::At {
                uuid: U1.into(),
                t: 20.7,
            },
            1 << 20,
        )
        .unwrap()
        .unwrap();
    assert_eq!(d.command.unwrap().seq, 2);
    let d = r
        .show(
            &EntryRef::At {
                uuid: U1.into(),
                t: 15.0,
            },
            1 << 20,
        )
        .unwrap()
        .unwrap();
    assert!(d.command.is_none());
    assert_eq!(d.output, "prompt between");
    let d = r
        .show(
            &EntryRef::At {
                uuid: U1.into(),
                t: 1.0,
            },
            1 << 20,
        )
        .unwrap()
        .unwrap();
    assert_eq!(d.output, "welcome banner");

    let d = r
        .show(&EntryRef::Session { uuid: U1.into() }, 1 << 20)
        .unwrap()
        .unwrap();
    assert!(d.output.starts_with("welcome banner\nfile.txt"));
    assert!(d.output.ends_with("viel ÄRGER hier"));

    assert!(
        r.show(
            &EntryRef::Command {
                uuid: U1.into(),
                seq: 99
            },
            10
        )
        .unwrap()
        .is_none()
    );
    assert!(r.show(&EntryRef::Session { uuid: U2.into() }, 10).unwrap().is_none());
}

#[test]
fn forget_flow() {
    let env = Env::new();
    let mut w = env.writer();
    fixture(&env, w.as_mut());
    let r = env.reader();
    let mut meta = env.meta();

    // Recorded in meta.db: the reader excludes it at once, before the writer purges.
    let f = meta
        .forget(
            &ForgetTarget::Command {
                uuid: U1.into(),
                seq: 1,
            },
            "alice",
        )
        .unwrap();
    assert_eq!(f.by, "alice");
    let again = meta
        .forget(
            &ForgetTarget::Command {
                uuid: U1.into(),
                seq: 1,
            },
            "bob",
        )
        .unwrap();
    assert_eq!(again, f, "idempotent");
    assert!(search(r.as_ref(), "needle", SearchTarget::All).hits.is_empty());
    assert!(
        r.show(
            &EntryRef::Command {
                uuid: U1.into(),
                seq: 1
            },
            100
        )
        .unwrap()
        .is_none()
    );
    let raw = env.raw();
    assert_eq!(count(&raw, "SELECT count(*) FROM commands"), 3, "not purged yet");

    let range = meta
        .forget(
            &ForgetTarget::Range {
                uuid: U1.into(),
                t_from: 20.0,
                t_to: 21.0,
            },
            "alice",
        )
        .unwrap();
    assert!(search(r.as_ref(), "50%", SearchTarget::All).hits.is_empty());
    let out = w.apply_forgotten(&[f.clone(), range.clone()]).unwrap();
    assert_eq!(
        out,
        ForgetOutcome {
            sessions: 0,
            commands: 2,
            outputs: 2
        }
    );
    assert_eq!(
        w.apply_forgotten(&[f.clone(), range.clone()]).unwrap(),
        ForgetOutcome::default()
    );

    // Re-indexing the same batch (rebuild) never brings it back.
    w.clear().unwrap();
    fixture(&env, w.as_mut());
    assert_eq!(count(&raw, "SELECT count(*) FROM commands"), 1);
    assert!(search(r.as_ref(), "needle", SearchTarget::All).hits.is_empty());
    assert_eq!(search(r.as_ref(), "ärger", SearchTarget::All).hits.len(), 2);

    // A whole session.
    let s = meta
        .forget(&ForgetTarget::Session { uuid: U1.into() }, "alice")
        .unwrap();
    assert!(r.session(U1).unwrap().is_none());
    assert!(search(r.as_ref(), "ärger", SearchTarget::All).hits.is_empty());
    let all = meta.forgotten().unwrap();
    assert_eq!(all.len(), 3);
    let out = w.apply_forgotten(&all).unwrap();
    assert_eq!(out.sessions, 1);
    assert_eq!(w.apply_forgotten(&all).unwrap(), ForgetOutcome::default(), "idempotent");
    assert_tombstone(&raw, U1);
    fixture(&env, w.as_mut());
    assert_tombstone(&raw, U1);
    fts_integrity(&raw);

    // A new writer picks the list up from meta.db by itself.
    drop(w);
    let mut w = env.writer();
    fixture(&env, w.as_mut());
    assert_tombstone(&raw, U1);
    assert_eq!(s.target.kind(), "session");

    assert!(
        meta.forget(
            &ForgetTarget::Range {
                uuid: U1.into(),
                t_from: 5.0,
                t_to: 1.0
            },
            "x"
        )
        .is_err()
    );
}

/// What a forgotten session leaves in index.db: one stripped session row, no content.
fn assert_tombstone(raw: &Connection, uuid: &str) {
    let (n, user, attribution, hidden, tmux, tty): (i64, String, String, bool, Option<String>, Option<String>) = raw
        .query_row(
            "SELECT count(*), user, attribution, hidden, tmux_pane, tty FROM sessions WHERE uuid = ?1",
            [uuid],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)),
        )
        .unwrap();
    assert_eq!(
        (n, user.as_str(), attribution.as_str(), hidden),
        (1, "", "forgotten", true)
    );
    assert_eq!((tmux, tty), (None, None));
    for table in ["commands", "outputs", "markers", "segments"] {
        assert_eq!(count(raw, &format!("SELECT count(*) FROM {table}")), 0, "{table}");
    }
    type State = (Option<Vec<u8>>, Option<String>, Option<String>);
    let state: Option<State> = raw
        .query_row(
            "SELECT i.vt_dump, i.open_stack, i.state_json FROM index_state i JOIN sessions s ON s.id = i.session_id
             WHERE s.uuid = ?1",
            [uuid],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .ok();
    if let Some(state) = state {
        assert_eq!(
            state,
            (None, None, None),
            "checkpoint keeps no terminal or pipeline state"
        );
    }
}

/// U1 with the prompts and echoes zsh writes before each `cmd` marker: a banner at 1, `echo
/// hello` at 10, `echo forgetmetoken` at 20 (its prompt at 11, the end of the one before)
/// and `bash` at 30 with a nested `echo forgetmetoken` at 35.
fn echo_fixture(env: &Env, w: &mut dyn IndexWriter) {
    let mut b = IndexBatch::new(session(env, U1));
    let mut bash = command(3, "bash", 30.0);
    bash.t_end = Some(40.0);
    let mut nested = command(4, "echo forgetmetoken", 35.0);
    nested.parent_seq = Some(3);
    nested.depth = 1;
    b.commands = vec![
        command(1, "echo hello", 10.0),
        command(2, "echo forgetmetoken", 20.0),
        bash,
        nested,
    ];
    b.chunks = vec![
        chunk(
            None,
            1.0,
            "banner: forgetmetoken is the word of the day\nalice@h1:~$ echo hello",
        ),
        chunk(Some(1), 10.5, "hello"),
        chunk(None, 11.0, "alice@h1:~$\nalice@h1:~$ echo forgetmetoken"),
        chunk(Some(2), 20.5, "forgetmetoken"),
        chunk(None, 21.0, "alice@h1:~$ bash"),
        chunk(Some(3), 30.5, "bash-5.2$ echo forgetmetoken    [rprompt]"),
        chunk(Some(4), 35.5, "forgetmetoken"),
    ];
    w.apply(&b).unwrap();
}

fn gap_texts(raw: &Connection) -> Vec<String> {
    let mut st = raw
        .prepare("SELECT text FROM outputs WHERE command_id IS NULL OR command_id = (SELECT id FROM commands WHERE seq = 3) ORDER BY t")
        .unwrap();
    st.query_map([], |r| r.get(0)).unwrap().map(Result::unwrap).collect()
}

/// §15.7: a command forget also removes the echoed command line, which lies in the output
/// before the command (unattributed, or the enclosing command's), from search and show.
#[test]
fn command_forget_removes_the_echo() {
    let env = Env::new();
    let mut w = env.writer();
    echo_fixture(&env, w.as_mut());
    let r = env.reader();
    let mut meta = env.meta();
    let hits = |r: &dyn Searcher| {
        let mut v: Vec<(HitKind, Option<Seq>, String)> = search(r, "forgetmetoken", SearchTarget::All)
            .hits
            .into_iter()
            .map(|h| (h.kind, h.cmd_seq, h.snippet.unwrap_or_default()))
            .collect();
        v.sort_by(|a, b| a.2.cmp(&b.2));
        v
    };
    assert_eq!(hits(r.as_ref()).len(), 7);
    let forgotten: Vec<Forgotten> = [2, 4]
        .into_iter()
        .map(|seq| {
            meta.forget(&ForgetTarget::Command { uuid: U1.into(), seq }, "alice")
                .unwrap()
        })
        .collect();

    // Excluded at once, before the purge: only the banner, which lies before the prompt.
    let banner = vec![(
        HitKind::Output,
        None,
        "banner: \u{E000}forgetmetoken\u{E001} is the word of the day".to_owned(),
    )];
    assert_eq!(hits(r.as_ref()), banner);
    let shown = r
        .show(&EntryRef::Session { uuid: U1.into() }, 1 << 20)
        .unwrap()
        .unwrap();
    assert_eq!(shown.output.matches("forgetmetoken").count(), 1, "{}", shown.output);
    assert!(shown.output.contains("echo hello") && shown.output.contains("alice@h1:~$ bash"));
    assert_eq!(search(r.as_ref(), "alice@h1", SearchTarget::Output).hits.len(), 3);

    let raw = env.raw();
    let out = w.apply_forgotten(&forgotten).unwrap();
    assert_eq!((out.commands, out.outputs), (2, 4), "two echoes, two outputs");
    let after = vec![
        "banner: forgetmetoken is the word of the day\nalice@h1:~$ echo hello".to_owned(),
        "alice@h1:~$\n".to_owned(),
        "alice@h1:~$ bash".to_owned(),
        String::new(),
    ];
    assert_eq!(gap_texts(&raw), after, "echoed lines emptied, ordinals kept");
    assert_eq!(hits(r.as_ref()), banner);
    fts_integrity(&raw);
    assert_eq!(
        w.apply_forgotten(&forgotten).unwrap(),
        ForgetOutcome::default(),
        "idempotent"
    );

    // A rebuild strips the echo on the way in.
    w.clear().unwrap();
    echo_fixture(&env, w.as_mut());
    assert_eq!(gap_texts(&raw), after);
    assert_eq!(hits(r.as_ref()), banner);
    fts_integrity(&raw);
}

#[test]
fn range_forget_catches_commands_that_close_later() {
    let env = Env::new();
    let mut w = env.writer();
    let mut b = IndexBatch::new(session(&env, U1));
    let mut c = command(1, "secret-tool lookup", 5.0);
    c.t_end = None;
    b.commands = vec![c.clone()];
    w.apply(&b).unwrap();
    let f = Forgotten {
        target: ForgetTarget::Range {
            uuid: U1.into(),
            t_from: 0.0,
            t_to: 10.0,
        },
        by: "me".into(),
        at: 0.0,
    };
    w.apply_forgotten(std::slice::from_ref(&f)).unwrap();
    let raw = env.raw();
    assert_eq!(count(&raw, "SELECT count(*) FROM commands"), 1, "still running");
    let mut b = IndexBatch::new(session(&env, U1));
    c.t_end = Some(6.0);
    b.commands = vec![c];
    b.chunks = vec![chunk(Some(1), 12.0, "after the range but owned by it")];
    w.apply(&b).unwrap();
    assert_eq!(count(&raw, "SELECT count(*) FROM commands"), 0);
    assert_eq!(count(&raw, "SELECT count(*) FROM outputs"), 0);
}

#[test]
fn clear_keeps_schema_and_info() {
    let env = Env::new();
    let mut w = env.writer();
    fixture(&env, w.as_mut());
    w.record_legacy(&legacy_row(&env, "a.cast", LegacyStatus::Skipped))
        .unwrap();
    w.set_info(info_keys::REBUILD_STARTED, Some("1")).unwrap();
    w.clear().unwrap();
    let raw = env.raw();
    for t in ["sessions", "commands", "outputs", "legacy_import"] {
        assert_eq!(count(&raw, &format!("SELECT count(*) FROM {t}")), 0, "{t}");
    }
    assert_eq!(
        count(&raw, "SELECT count(*) FROM sqlite_schema WHERE type = 'trigger'"),
        6
    );
    fts_integrity(&raw);
    let r = env.reader();
    assert_eq!(
        r.status()
            .unwrap()
            .info
            .get(info_keys::REBUILD_STARTED)
            .map(String::as_str),
        Some("1")
    );
    // Indexing works as before.
    fixture(&env, w.as_mut());
    assert_eq!(search(r.as_ref(), "needle", SearchTarget::All).hits.len(), 2);
    fts_integrity(&raw);
}

#[test]
fn redaction_is_off_unless_configured() {
    let env = Env::new();
    let mut w = env.writer();
    let mut b = IndexBatch::new(session(&env, U1));
    b.commands = vec![command(1, "mysql -pHunter2 db", 1.0)];
    b.chunks = vec![chunk(Some(1), 1.5, "Authorization: Bearer abc.def")];
    w.apply(&b).unwrap();
    let r = env.reader();
    assert_eq!(search(r.as_ref(), "Hunter2", SearchTarget::All).hits.len(), 1);
    assert_eq!(search(r.as_ref(), "Bearer abc", SearchTarget::All).hits.len(), 1);
    drop(w);

    // The optional user hook: their own patterns, applied before insert.
    let cfg = IndexConfig {
        redact: vec![r"-p\S+".into(), r"Bearer \S+".into()],
        ..IndexConfig::default()
    };
    let mut w = open_writer(&env.paths, &cfg).unwrap();
    let mut b = IndexBatch::new(session(&env, U2));
    b.commands = vec![command(1, "mysql -pHunter3 db", 1.0)];
    b.chunks = vec![chunk(Some(1), 1.5, "Authorization: Bearer xyz.uvw")];
    w.apply(&b).unwrap();
    assert!(search(r.as_ref(), "Hunter3", SearchTarget::All).hits.is_empty());
    let p = search(r.as_ref(), REDACTED, SearchTarget::All);
    assert_eq!(p.hits.len(), 2);
    drop(w);
    let bad = IndexConfig {
        redact: vec!["(".into()],
        ..IndexConfig::default()
    };
    assert!(matches!(open_writer(&env.paths, &bad), Err(IndexError::Invalid(_))));
}

fn annotation(uuid: &str, created: f64, tags: &[&str]) -> Annotation {
    Annotation {
        uuid: uuid.into(),
        session_uuid: U1.into(),
        cmd_seq: Some(2),
        t_from: Some(1.0),
        t_to: None,
        author: "alice".into(),
        body: format!("note {uuid}"),
        tags: tags.iter().map(|t| (*t).to_owned()).collect(),
        created_at: created,
        updated_at: created,
        deleted_at: None,
    }
}

#[test]
fn meta_settings_and_annotations() {
    let env = Env::new();
    let mut m = env.meta();
    assert_eq!(m.setting(settings_keys::PORT).unwrap(), None);
    m.set_setting(settings_keys::PORT, Some("10123")).unwrap();
    m.set_setting(settings_keys::PORT, Some("10124")).unwrap();
    assert_eq!(m.setting(settings_keys::PORT).unwrap().as_deref(), Some("10124"));
    m.set_setting(settings_keys::PORT, None).unwrap();
    assert_eq!(m.setting(settings_keys::PORT).unwrap(), None);

    m.put_annotation(&annotation("a2", 20.0, &["ops", "incident"])).unwrap();
    m.put_annotation(&annotation("a1", 10.0, &[])).unwrap();
    let mut a2 = annotation("a2", 20.0, &["ops", "later", "ops"]);
    a2.body = "edited".into();
    a2.deleted_at = Some(30.0);
    m.put_annotation(&a2).unwrap();
    let got = m.annotations(U1).unwrap();
    assert_eq!(got.iter().map(|a| a.uuid.as_str()).collect::<Vec<_>>(), ["a1", "a2"]);
    assert_eq!(got[1].body, "edited");
    assert_eq!(got[1].tags, ["later", "ops"]);
    assert_eq!(got[1].deleted_at, Some(30.0));
    assert!(m.annotations(U2).unwrap().is_empty());
    assert!(m.put_annotation(&annotation("", 1.0, &[])).is_err());

    // A second handle (another process) sees the same data.
    let other = env.meta();
    assert_eq!(other.annotations(U1).unwrap().len(), 2);
}

/// `find_annotations`, `annotation`, `tags` and `put_tag`, for any [`MetaStore`].
fn check_annotation_queries(m: &mut dyn MetaStore) {
    let mut a = |uuid: &str, session: &str, created: f64, tags: &[&str]| {
        let mut x = annotation(uuid, created, tags);
        x.session_uuid = session.into();
        m.put_annotation(&x).unwrap();
        x
    };
    a("a1", U1, 10.0, &["ops"]);
    a("a3", U1, 20.0, &["ops", "incident"]);
    a("a2", U1, 20.0, &[]);
    let mut gone = a("a4", U2, 30.0, &["ops"]);
    gone.deleted_at = Some(31.0);
    gone.author = "bob".into();
    m.put_annotation(&gone).unwrap();

    let uuids =
        |q: &AnnotationQuery| -> Vec<String> { m.find_annotations(q).unwrap().into_iter().map(|a| a.uuid).collect() };
    let all = AnnotationQuery::default();
    assert_eq!(uuids(&all), ["a1", "a2", "a3"], "creation order, uuid breaks ties");
    let q = AnnotationQuery {
        include_deleted: true,
        ..all.clone()
    };
    assert_eq!(uuids(&q), ["a1", "a2", "a3", "a4"]);
    let q = AnnotationQuery {
        sessions: vec![U2.into()],
        include_deleted: true,
        author: Some("bob".into()),
        ..all.clone()
    };
    assert_eq!(uuids(&q), ["a4"]);
    let q = AnnotationQuery {
        sessions: vec![U1.into(), U2.into()],
        tag: Some("ops".into()),
        ..all.clone()
    };
    assert_eq!(uuids(&q), ["a1", "a3"]);
    let q = AnnotationQuery {
        after: Some((20.0, "a2".into())),
        limit: 1,
        include_deleted: true,
        ..all.clone()
    };
    assert_eq!(uuids(&q), ["a3"], "keyset cursor and limit");
    let q = AnnotationQuery {
        cmd_seq: Some(3),
        ..all.clone()
    };
    assert!(uuids(&q).is_empty());

    assert_eq!(m.annotation("a4").unwrap().unwrap().deleted_at, Some(31.0));
    assert_eq!(m.annotation("nope").unwrap(), None);

    let tag = |name: &str, color: Option<&str>, count| TagInfo {
        name: name.into(),
        color: color.map(str::to_owned),
        count,
    };
    assert_eq!(m.tags().unwrap(), [tag("incident", None, 1), tag("ops", None, 2)]);
    assert_eq!(
        m.put_tag(" ops ", Some("#ff8800")).unwrap(),
        tag("ops", Some("#ff8800"), 2)
    );
    assert_eq!(m.put_tag("new", None).unwrap(), tag("new", None, 0));
    assert_eq!(
        m.put_tag("ops", None).unwrap(),
        tag("ops", None, 2),
        "None clears the colour"
    );
    assert!(m.put_tag("  ", None).is_err());
    assert_eq!(m.tags().unwrap().len(), 3);
}

#[test]
fn meta_annotation_queries_and_tags() {
    let env = Env::new();
    check_annotation_queries(env.meta().as_mut());
    check_annotation_queries(&mut crate::fake::MemIndex::new());
}

#[test]
fn meta_export_import_roundtrip_and_merge() {
    let env = Env::new();
    let mut m = env.meta();
    m.put_annotation(&annotation("a1", 10.0, &["ops"])).unwrap();
    m.forget(&ForgetTarget::Session { uuid: U2.into() }, "alice").unwrap();
    m.set_setting(settings_keys::PORT, Some("10123")).unwrap();
    let mut buf = Vec::new();
    let n = m.export_jsonl(&mut buf).unwrap();
    assert_eq!(n, 4, "tag, annotation, forgotten, setting");
    let text = String::from_utf8(buf.clone()).unwrap();
    for line in text.lines() {
        let v: serde_json::Value = serde_json::from_str(line).unwrap();
        assert!(v["table"].is_string() && v["row"].is_object(), "{line}");
    }

    let env2 = Env::new();
    let mut m2 = env2.meta();
    // A newer local edit wins over the older export.
    let mut newer = annotation("a1", 10.0, &["mine"]);
    newer.updated_at = 99.0;
    newer.body = "local".into();
    m2.put_annotation(&newer).unwrap();
    let extra = format!("{text}\n{{\"table\":\"auth_sessions\",\"row\":{{}}}}\n");
    assert_eq!(m2.import_jsonl(&mut extra.as_bytes()).unwrap(), 4);
    assert_eq!(m2.annotations(U1).unwrap()[0].body, "local");
    assert_eq!(m2.forgotten().unwrap().len(), 1);
    assert_eq!(m2.setting(settings_keys::PORT).unwrap().as_deref(), Some("10123"));
    // Importing twice changes nothing.
    m2.import_jsonl(&mut text.as_bytes()).unwrap();
    assert_eq!(m2.forgotten().unwrap().len(), 1);

    let env3 = Env::new();
    let mut m3 = env3.meta();
    m3.import_jsonl(&mut text.as_bytes()).unwrap();
    let mut again = Vec::new();
    m3.export_jsonl(&mut again).unwrap();
    // `at` of forgotten rows travels unchanged, so the exports are identical.
    assert_eq!(String::from_utf8(again).unwrap(), text);
    assert!(m3.import_jsonl(&mut "not json\n".as_bytes()).is_err());
}

#[test]
fn reader_without_meta_db_and_meta_created_later() {
    let env = Env::new();
    let mut w = env.writer();
    fixture(&env, w.as_mut());
    assert!(!env.paths.meta.exists());
    let r = env.reader();
    assert_eq!(search(r.as_ref(), "needle", SearchTarget::All).hits.len(), 2);
    let mut m = env.meta();
    m.forget(
        &ForgetTarget::Command {
            uuid: U1.into(),
            seq: 1,
        },
        "me",
    )
    .unwrap();
    assert!(search(r.as_ref(), "needle", SearchTarget::All).hits.is_empty());
}

#[test]
fn concurrent_reader_sees_commits() {
    let env = Env::new();
    let mut w = env.writer();
    fixture(&env, w.as_mut());
    let r = env.reader();
    let before = search(r.as_ref(), "", SearchTarget::Commands).hits.len();
    let handle = {
        let paths = env.paths.clone();
        std::thread::spawn(move || {
            let r = open_reader(&paths).unwrap();
            (0..50)
                .map(|_| r.search(&SearchQuery::new("")).unwrap().hits.len())
                .max()
                .unwrap()
        })
    };
    for i in 0..20 {
        let mut b = IndexBatch::new(session(&env, U1));
        b.commands = vec![command(100 + i, &format!("live {i}"), 100.0 + i as f64)];
        w.apply(&b).unwrap();
    }
    assert!(handle.join().unwrap() >= before);
    assert_eq!(search(r.as_ref(), "", SearchTarget::Commands).hits.len(), before + 20);
}

/// `n` commands `typed` at t = `t0`, `t0 + 1`, …, each with one output chunk `text`.
fn bulk(env: &Env, uuid: &str, n: u64, t0: f64, typed: &str, text: &str) -> IndexBatch {
    let mut b = IndexBatch::new(session(env, uuid));
    b.session.cast_path = env.cast(&format!("{uuid}.cast"));
    b.commands = (1..=n)
        .map(|i| command(i, &format!("{typed} {i}"), t0 + i as f64))
        .collect();
    b.chunks = (1..=n)
        .map(|i| chunk(Some(i), t0 + i as f64 + 0.5, &format!("{text} {i}")))
        .collect();
    b
}

/// Rowid order is not ts order: rows backfilled later (higher rowids) can be older. The
/// candidates are the newest by ts, not the highest rowids.
#[test]
fn candidates_are_newest_by_ts_not_rowid() {
    let env = Env::new();
    let mut w = env.writer();
    // Indexed first (low rowids) but newest.
    w.apply(&bulk(&env, U1, 3, 100_000.0, "needle new", "needle new out"))
        .unwrap();
    // Backfilled afterwards (high rowids) but older, more than the cap.
    w.apply(&bulk(
        &env,
        U2,
        CANDIDATE_CAP as u64 + 1,
        0.0,
        "needle old",
        "needle old out",
    ))
    .unwrap();
    let r = env.reader();
    for target in [SearchTarget::Commands, SearchTarget::Output] {
        let mut q = SearchQuery::new("needle");
        q.target = target;
        q.limit = 5;
        let p = r.search(&q).unwrap();
        assert!(p.capped, "{target:?}");
        let got: Vec<(&str, Option<Seq>)> = p.hits.iter().map(|h| (h.session_uuid.as_str(), h.cmd_seq)).collect();
        assert_eq!(
            got,
            [
                (U1, Some(3)),
                (U1, Some(2)),
                (U1, Some(1)),
                (U2, Some(5001)),
                (U2, Some(5000))
            ],
            "{target:?}"
        );
        // Pages continue in ts order across the candidate window.
        let mut q2 = q.clone();
        q2.cursor = p.next;
        let p2 = r.search(&q2).unwrap();
        assert_eq!(p2.hits[0].cmd_seq, Some(4999), "{target:?}");
    }
}

/// Filters apply before the cap: a filtered search finds its rows even when the term has
/// more than CANDIDATE_CAP matches elsewhere.
#[test]
fn filters_apply_before_the_candidate_cap() {
    let env = Env::new();
    let mut w = env.writer();
    w.apply(&bulk(&env, U1, 3, 0.0, "needle mine", "needle mine out"))
        .unwrap();
    w.apply(&bulk(
        &env,
        U2,
        CANDIDATE_CAP as u64 + 1,
        1000.0,
        "needle other",
        "needle other out",
    ))
    .unwrap();
    let r = env.reader();
    for target in [SearchTarget::Commands, SearchTarget::Output, SearchTarget::All] {
        let mut q = SearchQuery::new("needle");
        q.target = target;
        q.filters.session = Some(U1.into());
        let p = r.search(&q).unwrap();
        assert!(!p.capped, "{target:?}");
        assert!(p.hits.iter().all(|h| h.session_uuid == U1), "{target:?}");
        let want = if target == SearchTarget::All { 6 } else { 3 };
        assert_eq!(p.hits.len(), want, "{target:?}");

        // A time filter as well: only U1's rows are that old.
        let mut q = SearchQuery::new("needle");
        q.target = target;
        q.filters.until = Some(T0 + 10.0);
        let p = r.search(&q).unwrap();
        assert!(!p.capped && p.hits.len() == want, "{target:?}: {}", p.hits.len());
    }
    let p = search(r.as_ref(), "needle", SearchTarget::Commands);
    assert!(p.capped, "unfiltered, the cap still applies");
}

/// A chunk that starts before a forgotten range but holds lines written inside it is
/// forgotten too: excluded by the reader at once, blanked by the writer, and not revived by
/// a rebuild. Its neighbours stay, also after repeated purges.
#[test]
fn range_forget_covers_chunks_that_start_before_it() {
    let env = Env::new();
    let mut w = env.writer();
    let batch = |state: SessionState| {
        let mut b = IndexBatch::new(session(&env, U1));
        b.session.state = state;
        let mut c = command(1, "deploy", 5.0);
        c.t_end = Some(25.0);
        let mut running = command(2, "watch", 30.0);
        running.t_end = None;
        b.commands = vec![c, running];
        b.chunks = vec![
            chunk(Some(1), 5.0, "early lines"),
            chunk(Some(1), 8.0, "a secret token appears"),
            chunk(Some(1), 22.0, "late lines"),
            chunk(Some(2), 31.0, "watching for a secret"),
        ];
        b
    };
    w.apply(&batch(SessionState::Live)).unwrap();
    let mut meta = env.meta();
    let mut forget = |t_from, t_to| {
        meta.forget(
            &ForgetTarget::Range {
                uuid: U1.into(),
                t_from,
                t_to,
            },
            "alice",
        )
        .unwrap()
    };
    let ranges = [forget(10.0, 20.0), forget(40.0, 50.0)];
    let r = env.reader();
    let shown = |r: &dyn Searcher| {
        r.show(
            &EntryRef::Command {
                uuid: U1.into(),
                seq: 1,
            },
            1 << 20,
        )
        .unwrap()
        .unwrap()
        .output
    };
    let check = |r: &dyn Searcher| {
        assert!(search(r, "secret", SearchTarget::Output).hits.is_empty());
        assert_eq!(shown(r), "early lines\nlate lines");
        assert_eq!(search(r, "lines", SearchTarget::Output).hits.len(), 2);
    };
    check(r.as_ref());

    let raw = env.raw();
    let texts = |raw: &Connection| -> Vec<String> {
        raw.prepare("SELECT text FROM outputs ORDER BY t")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .map(|r| r.unwrap())
            .collect()
    };
    let out = w.apply_forgotten(&ranges).unwrap();
    // The running command's chunk is open: hidden, but blanked only once its end is known.
    assert_eq!(out.outputs, 1);
    assert_eq!(texts(&raw), ["early lines", "", "late lines", "watching for a secret"]);
    check(r.as_ref());
    assert_eq!(
        w.apply_forgotten(&ranges).unwrap(),
        ForgetOutcome::default(),
        "idempotent"
    );
    assert_eq!(texts(&raw)[0], "early lines", "no cascade to earlier chunks");

    // The session ends: the open chunk is now known to run into the second range.
    let mut end = IndexBatch::new(session(&env, U1));
    end.session.state = SessionState::Ended;
    w.apply(&end).unwrap();
    assert_eq!(texts(&raw), ["early lines", "", "late lines", ""]);
    fts_integrity(&raw);
    let fts_hits = count(&raw, "SELECT count(*) FROM out_fts WHERE text LIKE '%secret%'");
    assert_eq!(fts_hits, 0);

    // A rebuild stores the same.
    w.clear().unwrap();
    w.apply(&batch(SessionState::Ended)).unwrap();
    assert_eq!(texts(&raw), ["early lines", "", "late lines", ""]);
    check(r.as_ref());
    fts_integrity(&raw);
}

/// A forgotten session keeps a tombstone: discovery sees the cast as indexed up to the last
/// checkpoint, so it is not re-rendered on every walk, while nothing of its content (and no
/// terminal or pipeline state) is stored and readers never see it.
#[test]
fn forgotten_session_keeps_a_tombstone_for_discovery() {
    let env = Env::new();
    let mut meta = env.meta();
    meta.forget(&ForgetTarget::Session { uuid: U1.into() }, "alice")
        .unwrap();
    let mut w = env.writer();
    let stamp = FileStamp {
        size: 517,
        mtime: 3.5,
        ino: 99,
    };
    let mut b = IndexBatch::new(session(&env, U1));
    b.session.state = SessionState::Ended;
    b.session.ended_at = Some(T0 + 50.0);
    b.commands = vec![command(1, "secret-tool store", 1.0)];
    b.chunks = vec![chunk(Some(1), 1.5, "the secret")];
    b.segments = vec![Segment {
        seq: 0,
        part: 1,
        t_start: 0.0,
        t_end: None,
        byte_offset: 0,
        vt_dump: "screen with the secret".into(),
    }];
    b.checkpoint = Some(Checkpoint {
        part: 1,
        byte_offset: 517,
        t: 50.0,
        transcript: TranscriptState {
            vt_dump: "screen with the secret".into(),
            evicted: 3,
            state: "{}".into(),
        },
        open_stack: Vec::new(),
        pipeline_state: r#"{"secret":1}"#.into(),
        file: stamp,
    });
    // Applied twice, like two walks of an unchanged cast.
    w.apply(&b).unwrap();
    w.apply(&b).unwrap();
    fixture(&env, w.as_mut());

    let known = w.known_casts().unwrap();
    assert_eq!(known.len(), 1);
    let k = &known[0];
    assert_eq!(k.uuid, U1);
    assert_eq!(k.cast_path, env.cast(&format!("{U1}.cast")));
    assert_eq!(k.state, SessionState::Live, "the last batch's state");
    assert_eq!(
        (k.indexed_part, k.indexed_offset, k.indexed_file),
        (Some(1), Some(517), Some(stamp))
    );
    let cp = w.checkpoint(U1).unwrap().unwrap();
    assert_eq!((cp.byte_offset, cp.file), (517, stamp));
    assert!(cp.transcript.vt_dump.is_empty() && cp.pipeline_state.is_empty());

    let raw = env.raw();
    assert_tombstone(&raw, U1);
    let r = env.reader();
    assert!(r.session(U1).unwrap().is_none());
    assert!(r.show(&EntryRef::Session { uuid: U1.into() }, 100).unwrap().is_none());
    let mut q = SearchQuery::new("");
    q.filters.include_hidden = true;
    assert!(r.search(&q).unwrap().hits.is_empty());
    let st = r.status().unwrap();
    assert_eq!(st.sessions, SessionCounts::default(), "tombstones are not sessions");
    assert!(st.live.is_empty());
    fts_integrity(&raw);
}

/// Past the scan deadline before a stream starts, the stream is partial instead of an error.
#[test]
fn scan_deadline_before_the_second_stream() {
    let env = Env::new();
    let mut w = env.writer();
    fixture(&env, w.as_mut());
    let mut r = SqliteSearcher::open(&env.paths).unwrap();
    r.set_short_scan(Duration::ZERO);
    for target in [SearchTarget::All, SearchTarget::Output, SearchTarget::Commands] {
        let mut q = SearchQuery::new("ne");
        q.target = target;
        let p = r.search(&q).unwrap();
        assert!(p.partial, "{target:?}");
    }
    r.set_short_scan(SHORT_TERM_SCAN);
    let mut q = SearchQuery::new("ne");
    q.target = SearchTarget::All;
    assert!(!r.search(&q).unwrap().partial);
}

/// Timestamps from `now()` survive the JSON of meta exports bit for bit.
#[test]
fn microsecond_timestamps_roundtrip_through_json() {
    let base = now();
    assert_eq!((base * 1e6).round() / 1e6, base);
    for i in 0..20_000u32 {
        let at = ((base * 1e6).round() + f64::from(i) * 7919.0) / 1e6;
        let back: f64 = serde_json::from_str(&serde_json::to_string(&at).unwrap()).unwrap();
        assert_eq!(back.to_bits(), at.to_bits(), "{at}");
    }
}
