use std::fs::File;
use std::io::BufWriter;

use cinelog_cast::{CastWriter, V3Writer};

use super::*;
use crate::api::IndexWriter;
use crate::fake::MemIndex;
use crate::model::ChunkKind;

const U: &str = "01934f5e-7b2a-7c3d-8e4f-0123456789ab";
const START_US: i64 = 1_731_070_000_000_000;
const BOOT_US: u64 = 5_000_000;
const STEM: &str = "h_20261108T101500.000000_456789ab";

struct CastFile {
    w: V3Writer<BufWriter<File>>,
    path: PathBuf,
}

fn session_json(part: u32, start_us: i64, boot_us: u64) -> String {
    format!(
        r#"{{"cl":1,"k":"session","uuid":"{U}","part":{part},"start_epoch_us":{start_us},"boottime_us":{boot_us},"tz":"UTC","scope":"personal","host":"h","user":"u","euid":1000,"attribution":"ancestor","tty":"/dev/pts/3","tmux":{{"socket":"/tmp/s","server_pid":42,"server_start":7,"session_id":"$1","pane":"%3"}}}}"#
    )
}

fn cmd(seq: u64, sh: u32, n: u64, typed: &str) -> String {
    format!(
        r#"{{"cl":1,"k":"cmd","seq":{seq},"sh":{sh},"n":{n},"typed":"{typed}","cwd":"/tmp","user":"u","euid":1000}}"#
    )
}

