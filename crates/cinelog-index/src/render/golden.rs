//! avt dump golden test (plan §6.4 and Appendix B, M2): checkpoints and playback segments
//! rely on `Terminal::dump()` restoring every visible state, so for fixtures with SGR,
//! modes, the alt screen, origin mode and scroll regions, dump → restore → replay must
//! equal an uninterrupted replay at every split point. The dumps themselves are snapshots,
//! so an avt upgrade that changes them is noticed (§14 risk 7).

use avt::{Line, ScreenKind, Terminal};

const COLS: usize = 16;
const ROWS: usize = 6;

/// Output fragments; split points fall between them, also mid-sequence.
const FIXTURES: &[(&str, &[&str])] = &[
    (
        "sgr",
        &[
            "\x1b[1mbold\x1b[22m \x1b[3mit\x1b[23m \x1b[4mund\x1b[24m\r\n",
            "\x1b[38;5;196m256\x1b[39m \x1b[48;2;10;20;30mrgb\x1b[49m \x1b[7minv\x1b[27m\r\n",
            "\x1b[31;42mcolor \x1b[",
            "1;9mmixed\x1b[0m plain\r\n",
            // Ends in the pending-wrap state at the last column.
            "0123456789abcdef",
        ],
    ),
    (
        "alt-screen",
        &[
            "primary 1\r\nprimary 2\r\n\x1b[33mprompt$ \x1b[0m",
            "\x1b[?1049h\x1b[H\x1b[2J\x1b[44malt top\x1b[0m",
            "\x1b[3;5Halt mid\x1b7\x1b[6;1Halt bottom",
            "\x1b8 saved",
            "\x1b[?1049l",
            "back\r\n\x1b[?1047halt again",
        ],
    ),
    (
        "origin-scroll-region",
        &[
            "r1\r\nr2\r\nr3\r\nr4\r\nr5\r\nr6",
            "\x1b[2;5r\x1b[?6h\x1b[1;1Horigin",
            "\x1b[4;1H\n\n\nscrolled in region",
            "\x1b[2;1H\x1b[2L\x1b[1M\x1bM",
            "\x1b[3S\x1b[1T",
            "\x1b[?6l\x1b[r\x1b[6;1Hfull",
        ],
    ),
    (
        "modes-and-tabs",
        &[
            "\x1b[?1h\x1b=\x1b[?25l\x1b[4h",
            "ab\x1b[1Dz\x1b[4l\r\n",
            "\x1b[3g\x1b[5G\x1bH\x1b[12G\x1bH\r\tT1\tT2\r\n",
            "\x1b(0lqqk\x1b(B text\r\n",
            "\x1b[?7lno autowrap past the edge\x1b[?7h\r\n",
            "\x1b[2 q\x1b]0;title\x07",
            "\x1b]2;unfinished osc",
        ],
    ),
    (
        "wide-and-wrapped",
        &[
            "全角の文字列がここで折り返す\r\n",
            "a long line that wraps over several rows of this narrow screen\r\n",
            "x\x1b[K\x1b[1;14H全",
            "\x1bc\x1b[1mafter reset",
        ],
    ),
];

fn term() -> Terminal {
    Terminal::with_scrollback(COLS, ROWS, Some(0)).unwrap()
}

/// Everything observable about the visible state.
#[derive(Debug, PartialEq)]
struct Observed {
    kind: ScreenKind,
    cursor: avt::Cursor,
    primary: Vec<Line>,
    active: Vec<Line>,
    wrapped: Vec<bool>,
    app_keys: bool,
    dump: String,
}

fn observe(t: &Terminal) -> Observed {
    Observed {
        kind: t.active_screen_kind(),
        cursor: t.cursor(),
        primary: t.primary_screen().visible_lines().cloned().collect(),
        active: t.screen().visible_lines().cloned().collect(),
        wrapped: t.primary_screen().visible_lines().map(Line::is_wrapped).collect(),
        app_keys: t.cursor_key_app_mode(),
        dump: t.dump(),
    }
}

#[test]
fn dump_restore_replay_equals_one_pass() {
    for (name, frags) in FIXTURES {
        let mut one = term();
        let mut states = Vec::new();
        for f in *frags {
            one.feed(*f);
            states.push(observe(&one));
        }
        for split in 0..frags.len() {
            let mut a = term();
            for f in &frags[..split] {
                a.feed(*f);
            }
            let mut b = term();
            b.feed(a.dump().as_str());
            assert_eq!(b.dump(), a.dump(), "{name}: dump is stable across a restore at {split}");
            for (i, f) in frags.iter().enumerate().skip(split) {
                b.feed(*f);
                // Feeding a restored terminal and the original further keeps them equal.
                assert_eq!(
                    observe(&b),
                    states[i],
                    "{name}: restored at {split}, compared after {i}"
                );
            }
        }
    }
}

#[test]
fn dumps_are_golden() {
    for (name, frags) in FIXTURES {
        let mut t = term();
        let mut dumps = String::new();
        for f in *frags {
            t.feed(*f);
            dumps.push_str(&format!("{:?}\n", t.dump()));
        }
        insta::assert_snapshot!(format!("dump_{name}"), dumps);
    }
}
