use super::*;

fn at(t: f64) -> Stamp {
    Stamp { t, ts: 1000.0 + t }
}

fn opts() -> RenderOptions {
    RenderOptions::default()
}

fn tr(cols: u16, rows: u16) -> AvtTranscript {
    AvtTranscript::new(cols, rows, opts()).unwrap()
}

fn view(chunks: &[Chunk]) -> Vec<(ChunkKind, Option<u64>, u64, f64, String)> {
    chunks
        .iter()
        .map(|c| (c.kind, c.cmd_seq, c.line0, c.t, c.text.clone()))
        .collect()
}

#[derive(Debug, Clone, Copy)]
enum Step {
    Out(f64, &'static str),
    Resize(f64, u16, u16),
    Boundary(f64, Owner),
}

fn apply(t: &mut dyn Transcript, s: Step) {
    match s {
        Step::Out(x, d) => t.output(at(x), d),
        Step::Resize(x, c, r) => t.resize(at(x), c, r),
        Step::Boundary(x, o) => t.boundary(at(x), o),
    }
}

const C1: Owner = Owner::Command(1);
const C2: Owner = Owner::Command(2);
const C3: Owner = Owner::Command(3);

/// Everything the transcript emits for `steps`, then `finish`, as (owner, text) pairs.
fn texts(t: &mut AvtTranscript, steps: &[Step]) -> Vec<(Option<u64>, String)> {
    let mut all = Vec::new();
    for s in steps {
        apply(t, *s);
        all.extend(t.take_chunks());
    }
    all.extend(t.finish(at(1e3)));
    all.into_iter().map(|c| (c.cmd_seq, c.text)).collect()
}

fn owned(v: &[(Option<u64>, &str)]) -> Vec<(Option<u64>, String)> {
    v.iter().map(|(o, t)| (*o, (*t).to_owned())).collect()
}

#[test]
fn prompt_command_and_output_are_attributed_at_the_cursor() {
    let mut t = tr(20, 5);
    t.output(at(0.0), "$ ls\r\n");
    t.boundary(at(1.0), C1);
    t.output(at(2.0), "a\r\n");
    t.output(at(2.5), "b\r\n");
    t.boundary(at(3.0), Owner::Gap);
    assert_eq!(t.owner(), Owner::Gap);
    t.output(at(4.0), "$ ");
    let mut all = t.take_chunks();
    all.extend(t.finish(at(5.0)));
    assert_eq!(
        view(&all),
        [
            (ChunkKind::Gap, None, 0, 0.0, "$ ls".into()),
            (ChunkKind::Primary, Some(1), 0, 2.0, "a\nb".into()),
            (ChunkKind::Gap, None, 1, 4.0, "$".into()),
        ]
    );
    assert_eq!(
        t.output_stats(C1),
        OutputStats {
            bytes: 2,
            lines: 2,
            truncated: false
        }
    );
}

#[test]
fn scrolled_rows_keep_their_first_write_time_and_split_by_lines() {
    let mut t = AvtTranscript::new(
        10,
        3,
        RenderOptions {
            chunk_lines: 4,
            ..opts()
        },
    )
    .unwrap();
    t.boundary(at(0.0), C1);
    for i in 1..=6 {
        t.output(at(f64::from(i)), &format!("{i}\r\n"));
    }
    let first = t.take_chunks();
    assert_eq!(
        view(&first),
        [(ChunkKind::Primary, Some(1), 0, 1.0, "1\n2\n3\n4".into())]
    );
    t.boundary(at(7.0), Owner::Gap);
    assert_eq!(
        view(&t.take_chunks()),
        [(ChunkKind::Primary, Some(1), 4, 5.0, "5\n6".into())]
    );
}

#[test]
fn soft_wraps_are_joined_across_eviction() {
    let mut t = tr(5, 2);
    t.boundary(at(0.0), C1);
    t.output(at(1.0), "abcdefghijkl\r\nnext\r\n");
    let chunks = t.finish(at(2.0));
    assert_eq!(
        view(&chunks),
        [(ChunkKind::Primary, Some(1), 0, 1.0, "abcdefghijkl\nnext".into())]
    );
}

#[test]
fn hard_reset_and_clear_keep_the_text() {
    let mut t = tr(10, 4);
    t.output(at(0.0), "a\r\nb");
    t.output(at(1.0), "\x1bc");
    t.output(at(2.0), "c\r\nd\r\n\x1b[H\x1b[2J\x1b[3J");
    t.output(at(3.0), "e");
    let chunks = t.finish(at(4.0));
    assert_eq!(view(&chunks), [(ChunkKind::Gap, None, 0, 0.0, "a\nb\nc\nd\ne".into())]);
}

#[test]
fn a_clear_that_homes_without_scrolling_keeps_later_output() {
    use Step::*;
    // `clear` for TERM=tmux-256color/screen: `\E[H\E[J` erases in place.
    let mut t = tr(40, 10);
    let got = texts(
        &mut t,
        &[
            Out(0.0, "$ echo hi\r\n"),
            Boundary(0.1, C1),
            Out(0.2, "hi\r\n"),
            Boundary(0.3, Owner::Gap),
            Out(1.0, "$ clear\r\n"),
            Boundary(1.1, C2),
            Out(1.2, "\x1b[H\x1b[J"),
            Boundary(1.3, Owner::Gap),
            Out(2.0, "$ ls\r\n"),
            Boundary(2.1, C3),
            Out(2.2, "file1\r\nfile2\r\n"),
            Boundary(2.3, Owner::Gap),
            Out(3.0, "$ "),
        ],
    );
    assert_eq!(
        got,
        owned(&[
            (None, "$ echo hi"),
            (Some(1), "hi"),
            (None, "$ clear"),
            (None, "$ ls"),
            (Some(3), "file1\nfile2"),
            (None, "$"),
        ])
    );
    // Rows that were emitted before the clear and scroll off later are not emitted twice.
    let mut t = tr(20, 4);
    let mut steps = vec![
        Out(0.0, "old1\r\nold2\r\nold3\r\n"),
        Boundary(0.5, C1),
        Out(1.0, "\x1b[H\x1b[J"),
        Out(1.1, "new1\r\n\r\nnew2\r\n"),
    ];
    steps.extend((0..4).map(|i| Out(2.0 + f64::from(i), "\r\n")));
    let got = texts(&mut t, &steps);
    assert_eq!(got, owned(&[(None, "old1\nold2\nold3"), (Some(1), "new1\n\nnew2")]));
}

#[test]
fn zsh_clear_screen_in_the_gap_and_cursor_up_redraws() {
    use Step::*;
    let mut t = tr(30, 6);
    let got = texts(
        &mut t,
        &[
            Out(0.0, "$ ls\r\n"),
            Boundary(0.1, C1),
            Out(0.2, "x\r\n"),
            Boundary(0.3, Owner::Gap),
            Out(1.0, "$ echo a"),
            // Ctrl-L in tmux: clear without scrolling, then the prompt and buffer again.
            Out(1.5, "\x1b[H\x1b[J$ echo a"),
            Out(1.6, "\r\n"),
            Boundary(1.7, C2),
            Out(1.8, "a\r\n"),
            // A done row printed over with the same text is not emitted again; with new
            // text it belongs to the span that wrote it.
            Out(1.85, "\x1b[1;1H$ echo a\x1b[3;1H"),
            Out(1.9, "\x1b[2;1Hb\x1b[3;1H"),
            Boundary(2.0, Owner::Gap),
            Out(2.1, "$ "),
        ],
    );
    assert_eq!(
        got,
        owned(&[
            (None, "$ ls"),
            (Some(1), "x"),
            (None, "$ echo a"),
            (Some(2), "b"),
            (None, "$"),
        ])
    );
}

#[test]
fn widening_resize_keeps_wrapped_prompts_and_pending_output() {
    use Step::*;
    let prompt = "$ ".to_owned() + &"p".repeat(28);
    let prompt: &'static str = Box::leak(format!("{prompt}\r\n").into_boxed_str());
    for (cols, rows) in [(40, 10), (40, 6), (25, 12)] {
        let mut t = tr(20, 10);
        let got = texts(
            &mut t,
            &[
                Out(0.0, prompt),
                Boundary(0.1, C1),
                Out(0.2, "out1\r\nout2\r\nout3\r\n"),
                Resize(0.3, cols, rows),
                Out(0.4, "out4\r\n"),
                Boundary(0.5, Owner::Gap),
                Out(0.6, "$ "),
            ],
        );
        let want_prompt = prompt.trim_end();
        assert_eq!(
            got,
            owned(&[(None, want_prompt), (Some(1), "out1\nout2\nout3\nout4"), (None, "$")]),
            "{cols}x{rows}"
        );
    }
}

#[test]
fn narrowing_resize_neither_duplicates_nor_loses_text() {
    use Step::*;
    let b30: &'static str = Box::leak(format!("{}\r\n", "B".repeat(30)).into_boxed_str());
    let mut t = tr(40, 6);
    let got = texts(
        &mut t,
        &[
            Out(0.0, "$ a\r\n"),
            Boundary(0.1, C1),
            Out(0.2, b30),
            Boundary(0.3, C2),
            Out(0.4, "pending"),
            Out(0.5, " line\r\n"),
            Resize(0.6, 10, 6),
            Out(0.7, "c2\r\n"),
            Resize(0.8, 7, 4),
            Out(0.9, "more\r\n"),
            Boundary(1.0, Owner::Gap),
            Out(1.1, "$ "),
        ],
    );
    let b = "B".repeat(30);
    assert_eq!(
        got,
        owned(&[
            (None, "$ a"),
            (Some(1), &b),
            (Some(2), "pending line\nc2\nmore"),
            (None, "$"),
        ])
    );
    // While a command is still pending, a resize under the alt screen reflows the primary.
    let mut t = tr(40, 6);
    let got = texts(
        &mut t,
        &[
            Out(0.0, "$ vim\r\n"),
            Boundary(0.1, C1),
            Out(0.2, b30),
            Out(0.3, "\x1b[?1049hvim ui"),
            Resize(0.4, 10, 6),
            Out(0.5, "\x1b[?1049l"),
            Boundary(0.6, Owner::Gap),
        ],
    );
    assert_eq!(got, owned(&[(None, "$ vim"), (Some(1), "vim ui"), (Some(1), &b)]));
}

#[test]
fn alt_screen_is_snapshotted_on_leave_periodically_and_deduplicated() {
    let mut t = tr(20, 4);
    t.boundary(at(0.0), C1);
    t.output(at(0.5), "\x1b[?1049h");
    assert!(t.is_alt_screen());
    t.output(at(1.0), "\x1b[Hfile one\r\nfile two");
    t.output(at(12.0), "\x1b[3;1Hthird");
    // 11.5 s after entering: the periodic snapshot runs before this event.
    let periodic = t.take_chunks();
    assert_eq!(
        view(&periodic),
        [(ChunkKind::Alt, Some(1), 0, 1.0, "file one\nfile two".into())]
    );
    t.output(at(13.0), "\x1b[4;1Hfourth");
    t.output(at(14.0), "\x1b[?1049l");
    assert!(!t.is_alt_screen());
    assert_eq!(
        view(&t.take_chunks()),
        [(ChunkKind::Alt, Some(1), 2, 12.0, "third\nfourth".into())]
    );
    // The primary screen is back and untouched by the alt content.
    t.output(at(15.0), "done\r\n");
    t.boundary(at(16.0), Owner::Gap);
    assert_eq!(
        view(&t.take_chunks()),
        [(ChunkKind::Primary, Some(1), 0, 15.0, "done".into())]
    );
}

#[test]
fn alt_screen_is_snapshotted_at_boundaries_and_per_span() {
    let mut t = tr(20, 4);
    t.boundary(at(0.0), C1);
    t.output(at(1.0), "\x1b[?1049hsame");
    t.boundary(at(2.0), C2);
    t.output(at(3.0), "\x1b[2;1Hother");
    t.output(at(4.0), "\x1b[?1049l");
    let chunks = t.take_chunks();
    assert_eq!(
        view(&chunks),
        [
            (ChunkKind::Alt, Some(1), 0, 1.0, "same".into()),
            // A new span de-duplicates afresh.
            (ChunkKind::Alt, Some(2), 0, 1.0, "same\nother".into()),
        ]
    );
    let mut off = AvtTranscript::new(
        20,
        4,
        RenderOptions {
            alt_snapshots: false,
            ..opts()
        },
    )
    .unwrap();
    off.output(at(0.0), "\x1b[?1049hhidden\x1b[?1049l");
    assert!(off.finish(at(1.0)).is_empty());
}

#[test]
fn resize_forces_a_chunk_boundary() {
    let mut t = tr(10, 2);
    t.boundary(at(0.0), C1);
    t.output(at(1.0), "a\r\nb\r\n");
    t.resize(at(2.0), 10, 2);
    assert!(t.take_chunks().is_empty(), "same size is no resize");
    t.resize(at(2.0), 12, 3);
    assert_eq!(t.size(), (12, 3));
    t.output(at(3.0), "c\r\n");
    let mut all = t.take_chunks();
    all.extend(t.finish(at(4.0)));
    let texts: Vec<_> = all.iter().map(|c| c.text.as_str()).collect();
    assert_eq!(texts, ["a", "b\nc"]);
}

#[test]
fn output_switches_and_cap() {
    let gap_off = RenderOptions {
        gap_output: false,
        ..opts()
    };
    let mut t = AvtTranscript::new(20, 3, gap_off).unwrap();
    t.output(at(0.0), "prompt\r\n");
    t.boundary(at(1.0), C1);
    t.output(at(2.0), "kept\r\n");
    let chunks = t.finish(at(3.0));
    assert_eq!(view(&chunks), [(ChunkKind::Primary, Some(1), 0, 2.0, "kept".into())]);

    let capped = RenderOptions {
        max_output_per_command: 10,
        ..opts()
    };
    let mut t = AvtTranscript::new(20, 3, capped).unwrap();
    t.boundary(at(0.0), C1);
    t.output(at(1.0), "12345\r\n67890\r\nmore\r\n");
    t.boundary(at(2.0), Owner::Gap);
    assert_eq!(t.take_chunks()[0].text, "12345\n67890");
    assert_eq!(
        t.output_stats(C1),
        OutputStats {
            bytes: 10,
            lines: 2,
            truncated: true
        }
    );

    let mut t = AvtTranscript::new(
        20,
        3,
        RenderOptions {
            output: false,
            ..opts()
        },
    )
    .unwrap();
    t.output(at(0.0), "x\r\ny\r\nz\r\n");
    assert!(t.finish(at(1.0)).is_empty());
}

#[test]
fn provisional_shows_the_running_command() {
    let mut t = tr(20, 4);
    t.output(at(0.0), "$ make\r\n");
    assert!(t.provisional().iter().all(|c| c.cmd_seq.is_none()));
    t.boundary(at(1.0), C1);
    assert!(t.provisional().is_empty(), "nothing written yet");
    t.output(at(2.0), "step 1\r\nstep 2\r\nstep 3\r\nstep 4");
    let p = t.provisional();
    assert_eq!(
        view(&p),
        [(
            ChunkKind::Provisional,
            Some(1),
            0,
            2.0,
            "step 1\nstep 2\nstep 3\nstep 4".into()
        )]
    );
    t.boundary(at(3.0), Owner::Gap);
    // The unterminated cursor row belongs to the next span (trait contract).
    assert_eq!(
        view(&t.provisional()),
        [(ChunkKind::Provisional, None, 1, 2.0, "step 4".into())]
    );
    let texts: Vec<_> = t.take_chunks().into_iter().map(|c| c.text).collect();
    assert_eq!(texts, ["$ make", "step 1\nstep 2\nstep 3"]);
}

#[test]
fn lines_above_cursor_reach_into_scrolled_rows() {
    let mut t = tr(10, 3);
    t.output(at(0.0), "line1\r\nline2\r\n$ cmd\r\n");
    assert_eq!(t.lines_above_cursor(2), ["line2", "$ cmd"]);
    assert_eq!(t.lines_above_cursor(9), ["line1", "line2", "$ cmd"]);
    assert!(t.lines_above_cursor(0).is_empty());
    let mut w = tr(4, 2);
    w.output(at(0.0), "abcdefgh\r\n");
    assert_eq!(w.lines_above_cursor(1), ["abcdefgh"]);
}

/// Feeds `steps` once; then again with a snapshot/restore before every step, and checks
/// that chunks, stats and the final terminal are identical.
fn assert_resumable(cols: u16, rows: u16, steps: &[Step]) {
    let o = RenderOptions {
        chunk_lines: 3,
        alt_snapshot_secs: 2.0,
        ..opts()
    };
    let mut one = AvtTranscript::new(cols, rows, o).unwrap();
    let mut want = Vec::new();
    for s in steps {
        apply(&mut one, *s);
        want.extend(one.take_chunks());
    }
    let end = at(1e3);
    let want_stats: Vec<_> = [Owner::Gap, C1, C2].map(|w| one.output_stats(w)).to_vec();
    let want_lines = one.lines_above_cursor(5);
    let want_dump = one.vt_dump();
    want.extend(one.finish(end));

    for split in 0..=steps.len() {
        let mut a: Box<dyn Transcript> = Box::new(AvtTranscript::new(cols, rows, o).unwrap());
        let mut got = Vec::new();
        for (i, s) in steps.iter().enumerate() {
            if i == split {
                let snap = a.snapshot();
                a = Box::new(AvtTranscript::restore(&snap, o).unwrap());
            }
            apply(a.as_mut(), *s);
            got.extend(a.take_chunks());
        }
        if split == steps.len() {
            let snap = a.snapshot();
            let r = AvtTranscript::restore(&snap, o).unwrap();
            assert_eq!(r.vt_dump(), want_dump, "dump after restore");
            a = Box::new(r);
        }
        let stats: Vec<_> = [Owner::Gap, C1, C2].map(|w| a.output_stats(w)).to_vec();
        assert_eq!(stats, want_stats, "split {split}");
        assert_eq!(a.lines_above_cursor(5), want_lines, "split {split}");
        got.extend(a.finish(end));
        assert_eq!(got, want, "split {split}");
    }
}

#[test]
fn snapshot_restore_equals_one_pass() {
    use Step::*;
    assert_resumable(
        12,
        4,
        &[
            Out(0.0, "\x1b[1;31m$ \x1b[0mvim f\r\n"),
            Boundary(0.5, C1),
            Out(1.0, "\x1b[?1049h\x1b[Hline one\r\nline two"),
            Out(4.0, "\x1b[2;3r\x1b[2;1H\x1b[S\x1b[r"),
            Out(5.0, "\x1b[?1049l"),
            Out(5.5, "wrapping output that is long\r\n"),
            Boundary(6.0, C2),
            Out(6.5, "\x1b[?6h\x1b[2;3r\x1b[1;1Horigin\x1b[?6l\x1b[r\x1b[4;1H"),
            Out(7.0, "全角文字の行です\r\n\x1b[4mund"),
            Resize(8.0, 9, 5),
            Out(8.5, "\x1b[H\x1b[Jcleared\r\n"),
            Out(9.0, "erline\r\n\x1b7\x1b[1;1Hsaved\x1b8tail\x1b]0;title"),
            Out(9.5, "\x07 x\x1b[38;5;196mred\x1b[48;2;1;2;3mrgb\r\n"),
            Boundary(10.0, Owner::Gap),
            Out(11.0, "$ \x1b["),
            Out(11.5, "1mbold"),
        ],
    );
}

#[test]
fn restore_rejects_inconsistent_state() {
    let t = tr(10, 3);
    let mut snap = t.snapshot();
    snap.evicted = 5;
    assert!(AvtTranscript::restore(&snap, opts()).is_err());
    snap.state = "{".into();
    assert!(AvtTranscript::restore(&snap, opts()).is_err());
}

#[test]
fn transcript_is_send() {
    fn send<T: Send>() {}
    send::<AvtTranscript>();
}

#[test]
fn one_large_event_and_endless_lines() {
    let o = RenderOptions {
        chunk_bytes: 64,
        ..opts()
    };
    let mut t = AvtTranscript::new(10, 3, o).unwrap();
    t.boundary(at(0.0), C1);
    let many: String = (0..50).map(|i| format!("row {i}\r\n")).collect();
    t.output(at(1.0), &many);
    // 200 characters without a newline: cut into chunk-sized lines.
    t.output(at(2.0), &"x".repeat(200));
    t.output(at(3.0), "\r\n");
    let mut all = t.take_chunks();
    all.extend(t.finish(at(4.0)));
    let text: String = all.iter().map(|c| c.text.as_str()).collect::<Vec<_>>().join("\n");
    let rows: Vec<String> = (0..50).map(|i| format!("row {i}")).collect();
    assert!(text.starts_with(&rows.join("\n")));
    assert!(all.iter().all(|c| c.text.len() <= 64 + 10));
    assert_eq!(text.matches('x').count(), 200);
    assert_eq!(all[0].t, 1.0);
    assert_eq!(
        t.output_stats(C1).lines,
        53,
        "50 rows and the 200 x in three 70-byte pieces"
    );
}

/// Regression: `stty cols 65535 rows 65535` in a recorded shell made the indexer allocate a
/// 65535x65535 terminal (OOM abort of the daemon), and a legacy cast's resize to 3350 columns
/// rendered at 1000. Implausible sizes are dropped like the web player drops them; a header's
/// becomes the fallback size.
#[test]
fn implausible_terminal_sizes_are_dropped() {
    use cinelog_cast::{FALLBACK_SIZE, MAX_COLS, MAX_ROWS};
    let mut t = tr(u16::MAX, u16::MAX);
    assert_eq!(t.size(), FALLBACK_SIZE);
    t.resize(at(0.5), 120, 40);
    assert_eq!(t.size(), (120, 40));
    for (c, r) in [
        (u16::MAX, 3),
        (3350, 40),
        (MAX_COLS + 1, 40),
        (120, MAX_ROWS + 1),
        (0, 0),
        (80, 0),
    ] {
        t.resize(at(1.0), c, r);
        assert_eq!(t.size(), (120, 40), "{c}x{r}");
    }
    t.resize(at(2.0), MAX_COLS, MAX_ROWS);
    assert_eq!(t.size(), (MAX_COLS, MAX_ROWS));
    t.resize(at(3.0), 80, 24);
    t.output(at(4.0), "still fine\r\n");
    let all: Vec<String> = t.finish(at(5.0)).into_iter().map(|c| c.text).collect();
    assert!(all.iter().any(|s| s.contains("still fine")), "{all:?}");
}
