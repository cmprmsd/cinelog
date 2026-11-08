//! I7 [R7]: `typed` is byte-exact for escapes, backslashes, UTF-8 and a 16 KiB heredoc; a
//! leading-space command leaves a `skip` marker but no command; forged markers printed by
//! a program are ignored (bad-nonce ones passed through, `-` ones stripped, none recorded)
//! and counted in the session's `bad_markers`.

use std::fs;

use cinelog_it::spoof::{cmd_json, marker_b64, marker_json, nested_start_json};
use cinelog_it::util::{random_hex, sh_quote};
use cinelog_it::{Need, Sandbox, need, scaled, unique_id};

#[test]
fn i7_typed_is_byte_exact() {
    need!(Need::Zsh, Need::Tmux, Need::Plugin, Need::Recorder);
    let sb = Sandbox::new().unwrap();
    let t = sb.tmux().unwrap();
    let r = sb.start_recorded(&t, "main").unwrap();
    let lines = [
        r#"printf "a\nb""#,
        r"grep '\d+' /dev/null",
        r"echo foo\cbar",
        r"echo C:\temp\new",
        r#"print -r -- "quote\" back\\slash \$HOME %d %s""#,
        "echo grüße 日本 ✓",
    ];
    for line in lines {
        let (cmd, done) = r.run(line).unwrap();
        assert_eq!(cmd.str("typed"), Some(line), "{}", cmd.json);
        assert!(!cmd.flag("trunc"));
        assert_eq!(cmd.u64("sh"), done.u64("sh"));
        assert_eq!(cmd.u64("n"), done.u64("n"));
        if line.starts_with("grep") {
            assert_eq!(done.i64("exit"), Some(1), "grep without a match exits 1");
        }
    }
    let c = r.cast().unwrap();
    let echo = c.cmd(r"echo C:\temp\new").unwrap();
    assert!(
        echo.str("expanded").is_some(),
        "expanded ($3) is recorded too: {}",
        echo.json
    );
    let seqs: Vec<u64> = c.markers_of("cmd").iter().filter_map(|m| m.u64("seq")).collect();
    assert!(
        seqs.windows(2).all(|w| w[0] < w[1]),
        "seq strictly increasing: {seqs:?}"
    );
}

#[test]
fn i7_sixteen_kib_heredoc_is_byte_exact() {
    need!(Need::Zsh, Need::Tmux, Need::Plugin, Need::Recorder);
    let sb = Sandbox::new().unwrap();
    let t = sb.tmux().unwrap();
    let r = sb.start_recorded(&t, "main").unwrap();
    let mut text = String::from("cat <<'EOF' > /dev/null\n");
    for i in 0..200 {
        text.push_str(&format!("{i:04} {}\n", random_hex(37)));
    }
    text.push_str("EOF");
    assert!(text.len() > 15 * 1024 && text.len() < 16 * 1024, "{}", text.len());
    r.pane.paste(&text).unwrap();
    r.pane.send_keys(&["Enter"]).unwrap();
    let (cmd, done) = r.wait_done(&text, 0, scaled(20.0)).unwrap();
    assert!(!cmd.flag("trunc"));
    assert_eq!(done.i64("exit"), Some(0));
    // Larger than BASE64_MAX_JSON: the plugin had to use enc=j, and nothing leaked.
    assert!(
        !r.cast().unwrap().output().contains("7777;cinelog"),
        "marker bytes reached the cast"
    );
}

#[test]
fn i7_leading_space_leaves_a_skip_marker_only() {
    need!(Need::Zsh, Need::Tmux, Need::Plugin, Need::Recorder);
    let sb = Sandbox::new().unwrap();
    let t = sb.tmux().unwrap();
    let r = sb.start_recorded(&t, "main").unwrap();
    let tag = unique_id();
    let skips_before = r.cast().unwrap().markers_of("skip").len();
    r.pane.type_line(&format!(" echo hidden-{tag}")).unwrap();
    r.pane
        .wait_line("hidden output", scaled(10.0), |l| l == format!("hidden-{tag}"))
        .unwrap();
    r.run("echo visible").unwrap();
    let c = r.cast().unwrap();
    let skips = c.markers_of("skip");
    assert_eq!(skips.len(), skips_before + 1, "one visible skip marker");
    assert!(skips.last().unwrap().get("typed").is_none(), "skip carries no text");
    for m in c.markers() {
        assert!(
            !m.json.to_string().contains(&tag),
            "hidden command text in a marker: {}",
            m.json
        );
    }
    // The recorder cannot unsee the output; it stays in the cast, unattributed.
    assert!(c.output().contains(&format!("hidden-{tag}")));
}

#[test]
fn i7_forged_markers_are_ignored() {
    need!(Need::Zsh, Need::Tmux, Need::Plugin, Need::Recorder);
    let sb = Sandbox::new().unwrap();
    let t = sb.tmux().unwrap();
    let r = sb.start_recorded(&t, "main").unwrap();
    let tag = unique_id();
    let fake = random_hex(16);
    let forged = [
        marker_b64(&fake, &cmd_json(1, 1, &format!("spoof-b-{tag}"))),
        marker_json(&fake, &cmd_json(1, 2, &format!("spoof-j-{tag}"))),
        marker_b64("-", &cmd_json(1, 3, &format!("spoof-dash-{tag}"))),
        marker_b64("-", &nested_start_json(&format!("0193-{tag}"))),
    ]
    .concat();
    let file = sb.work.join("forged.bin");
    fs::write(&file, format!("before-{tag}\n{forged}after-{tag}\n")).unwrap();
    r.run(&format!("cat {}", sh_quote(&file.to_string_lossy()))).unwrap();
    r.run("echo still recording").unwrap();

    let c = r.cast().unwrap();
    for m in c.markers() {
        assert!(!m.json.to_string().contains(&tag), "forged marker recorded: {}", m.json);
    }
    assert!(c.markers_of("nested").is_empty());
    assert!(
        c.markers_of("pause").is_empty(),
        "an uncorroborated nested-start must not pause"
    );
    let out = c.output();
    assert!(out.contains(&format!("before-{tag}")) && out.contains(&format!("after-{tag}")));
    // A wrong session nonce passes through unchanged (PROTOCOL §1.2).
    assert!(
        out.contains(&format!("\x1b]7777;cinelog;1;{fake};b;")),
        "bad-nonce marker must pass through to the output"
    );
    // `-` markers are accepted by the scanner, hence stripped, then dropped (§4.1, §4.5).
    assert!(
        !out.contains("\x1b]7777;cinelog;1;-;"),
        "a `-` forgery reached the output instead of being stripped"
    );

    // Counted: two bad-nonce markers, a `-` cmd and an uncorroborated `-` nested-start
    // (PROTOCOL §1.2, §4.1 step 4, §4.5). The session counters are published in the
    // `stats` marker the recorder writes before `x`.
    r.pane.type_line("exit").unwrap();
    r.wait_exit(cinelog_it::cast_timeout()).unwrap();
    let c = r.cast().unwrap();
    let stats = c.markers_of("stats");
    let stats = stats.last().expect("a stats marker before x");
    assert_eq!(stats.u64("bad_markers"), Some(4), "{}", stats.json);
}
