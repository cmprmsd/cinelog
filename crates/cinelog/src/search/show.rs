//! `cinelog show <uuid>[:<seq>|@<t>]`: a command (or session) header and its output text.
//! Also the fzf preview of `search`.

use std::fmt::Write as _;
use std::io::Write;

use cinelog_index::model::{Command, EntryDetail, EntryRef, SessionSummary};

use super::text::{self, BOLD, DIM, RED, YELLOW};
use super::{Env, Store, code};
use crate::cli::ShowArgs;

pub fn show(
    args: &ShowArgs,
    store: &dyn Store,
    env: &Env,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> anyhow::Result<u8> {
    let entry: EntryRef = args.id.parse()?;
    let reader = store.reader()?;
    let Some(detail) = reader.show(&entry, args.max_bytes)? else {
        writeln!(err, "cinelog: show: {entry}: not in the index (or forgotten)")?;
        return Ok(code::NONE);
    };
    if args.json {
        let mut v = serde_json::to_value(&detail)?;
        if let Some(obj) = v.as_object_mut() {
            obj.insert("entry".into(), entry.to_string().into());
        }
        writeln!(out, "{v}")?;
    } else {
        out.write_all(render(&entry, &detail, args.max_bytes, env).as_bytes())?;
    }
    Ok(code::OK)
}

/// The text form. Every value from the index goes through [`text`] sanitising.
pub fn render(entry: &EntryRef, d: &EntryDetail, max_bytes: usize, env: &Env) -> String {
    let st = env.style;
    let mut s = String::new();
    match &d.command {
        Some(c) => command_header(&mut s, c, &d.session, env),
        None => session_header(&mut s, entry, &d.session, env),
    }
    let _ = writeln!(s, "{}", st.paint(DIM, &"─".repeat(40)));
    let output = text::block(&d.output);
    s.push_str(&output);
    if !output.is_empty() && !output.ends_with('\n') {
        s.push('\n');
    }
    if d.output_truncated {
        let note = format!(
            "… more output: cinelog show {entry} --max-bytes {}",
            max_bytes.saturating_mul(4).max(1)
        );
        let _ = writeln!(s, "{}", st.paint(DIM, &note));
    }
    s
}

fn command_header(s: &mut String, c: &Command, sess: &SessionSummary, env: &Env) {
    let st = env.style;
    for (i, line) in text::block(&c.typed).lines().enumerate() {
        let prompt = if i == 0 { "$ " } else { "> " };
        let _ = writeln!(s, "{}", st.paint(BOLD, &format!("{prompt}{line}")));
    }
    if c.typed_trunc {
        let _ = writeln!(s, "{}", st.paint(DIM, "(command text truncated by the recorder)"));
    }
    let mut facts = Vec::new();
    facts.push(match c.exit {
        Some(0) => "exit 0".to_owned(),
        Some(e) => {
            let ps = if c.pipestatus.len() > 1 {
                let v: Vec<String> = c.pipestatus.iter().map(i32::to_string).collect();
                format!(" (pipestatus {})", v.join(" "))
            } else {
                String::new()
            };
            st.paint(RED, &format!("exit {e}{ps}"))
        }
        None if c.t_end.is_none() => st.paint(YELLOW, "running"),
        None => "exit unknown".to_owned(),
    });
    let start = text::fmt_ts(c.ts_start, &env.tz);
    facts.push(match c.ts_end {
        Some(end) => format!(
            "{start} → {} ({})",
            text::fmt_ts_short(end, c.ts_start, &env.tz),
            text::fmt_duration(end - c.ts_start)
        ),
        None => start,
    });
    if let Some(cwd) = &c.cwd {
        facts.push(text::one_line(cwd, 200));
    }
    let user = c.user.as_deref().unwrap_or(&sess.user);
    facts.push(format!(
        "{}@{}",
        text::one_line(user, 64),
        text::one_line(&sess.host, 64)
    ));
    let _ = writeln!(s, "{}", facts.join(" · "));
    if c.depth > 0 {
        let parent = c
            .parent_seq
            .map_or_else(String::new, |p| format!(" inside command {p}"));
        let _ = writeln!(s, "nested shell, depth {}{parent}", c.depth);
    }
    if c.approx {
        let conf = c
            .confidence
            .map_or_else(String::new, |v| format!(", confidence {v:.2}"));
        let _ = writeln!(
            s,
            "{}",
            st.paint(YELLOW, &format!("≈ approximate boundaries (legacy import{conf})"))
        );
    }
    if c.out_truncated {
        let note = "output capped at index time (index.max_output_per_command_mb); the cast has all of it";
        let _ = writeln!(s, "{}", st.paint(DIM, note));
    }
    let _ = writeln!(
        s,
        "{}",
        st.paint(DIM, &format!("{} · t={:.1}s", location(sess), c.t_start))
    );
}

fn session_header(s: &mut String, entry: &EntryRef, sess: &SessionSummary, env: &Env) {
    let st = env.style;
    let mut kind = sess.state.to_string();
    if sess.legacy {
        kind.push_str(", legacy");
    }
    if sess.hidden {
        kind.push_str(", hidden");
    }
    let _ = writeln!(s, "{}", st.paint(BOLD, &format!("session {} ({kind})", sess.uuid)));
    let start = text::fmt_ts(sess.started_at, &env.tz);
    let span = match sess.ended_at {
        Some(end) => format!(
            "{start} → {} ({})",
            text::fmt_ts_short(end, sess.started_at, &env.tz),
            text::fmt_duration(end - sess.started_at)
        ),
        None => format!("since {start}"),
    };
    let _ = writeln!(
        s,
        "{span} · {}@{} · {} commands",
        text::one_line(&sess.user, 64),
        text::one_line(&sess.host, 64),
        sess.commands
    );
    if let EntryRef::At { t, .. } = entry {
        let _ = writeln!(s, "output outside commands around t={t:.1}s");
    }
    let _ = writeln!(s, "{}", st.paint(DIM, &location(sess)));
}

/// `tmux main:1 %3 · cast <path>`.
fn location(sess: &SessionSummary) -> String {
    let t = &sess.tmux;
    let mut parts = Vec::new();
    if t.session.is_some() || t.pane.is_some() {
        let mut where_ = String::from("tmux ");
        if let Some(name) = &t.session {
            where_.push_str(&text::one_line(name, 64));
            if let Some(w) = &t.window {
                let _ = write!(where_, ":{}", text::one_line(w, 16));
            }
            where_.push(' ');
        }
        if let Some(p) = &t.pane {
            where_.push_str(&text::one_line(p, 16));
        }
        parts.push(where_.trim_end().to_owned());
    }
    parts.push(format!(
        "cast {}",
        text::one_line(&sess.cast_path.to_string_lossy(), 4096)
    ));
    parts.join(" · ")
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::super::testutil::*;
    use super::super::text::Style;
    use super::*;
    use crate::cli::{Cli, Command as Cmd};

    fn run(argv: &[&str]) -> (u8, String, String) {
        let mut full = vec!["cinelog", "show"];
        full.extend_from_slice(argv);
        let Cmd::Show(args) = Cli::try_parse_from(full).unwrap().command else {
            unreachable!()
        };
        let store = store();
        capture(|out, err| show(&args, &store, &env(), out, err))
    }

    #[test]
    fn command_with_output() {
        let (c, out, _) = run(&[&format!("{U1}:2")]);
        assert_eq!(c, 0);
        let expected = format!(
            "$ grep needle notes.txt\n\
             exit 1 · 2026-11-08 12:00:10 → 12:00:12 (2.5 s) · /home/alice/src · alice@box\n\
             tmux main:1 %3 · cast /home/alice/logs/2026-11/box_{U1}.cast · t=10.0s\n\
             {}\n\
             the needle is ^[[31mhere^[[0m\n",
            "─".repeat(40)
        );
        assert_eq!(out, expected);
    }

    #[test]
    fn running_command_and_truncation() {
        let (c, out, _) = run(&[&format!("{U1}:1"), "--max-bytes", "5"]);
        assert_eq!(c, 0);
        assert!(out.contains("\ntotal\n"), "{out}");
        assert!(
            out.ends_with(&format!("… more output: cinelog show {U1}:1 --max-bytes 20\n")),
            "{out}"
        );
        let (_, out, _) = run(&[&format!("{U1}:3")]);
        assert!(
            out.starts_with("$ make\ttest\nrunning · 2026-11-08 12:00:20 · /tmp"),
            "{out}"
        );
    }

    #[test]
    fn session_and_point_in_time() {
        let (c, out, _) = run(&[U1]);
        assert_eq!(c, 0);
        assert!(
            out.starts_with(&format!(
                "session {U1} (live)\nsince 2026-11-08 12:00:00 · alice@box · 3 commands\n"
            )),
            "{out}"
        );
        assert!(out.contains("drwxrwxrwt"));
        let (c, out, _) = run(&[&format!("{U1}@15")]);
        assert_eq!(c, 0);
        assert!(out.contains("output outside commands around t=15.0s"), "{out}");
    }

    #[test]
    fn not_found_and_bad_ids() {
        let (c, out, err) = run(&[&format!("{U1}:99")]);
        assert_eq!((c, out.as_str()), (1, ""));
        assert!(err.contains("not in the index"));
        let store = store();
        let args = ShowArgs {
            id: "../../etc/passwd".into(),
            json: false,
            max_bytes: 10,
        };
        let r = show(&args, &store, &env(), &mut Vec::new(), &mut Vec::new());
        assert!(r.unwrap_err().to_string().contains("expected <uuid>"));
    }

    #[test]
    fn json() {
        let (c, out, _) = run(&["--json", &format!("{U1}:2")]);
        assert_eq!(c, 0);
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["entry"], format!("{U1}:2"));
        assert_eq!(v["command"]["typed"], "grep needle notes.txt");
        assert_eq!(v["session"]["user"], "alice");
        assert_eq!(v["output_truncated"], false);
    }

    #[test]
    fn approximate_and_nested_notes() {
        let mut c = command(7, 1.0, "echo hi", Some(0), "/");
        c.approx = true;
        c.confidence = Some(0.8234);
        c.depth = 1;
        c.parent_seq = Some(6);
        c.pipestatus = vec![0, 0];
        let store = store();
        let sess = store.reader().unwrap().session(U1).unwrap().unwrap();
        let d = EntryDetail {
            session: sess,
            command: Some(c),
            output: String::new(),
            output_truncated: false,
        };
        let entry = EntryRef::Command {
            uuid: U1.into(),
            seq: 7,
        };
        let mut env = env();
        let plain = render(&entry, &d, 100, &env);
        assert!(plain.contains("nested shell, depth 1 inside command 6\n"), "{plain}");
        assert!(plain.contains("≈ approximate boundaries (legacy import, confidence 0.82)\n"));
        env.style = Style::COLOR;
        let colored = render(&entry, &d, 100, &env);
        assert!(colored.starts_with(&format!("{BOLD}$ echo hi")), "{colored:?}");
    }
}