fn done(seq: u64, sh: u32, n: u64, exit: i32) -> String {
    format!(r#"{{"cl":1,"k":"done","seq":{seq},"sh":{sh},"n":{n},"exit":{exit},"pipestatus":[{exit}]}}"#)
}

impl CastFile {
    fn new(dir: &Path, part: u32, cols: u16, rows: u16, offset_us: u64) -> Self {
        let path = dir.join(cinelog_cast::part_file_name(STEM, part, false));
        let mut w = cinelog_cast::create(&path).unwrap();
        let start = START_US + offset_us as i64;
        w.write_header(&Header::new_v3(
            cols,
            rows,
            Some("xterm-256color".into()),
            start / 1_000_000,
            None,
        ))
        .unwrap();
        w.write_event(0, EventCode::Marker, &session_json(part, start, BOOT_US + offset_us))
            .unwrap();
        Self { w, path }
    }

    fn ev(&mut self, t: f64, code: EventCode, data: &str) -> &mut Self {
        self.w.write_event((t * 1e6).round() as u64, code, data).unwrap();
        self
    }

    fn o(&mut self, t: f64, data: &str) -> &mut Self {
        self.ev(t, EventCode::Output, data)
    }

    fn m(&mut self, t: f64, data: &str) -> &mut Self {
        self.ev(t, EventCode::Marker, data)
    }

    fn flush(&mut self) {
        self.w.flush().unwrap();
    }
}

fn factory() -> Box<dyn PipelineFactory> {
    crate::render::cast_pipelines(RenderOptions::default()).unwrap()
}

const ALL: PollBudget = PollBudget {
    max_bytes: u64::MAX,
    live: false,
};

/// Like the engine's backfill: poll to the end, then finish casts that have their `x`.
fn index(mem: &MemIndex, path: &Path, budget: PollBudget) {
    let mut w = mem.clone();
    let mut p = factory().open(path, None).unwrap();
    let mut finished = false;
    while let Some(b) = p.poll(budget).unwrap() {
        finished |= b.finished;
        w.apply(&b).unwrap();
    }
    if !finished && p.is_finished() {
        w.apply(&p.finish(SessionState::Ended).unwrap()).unwrap();
    }
}

/// A restart after every poll: each poll runs on a pipeline reopened from the last
/// committed checkpoint. `between` runs before every reopen.
fn index_restarting(mem: &MemIndex, path: &Path, budget: u64, mut between: impl FnMut(usize)) {
    let mut w = mem.clone();
    let mut cp: Option<Checkpoint> = None;
    for round in 0.. {
        between(round);
        let mut p = factory().open(path, cp.clone()).unwrap();
        let b = p
            .poll(PollBudget {
                max_bytes: budget,
                live: false,
            })
            .unwrap()
            .expect("a reopened pipeline always reports");
        assert!(!b.reset, "round {round}: checkpoint was not accepted");
        w.apply(&b).unwrap();
        let next = b.checkpoint.clone().unwrap();
        let stuck = cp
            .as_ref()
            .is_some_and(|c| (c.part, c.byte_offset) == (next.part, next.byte_offset));
        cp = Some(next);
        if b.finished {
            return;
        }
        if stuck {
            if p.is_finished() {
                w.apply(&p.finish(SessionState::Ended).unwrap()).unwrap();
            }
            return;
        }
    }
}

fn texts(mem: &MemIndex, kind: ChunkKind, seq: Option<u64>) -> Vec<String> {
    mem.chunks(U)
        .into_iter()
        .filter(|c| c.kind == kind && c.cmd_seq == seq)
        .map(|c| c.text)
        .collect()
}

#[test]
fn commands_output_and_session_row() {
    let d = tempfile::tempdir().unwrap();
    let mut c = CastFile::new(d.path(), 1, 40, 6, 0);
    c.o(0.1, "$ ")
        .ev(0.2, EventCode::Input, "l")
        .o(0.3, "ls\r\n")
        .m(0.4, &cmd(1, 100, 1, "ls"))
        .o(0.5, "a.txt\r\nb.txt\r\n")
        .m(0.6, &done(1, 100, 1, 0))
        .o(0.7, "$ ")
        .ev(1.0, EventCode::Exit, "0");
    c.flush();
    let mem = MemIndex::new();
    index(&mem, &c.path, ALL);
    let s = &mem.sessions()[0];
    assert_eq!(
        (s.uuid.as_str(), s.state, s.exit_status),
        (U, SessionState::Ended, Some(0))
    );
    assert_eq!(s.ended_at, Some(1_731_070_001.0));
    assert_eq!((s.cols, s.rows, s.parts), (40, 6, 1));
    assert_eq!(s.tmux.pane.as_deref(), Some("%3"));
    let cmds = mem.commands(U);
    assert_eq!(cmds.len(), 1);
    let ls = &cmds[0];
    assert_eq!(
        (ls.typed.as_str(), ls.exit, ls.t_start, ls.t_end),
        ("ls", Some(0), 0.4, Some(0.6))
    );
    assert_eq!((ls.ts_start, ls.ts_end), (1_731_070_000.4, Some(1_731_070_000.6)));
    assert_eq!(
        (ls.out_bytes, ls.out_truncated, ls.pipestatus.clone()),
        (10, false, vec![0])
    );
    assert_eq!(texts(&mem, ChunkKind::Primary, Some(1)), ["a.txt\nb.txt"]);
    assert_eq!(texts(&mem, ChunkKind::Gap, None), ["$ ls", "$"]);
    let out = mem.chunks(U);
    let first = out.iter().find(|c| c.cmd_seq == Some(1)).unwrap();
    assert_eq!((first.t, first.ts), (0.5, 1_731_070_000.5));
    let segs = mem.segments(U);
    assert_eq!(segs.len(), 1);
    assert_eq!(
        (segs[0].seq, segs[0].part, segs[0].t_start, segs[0].t_end),
        (0, 1, 0.0, Some(1.0))
    );
    assert!(segs[0].byte_offset > 0);
}

#[test]
fn nested_shells_and_missing_dones() {
    let d = tempfile::tempdir().unwrap();
    let mut c = CastFile::new(d.path(), 1, 40, 10, 0);
    c.m(1.0, &cmd(1, 100, 1, "zsh"))
        .o(1.1, "outer before\r\n")
        .m(2.0, &cmd(2, 200, 1, "echo inner"))
        .o(2.1, "inner\r\n")
        .m(2.2, &done(2, 200, 1, 3))
        .o(2.3, "outer after\r\n")
        .m(3.0, &cmd(3, 200, 2, "sleep 9"))
        .o(3.1, "lost\r\n")
        // The inner shell dies: only the outer `done` arrives.
        .m(4.0, &done(1, 100, 1, 130))
        .o(4.1, "$ ")
        .ev(5.0, EventCode::Exit, "0");
    c.flush();
    let mem = MemIndex::new();
    index(&mem, &c.path, ALL);
    let cmds = mem.commands(U);
    let row = |seq: u64| cmds.iter().find(|c| c.seq == seq).unwrap();
    assert_eq!((row(1).parent_seq, row(1).depth, row(1).exit), (None, 0, Some(130)));
    assert_eq!((row(2).parent_seq, row(2).depth, row(2).exit), (Some(1), 1, Some(3)));
    assert_eq!((row(3).parent_seq, row(3).depth, row(3).exit), (Some(1), 1, None));
    assert_eq!(row(3).t_end, Some(4.0));
    assert_eq!(
        texts(&mem, ChunkKind::Primary, Some(1)),
        ["outer before", "outer after"]
    );
    assert_eq!(texts(&mem, ChunkKind::Primary, Some(2)), ["inner"]);
    assert_eq!(texts(&mem, ChunkKind::Primary, Some(3)), ["lost"]);
    let first_outer: Vec<u64> = mem
        .chunks(U)
        .iter()
        .filter(|c| c.cmd_seq == Some(1))
        .map(|c| c.line0)
        .collect();
    assert_eq!(first_outer, [0, 1], "line ordinals continue when the owner recurs");
}

#[test]
fn markers_ctx_stats_and_clock() {
    let d = tempfile::tempdir().unwrap();
    let mut c = CastFile::new(d.path(), 1, 40, 6, 0);
    let step_epoch = START_US + 3_600_000_000 + 2_000_000;
    c.m(1.0, r#"{"cl":1,"k":"pause","reason":"user"}"#)
        .m(1.5, r#"{"cl":1,"k":"resume","reason":"user"}"#)
        .m(1.6, r#"{"cl":1,"k":"skip","sh":100}"#)
        .m(1.7, r#"{"cl":1,"k":"hello","sh":100,"sst":1}"#)
        .m(1.8, r#"{"cl":1,"k":"ctx","session_name":"work","window_index":"2","window_name":"vim","pane_index":"0","pane_id":"%3"}"#)
        .m(1.9, "a foreign label")
        .m(2.0, &format!(r#"{{"cl":1,"k":"clock","epoch_us":{step_epoch},"boottime_us":0,"reason":"step"}}"#))
        .m(2.5, &cmd(1, 100, 1, "date"))
        .m(2.6, r#"{"cl":1,"k":"stats","bad_markers":4,"lossy_bytes":2,"dropped_bytes":0,"parts":1}"#)
        .m(2.7, r#"{"cl":1,"k":"future","x":1}"#)
        .ev(3.0, EventCode::Exit, "130");
    c.flush();
    let mem = MemIndex::new();
    index(&mem, &c.path, ALL);
    let kinds: Vec<String> = mem.markers(U).into_iter().map(|m| m.kind).collect();
    assert_eq!(kinds, ["pause", "resume", "skip", "ctx", "clock", "stats", "future"]);
    let s = &mem.sessions()[0];
    assert_eq!(s.tmux.session.as_deref(), Some("work"));
    assert_eq!(s.tmux.window_name.as_deref(), Some("vim"));
    assert_eq!((s.counters.bad_markers, s.counters.lossy_bytes), (4, 2));
    assert_eq!(s.exit_status, Some(130));
    // The clock stepped by +1 h at t=2: later times follow the new anchor.
    let date = &mem.commands(U)[0];
    assert_eq!(date.ts_start, 1_731_073_602.5);
    assert_eq!(date.exit, None, "closed at the end without its done");
    assert_eq!(date.t_end, Some(3.0));
}

#[test]
fn alt_screen_programs_are_snapshotted() {
    let d = tempfile::tempdir().unwrap();
    let mut c = CastFile::new(d.path(), 1, 30, 5, 0);
    c.m(1.0, &cmd(1, 100, 1, "less f"))
        .o(1.1, "\x1b[?1049h\x1b[Hfirst page line")
        .o(15.0, "\x1b[2J\x1b[Hsecond page line")
        .o(16.0, "\x1b[?1049l")
        .m(16.1, &done(1, 100, 1, 0))
        .ev(17.0, EventCode::Exit, "0");
    c.flush();
    let mem = MemIndex::new();
    index(&mem, &c.path, ALL);
    assert_eq!(
        texts(&mem, ChunkKind::Alt, Some(1)),
        ["first page line", "second page line"]
    );
}

#[test]
fn live_polls_provisional_and_crash() {
    let d = tempfile::tempdir().unwrap();
    let mut c = CastFile::new(d.path(), 1, 30, 5, 0);
    c.m(1.0, &cmd(1, 100, 1, "make")).o(1.1, "building...");
    c.flush();
    let live = PollBudget {
        max_bytes: 1 << 20,
        live: true,
    };
    let mut p = factory().open(&c.path, None).unwrap();
    assert_eq!(p.uuid(), U);
    let b = p.poll(live).unwrap().unwrap();
    assert_eq!(b.session.state, SessionState::Live);
    assert_eq!(b.commands.len(), 1);
    assert!(b.commands[0].is_open());
    assert_eq!(b.parts.len(), 1);
    let prov = b.provisional.unwrap();
    assert_eq!(
        (prov[0].kind, prov[0].cmd_seq, prov[0].text.as_str()),
        (ChunkKind::Provisional, Some(1), "building...")
    );
    assert!(p.poll(live).unwrap().is_none(), "nothing new");

    c.o(2.0, " done\r\nlinking\r\n");
    c.flush();
    let b = p.poll(live).unwrap().unwrap();
    assert_eq!(b.provisional.unwrap()[0].text, "building... done\nlinking");
    assert!(b.commands.is_empty());
    assert!(!p.is_finished());

    let b = p.finish(SessionState::Crashed).unwrap();
    assert!(b.finished);
    assert_eq!(b.session.state, SessionState::Crashed);
    assert_eq!(b.session.ended_at, Some(1_731_070_002.0));
    assert_eq!(b.provisional, Some(vec![]));
    assert_eq!((b.commands[0].exit, b.commands[0].t_end), (None, Some(2.0)));
    assert_eq!(b.chunks[0].text, "building... done\nlinking");
    assert!(p.poll(live).unwrap().is_none());
}

#[test]
fn not_a_cinelog_cast() {
    let d = tempfile::tempdir().unwrap();
    let p = d.path().join("legacy.cast");
    std::fs::write(&p, "{\"version\":2,\"width\":80,\"height\":24}\n[0.5, \"o\", \"$ \"]\n").unwrap();
    assert!(matches!(factory().open(&p, None), Err(IndexError::Invalid(_))));
    assert!(factory().open(&d.path().join("missing.cast"), None).is_err());
}

/// A cast with everything that has state: wraps, colors, alt screen, scroll regions,
/// resizes, nested shells, clock markers and idle gaps that cut playback segments.
fn rich_cast(dir: &Path) -> PathBuf {
    let mut c = CastFile::new(dir, 1, 24, 6, 0);
    let mut t = 0.0;
    let mut seq = 0;
    for i in 0..40u64 {
        t += 0.5;
        c.o(t, &format!("\x1b[32mu@h\x1b[0m:~$ cmd{i}\r\n"));
        seq += 1;
        let (outer, sh, n) = (seq, 100, i + 1);
        c.m(t + 0.01, &cmd(outer, sh, n, &format!("cmd{i}")));
        match i % 5 {
            0 => {
                for k in 0..12 {
                    c.o(
                        t + 0.02 + f64::from(k) * 0.001,
                        &format!("output {i}.{k} with some longer text\r\n"),
                    );
                }
            }
            1 => {
                c.o(
                    t + 0.02,
                    "\x1b[?1049h\x1b[2;4r\x1b[H\x1b[1mtitle\x1b[0m\r\nbody\x1b[2;1H",
                );
                c.o(t + 11.0, "\x1b[Sscrolled\x1b[r\x1b[?1049l");
                t += 11.0;
            }
            2 => {
                seq += 1;
                c.m(t + 0.02, &cmd(seq, 200 + i as u32, 1, "inner"));
                c.o(
                    t + 0.03,
                    "inner out\r\n\x1b[?6h\x1b[2;5r\x1b[1;1Horigin\x1b[?6l\x1b[r\x1b[6;1H\r\n",
                );
                c.m(t + 0.04, &done(seq, 200 + i as u32, 1, 1));
            }
            3 => {
                c.ev(t + 0.02, EventCode::Resize, if i % 2 == 0 { "30x7" } else { "20x5" });
                c.o(t + 0.03, "after resize 全角 text that wraps around\r\n\x1b[4mpartial");
                c.o(t + 0.04, " line\x1b[0m\r\n");
            }
            _ => {
                t += 70.0;
                c.m(
                    t,
                    &format!(
                        r#"{{"cl":1,"k":"clock","epoch_us":{},"boottime_us":0}}"#,
                        START_US + (t * 1e6) as i64 + 1000
                    ),
                );
            }
        }
        t += 0.1;
        c.m(t, &done(outer, sh, n, (i % 3) as i32));
    }
    c.o(t + 0.1, "$ exit\r\n");
    c.ev(t + 0.2, EventCode::Exit, "0");
    c.flush();
    c.path.clone()
}

fn same_rows(a: &MemIndex, b: &MemIndex, with_session: bool) {
    assert_eq!(a.commands(U), b.commands(U));
    assert_eq!(a.chunks(U), b.chunks(U));
    assert_eq!(a.markers(U), b.markers(U));
    assert_eq!(a.segments(U), b.segments(U));
    if with_session {
        assert_eq!(a.sessions(), b.sessions());
    }
}

#[test]
fn resuming_after_every_poll_equals_one_pass() {
    let d = tempfile::tempdir().unwrap();
    let path = rich_cast(d.path());
    let one = MemIndex::new();
    index(&one, &path, ALL);
    assert!(one.commands(U).len() > 40);
    assert!(one.segments(U).len() >= 8, "idle gaps cut segments");
    assert!(one.chunks(U).iter().any(|c| c.kind == ChunkKind::Alt));
    for budget in [150, 1000, 7000] {
        let resumed = MemIndex::new();
        index_restarting(&resumed, &path, budget, |_| {});
        same_rows(&one, &resumed, true);
    }
    let small = MemIndex::new();
    index(
        &small,
        &path,
        PollBudget {
            max_bytes: 300,
            live: true,
        },
    );
    same_rows(&one, &small, true);
}

#[test]
fn playback_segments_restore_the_screen() {
    let d = tempfile::tempdir().unwrap();
    let path = rich_cast(d.path());
    let one = MemIndex::new();
    index(&one, &path, ALL);
    let segs = one.segments(U);
    // Replaying the cast from each segment's dump and offset reaches the same final screen.
    let full = replay(&path, None);
    for s in &segs {
        assert_eq!(replay(&path, Some(s)), full, "segment {}", s.seq);
    }
    for w in segs.windows(2) {
        assert_eq!(w[0].t_end, Some(w[1].t_start));
        assert!(w[0].byte_offset < w[1].byte_offset);
    }
}

/// Screen text after replaying `path` from the start, or from `seg`.
fn replay(path: &Path, seg: Option<&Segment>) -> Vec<String> {
    let mut r = match seg {
        None => cinelog_cast::open(path).unwrap(),
        Some(s) => cinelog_cast::open_at(
            path,
            ResumePoint {
                offset: s.byte_offset,
                t_us: (s.t_start * 1e6).round() as u64,
            },
            None,
        )
        .unwrap(),
    };
    let size = |t: &cinelog_cast::TermInfo| (usize::from(t.size().0), usize::from(t.size().1));
    let (mut cols, mut rows) = size(&r.header().term);
    let mut sizes = Vec::new();
    // The dump restores a terminal of the size at the cut: find it by replaying sizes.
    if let Some(s) = seg {
        let mut all = cinelog_cast::open(path).unwrap();
        while let Some(ev) = all.next_event().transpose().unwrap() {
            if ev.t_us as f64 / 1e6 > s.t_start {
                break;
            }
            if let (EventCode::Resize, Some((c, rr))) = (ev.code, parse_sane_resize(&ev.data)) {
                sizes.push((usize::from(c), usize::from(rr)));
            }
        }
        if let Some(&(c, rr)) = sizes.last() {
            (cols, rows) = (c, rr);
        }
    }
    let mut term = avt::Terminal::with_scrollback(cols, rows, Some(0)).unwrap();
    if let Some(s) = seg {
        term.feed(s.vt_dump.as_str());
    }
    while let Some(ev) = r.next_event().transpose().unwrap() {
        match ev.code {
            EventCode::Output => term.feed(ev.data.as_str()),
            EventCode::Resize => {
                if let Some((c, rr)) = parse_sane_resize(&ev.data) {
                    term.resize(usize::from(c), usize::from(rr));
                }
            }
            _ => {}
        }
    }
    term.screen().text()
}

#[test]
fn rotation_parts_are_one_session() {
    let d = tempfile::tempdir().unwrap();
    let mut p1 = CastFile::new(d.path(), 1, 30, 5, 0);
    p1.m(1.0, &cmd(1, 100, 1, "tail -f log")).o(1.1, "line one\r\n").m(
        2.0,
        &format!(r#"{{"cl":1,"k":"rotate","part":1,"next":"{STEM}_part2.cast","reason":"size"}}"#),
    );
    p1.flush();
    let mem = MemIndex::new();
    let mut w = mem.clone();
    let mut p = factory().open(&p1.path, None).unwrap();
    let live = PollBudget {
        max_bytes: 1 << 20,
        live: true,
    };
    w.apply(&p.poll(live).unwrap().unwrap()).unwrap();
    // The recorder rotates after the pipeline was opened.
    let mut p2 = CastFile::new(d.path(), 2, 40, 6, 10_000_000);
    p2.o(0.5, "line two\r\n")
        .m(1.0, &done(1, 100, 1, 0))
        .ev(2.0, EventCode::Exit, "0");
    p2.flush();
    while let Some(b) = p.poll(live).unwrap() {
        w.apply(&b).unwrap();
    }
    assert!(p.is_finished());
    let s = &mem.sessions()[0];
    assert_eq!(
        (s.parts, s.state, s.cast_path.clone()),
        (2, SessionState::Ended, p1.path.clone())
    );
    let c = &mem.commands(U)[0];
    assert_eq!((c.t_start, c.t_end), (1.0, Some(11.0)));
    assert_eq!(texts(&mem, ChunkKind::Primary, Some(1)), ["line one\nline two"]);
    let segs = mem.segments(U);
    assert_eq!(
        segs.iter().map(|s| s.part).collect::<Vec<_>>(),
        [1, 2],
        "a new part starts a segment"
    );

    // The same files indexed in one pass, and with restarts that cross the part boundary.
    let one = MemIndex::new();
    index(&one, &p1.path, ALL);
    same_rows(&mem, &one, false);
    let resumed = MemIndex::new();
    index_restarting(&resumed, &p1.path, 40, |_| {});
    same_rows(&one, &resumed, true);
}

#[test]
fn a_live_session_follows_the_continuation_part_after_a_disk_error() {
    // The recorder's ENOSPC layout: part 1 ends without `rotate` (or `error`) marker, and
    // `cinelog resume` opens part 2 with the `error` marker at its start.
    let d = tempfile::tempdir().unwrap();
    let mut p1 = CastFile::new(d.path(), 1, 30, 5, 0);
    p1.m(1.0, &cmd(1, 100, 1, "make")).o(1.1, "before\r\n");
    p1.flush();
    let mem = MemIndex::new();
    let mut w = mem.clone();
    let mut p = factory().open(&p1.path, None).unwrap();
    let live = PollBudget {
        max_bytes: 1 << 20,
        live: true,
    };
    w.apply(&p.poll(live).unwrap().unwrap()).unwrap();
    assert!(p.poll(live).unwrap().is_none(), "degraded: no new part yet");

    let mut p2 = CastFile::new(d.path(), 2, 30, 5, 10_000_000);
    p2.m(0.1, r#"{"cl":1,"k":"error","error":"enospc","degraded":true}"#)
        .o(0.2, "after\r\n")
        .m(0.3, &done(1, 100, 1, 0))
        .m(
            0.4,
            r#"{"cl":1,"k":"stats","bad_markers":0,"lossy_bytes":0,"dropped_bytes":9,"parts":2}"#,
        )
        .ev(0.5, EventCode::Exit, "0");
    p2.flush();
    let mut finished = false;
    while let Some(b) = p.poll(live).unwrap() {
        finished |= b.finished;
        w.apply(&b).unwrap();
    }
    assert!(finished && p.is_finished());
    let s = &mem.sessions()[0];
    assert_eq!((s.parts, s.state, s.exit_status), (2, SessionState::Ended, Some(0)));
    let c = &mem.commands(U)[0];
    assert_eq!((c.exit, c.t_end), (Some(0), Some(10.3)));
    assert_eq!(texts(&mem, ChunkKind::Primary, Some(1)), ["before\nafter"]);
    assert!(mem.markers(U).iter().any(|m| m.kind == "error"));

    let one = MemIndex::new();
    index(&one, &p1.path, ALL);
    same_rows(&mem, &one, false);
    let resumed = MemIndex::new();
    index_restarting(&resumed, &p1.path, 40, |_| {});
    same_rows(&one, &resumed, true);
}

#[test]
fn closed_commands_do_not_grow_the_checkpoint() {
    let d = tempfile::tempdir().unwrap();
    let mut c = CastFile::new(d.path(), 1, 30, 5, 0);
    let size_after = |c: &mut CastFile, from: u64, to: u64| {
        for n in from..to {
            let t = n as f64;
            c.o(t, "$ true\r\n")
                .m(t + 0.1, &cmd(n, 100, n, "true"))
                .o(t + 0.2, "x\r\n")
                .m(t + 0.3, &done(n, 100, n, 0));
        }
        c.flush();
        let mut p = factory().open(&c.path, None).unwrap();
        let mut last = None;
        while let Some(b) = p.poll(ALL).unwrap() {
            last = b.checkpoint;
        }
        last.unwrap().transcript.state.len()
    };
    let small = size_after(&mut c, 1, 11);
    let large = size_after(&mut c, 11, 2001);
    assert!(large < small + 64, "{small} -> {large} bytes");
}

#[test]
fn compression_between_restarts_is_transparent() {
    let d = tempfile::tempdir().unwrap();
    let path = rich_cast(d.path());
    let one = MemIndex::new();
    index(&one, &path, ALL);
    let resumed = MemIndex::new();
    let zst = path.with_file_name(format!("{STEM}.cast.zst"));
    index_restarting(&resumed, &path, 2000, |round| {
        if round == 5 {
            cinelog_cast::compress_file(&path, &zst, cinelog_cast::CompressOptions::default()).unwrap();
            std::fs::remove_file(&path).unwrap();
        }
    });
    same_rows(&one, &resumed, false);
    let s = &resumed.sessions()[0];
    assert!(s.compressed);
    assert_eq!(s.cast_path, zst);
}

/// `index --rebuild` over a compressed cast: the rows carry the uncompressed sizes and the
/// frame table the compressor recorded.
#[test]
fn a_compressed_cast_indexed_from_scratch_keeps_sizes_and_frames() {
    let d = tempfile::tempdir().unwrap();
    let path = rich_cast(d.path());
    let plain_len = std::fs::metadata(&path).unwrap().len();
    let plain = MemIndex::new();
    index(&plain, &path, ALL);
    let zst = path.with_file_name(format!("{STEM}.cast.zst"));
    let opts = cinelog_cast::CompressOptions {
        frame_bytes: 4096,
        ..Default::default()
    };
    let out = cinelog_cast::compress_file(&path, &zst, opts).unwrap();
    assert!(out.frames.len() > 1, "several frames");
    std::fs::remove_file(&path).unwrap();

    let rebuilt = MemIndex::new();
    index(
        &rebuilt,
        &zst,
        PollBudget {
            max_bytes: 3000,
            live: false,
        },
    );
    same_rows(&plain, &rebuilt, false);
    let s = &rebuilt.sessions()[0];
    assert!(s.compressed);
    assert_eq!(s.cast_bytes, Some(plain_len));
    let parts = rebuilt.parts(U);
    assert_eq!(parts.len(), 1);
    assert_eq!(parts[0].bytes, Some(plain_len));
    assert_eq!(
        rebuilt.frames(U, 1),
        Some(crate::model::ZstFrameRow::from_frames(&out.frames))
    );
}

#[test]
fn a_replaced_or_truncated_cast_starts_over() {
    let d = tempfile::tempdir().unwrap();
    let mut c = CastFile::new(d.path(), 1, 30, 5, 0);
    c.m(1.0, &cmd(1, 100, 1, "true"))
        .o(1.1, "x\r\n")
        .m(1.2, &done(1, 100, 1, 0));
    c.flush();
    let mut p = factory().open(&c.path, None).unwrap();
    let cp = p.poll(ALL).unwrap().unwrap().checkpoint.unwrap();
    drop(p);
    let resumed = factory()
        .open(&c.path, Some(cp.clone()))
        .unwrap()
        .poll(ALL)
        .unwrap()
        .unwrap();
    assert!(!resumed.reset);
    assert!(resumed.commands.is_empty(), "nothing new after the checkpoint");

    // Replaced by a shorter file with the same name.
    std::fs::remove_file(&c.path).unwrap();
    let mut c2 = CastFile::new(d.path(), 1, 30, 5, 0);
    c2.m(1.0, &cmd(1, 100, 1, "false"));
    c2.flush();
    let b = factory()
        .open(&c2.path, Some(cp.clone()))
        .unwrap()
        .poll(ALL)
        .unwrap()
        .unwrap();
    assert!(b.reset);
    assert_eq!(b.commands[0].typed, "false");

    // A checkpoint of another session is ignored too.
    let mut other = cp;
    other.pipeline_state = other.pipeline_state.replace(U, "01934f5e-0000-7000-8000-000000000000");
    assert!(
        factory()
            .open(&c2.path, Some(other))
            .unwrap()
            .poll(ALL)
            .unwrap()
            .unwrap()
            .reset
    );
}

#[test]
fn output_options_reach_the_pipeline() {
    let d = tempfile::tempdir().unwrap();
    let mut c = CastFile::new(d.path(), 1, 30, 5, 0);
    c.o(0.5, "$ x\r\n")
        .m(1.0, &cmd(1, 100, 1, "x"))
        .o(1.1, "out\r\n")
        .m(1.2, &done(1, 100, 1, 0));
    c.flush();
    let opts = RenderOptions {
        output: false,
        ..RenderOptions::default()
    };
    let mut p = crate::render::cast_pipelines(opts)
        .unwrap()
        .open(&c.path, None)
        .unwrap();
    let b = p.poll(ALL).unwrap().unwrap();
    assert_eq!(b.commands.len(), 1);
    assert!(b.chunks.is_empty());
    let b = p.finish(SessionState::Ended).unwrap();
    assert!(b.chunks.is_empty());
}
