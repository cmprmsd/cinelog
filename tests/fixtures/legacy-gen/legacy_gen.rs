//! Synthetic legacy casts: a generator that mimics the old cinelog plugin (asciinema 3
//! recording a zsh whose `preexec`/`precmd` hooks printed `Begin`/`End` banners, set the
//! window title and appended to `<cast>.commands.log`). Used by the legacy importer's unit
//! tests (`crates/cinelog-index/src/legacy/tests.rs`) and by I14 (`tests/it`).
//!
//! Std-only and deterministic (seeded), so any test crate can include it with
//! `#[path = "<repo>/tests/fixtures/legacy-gen/legacy_gen.rs"] mod legacy_gen;`.
//!
//! What it reproduces, byte for byte where it matters to the importer:
//! - keystroke echo in separate `o` events, `\r\n` on Enter, `for> ` continuation prompts;
//! - `printf '%*s\n' $COLUMNS "Begin [%F %T]"` (and `End` + 3 spaces) after ONLCR; output
//!   is split into events at random points, banners included;
//! - `print -Pn '\e]0;%n@%m: %~ '"$3"'\a'` with prompt expansion of `%` and zsh `print`
//!   escapes (`\c` stops the output, also the closing BEL);
//! - the commands.log line `echo "[<t>, \"m\", \"<$3, \n→; and "→\">\"]"` with zsh `echo`
//!   escapes (`\c` truncates and merges the next entry into the line, `\n` splits it) and
//!   the garbage time of `tail -n1 $LOGFILE` minus 0.1; never for commands with `/hist`;
//! - plain and p10k-like prompts (right prompt, transient redraw on Enter), long commands
//!   that wrap, alias expansion (`$3` is expanded, the screen shows what was typed),
//!   `SHOW_START_END_TIME=0` periods, wall clock steps (DST), empty Enter, resizes.
#![allow(dead_code)]

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

/// Deterministic xorshift64*.
#[derive(Debug, Clone)]
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Self(seed.max(1))
    }

    pub fn next_u64(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    pub fn below(&mut self, n: u64) -> u64 {
        self.next_u64() % n.max(1)
    }

    /// Seconds in `[lo, hi)` in millisecond steps.
    pub fn secs(&mut self, lo: f64, hi: f64) -> f64 {
        let span = ((hi - lo) * 1000.0).max(1.0) as u64;
        lo + self.below(span) as f64 / 1000.0
    }
}

/// Prompt look.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Prompt {
    /// `user@host:~/dir$ `
    Plain,
    /// Two-line p10k-like prompt with a right prompt on the first line; with `transient`,
    /// both lines are redrawn as `❯ <cmd>` on Enter.
    P10k { transient: bool },
}

/// One command as the user typed it.
#[derive(Debug, Clone, Default)]
pub struct Cmd {
    /// Typed text; `\n` is Enter inside an unfinished construct (continuation prompt).
    pub typed: String,
    /// `$3` when it differs from `typed` (alias expansion).
    pub expanded: Option<String>,
    pub output: Vec<String>,
    /// Run time.
    pub secs: f64,
}

impl Cmd {
    pub fn new(typed: &str) -> Self {
        Self {
            typed: typed.to_owned(),
            secs: 0.3,
            ..Self::default()
        }
    }

    pub fn out(mut self, lines: &[&str]) -> Self {
        self.output = lines.iter().map(|l| (*l).to_owned()).collect();
        self
    }

    pub fn alias(mut self, expanded: &str) -> Self {
        self.expanded = Some(expanded.to_owned());
        self
    }

    pub fn secs(mut self, secs: f64) -> Self {
        self.secs = secs;
        self
    }

    /// What zsh passes to preexec as `$3`.
    pub fn dollar3(&self) -> &str {
        self.expanded.as_deref().unwrap_or(&self.typed)
    }
}

/// What the importer should find for one command.
#[derive(Debug, Clone, PartialEq)]
pub struct Expect {
    /// The command as typed (screen side).
    pub typed: String,
    /// The log entry as a tolerant parser recovers it (`"` unescaped); `None`: not logged.
    pub logged: Option<String>,
    /// The log entry was cut by `\c`.
    pub truncated: bool,
    /// A `Begin` banner was printed.
    pub banner: bool,
    /// Cast time of the Enter key (the banner follows within milliseconds), seconds.
    pub t: f64,
    /// The output lines.
    pub output: Vec<String>,
}

/// A generated cast with its commands.log.
#[derive(Debug, Clone)]
pub struct Legacy {
    pub cast: String,
    /// `None`: no commands.log file.
    pub log: Option<String>,
    pub expect: Vec<Expect>,
}

impl Legacy {
    /// Writes `<dir>/<name>` and, if there is one, `<dir>/<name>.commands.log`.
    pub fn write(&self, dir: &Path, name: &str) -> std::io::Result<PathBuf> {
        let cast = dir.join(name);
        std::fs::write(&cast, &self.cast)?;
        if let Some(log) = &self.log {
            std::fs::write(dir.join(format!("{name}.commands.log")), log)?;
        }
        Ok(cast)
    }

    pub fn banners(&self) -> usize {
        self.expect.iter().filter(|e| e.banner).count()
    }

    pub fn logged(&self) -> usize {
        self.expect.iter().filter(|e| e.logged.is_some()).count()
    }
}

/// The old plugin running in a terminal.
#[derive(Debug, Clone)]
pub struct Gen {
    cols: u16,
    rows: u16,
    header_size: (u16, u16),
    term: Option<String>,
    /// Header `timestamp` (UTC epoch seconds).
    started_at: i64,
    /// UTC offset of the banners' wall clock, seconds.
    utc_offset: i64,
    prompt: Prompt,
    show_times: bool,
    user: String,
    host: String,
    cwd: String,
    rng: Rng,
    t: f64,
    /// Time added to the next event's interval.
    pending: f64,
    events: Vec<(f64, char, String)>,
    log: String,
    expect: Vec<Expect>,
}

impl Gen {
    pub fn new(seed: u64) -> Self {
        Self {
            cols: 100,
            rows: 30,
            header_size: (100, 30),
            term: Some("xterm-256color".into()),
            started_at: 1_700_000_000,
            utc_offset: 3600,
            prompt: Prompt::Plain,
            show_times: true,
            user: "alice".into(),
            host: "box".into(),
            cwd: "~/src".into(),
            rng: Rng::new(seed),
            t: 0.0,
            pending: 0.0,
            events: Vec::new(),
            log: String::new(),
            expect: Vec::new(),
        }
    }

    pub fn with_prompt(mut self, p: Prompt) -> Self {
        self.prompt = p;
        self
    }

    pub fn with_size(mut self, cols: u16, rows: u16) -> Self {
        self.cols = cols;
        self.rows = rows;
        self.header_size = (cols, rows);
        self
    }

    pub fn with_term(mut self, term: Option<&str>) -> Self {
        self.term = term.map(str::to_owned);
        self
    }

    pub fn with_start(mut self, epoch: i64, utc_offset: i64) -> Self {
        self.started_at = epoch;
        self.utc_offset = utc_offset;
        self
    }

    pub fn started_at(&self) -> i64 {
        self.started_at
    }

    /// Cast time so far.
    pub fn t(&self) -> f64 {
        self.t
    }

    fn emit(&mut self, after: f64, code: char, data: &str) {
        let interval = after + std::mem::take(&mut self.pending);
        self.t += interval;
        self.events.push((interval, code, data.to_owned()));
    }

    /// Emits `data` as `o` events split at random char boundaries; the first event comes
    /// `after` seconds after the previous one.
    fn out(&mut self, after: f64, data: &str) {
        let chars: Vec<char> = data.chars().collect();
        let mut i = 0;
        let mut gap = after;
        while i < chars.len() {
            let left = chars.len() - i;
            let n = if left > 4 && self.rng.below(3) == 0 {
                1 + self.rng.below(left as u64 - 1) as usize
            } else {
                left
            };
            let piece: String = chars[i..i + n].iter().collect();
            self.emit(gap, 'o', &piece);
            gap = 0.0005;
            i += n;
        }
    }

    fn wall(&self) -> String {
        civil(self.started_at + self.t as i64 + self.utc_offset)
    }

    /// `printf '%*s\n' $COLUMNS "<text>"` through ONLCR.
    fn padded(&self, text: &str) -> String {
        let w = usize::from(self.cols);
        format!("{text:>w$}\r\n")
    }

    /// Draws the prompt (at the start and after every command).
    pub fn draw_prompt(&mut self) {
        let p = match self.prompt {
            Prompt::Plain => format!("{}@{}:{}$ ", self.user, self.host, self.cwd),
            Prompt::P10k { .. } => {
                let right = "✔  10:32:01";
                let col = usize::from(self.cols) - right.chars().count() + 1;
                format!(
                    "\x1b[1m{} \x1b[0mon \x1b[32m main\x1b[0m\x1b[{col}G{right}\r\n\x1b[35m❯\x1b[0m ",
                    self.cwd
                )
            }
        };
        self.out(0.01, &p);
    }

    /// Wall clock step for later banners (DST: 3600 or -3600).
    pub fn clock_step(&mut self, secs: i64) {
        self.utc_offset += secs;
    }

    /// `SHOW_START_END_TIME` (edited config sourced again).
    pub fn show_times(&mut self, on: bool) {
        self.show_times = on;
    }

    pub fn resize(&mut self, cols: u16, rows: u16) {
        self.cols = cols;
        self.rows = rows;
        self.emit(0.2, 'r', &format!("{cols}x{rows}"));
    }

    pub fn idle(&mut self, secs: f64) {
        self.pending += secs;
    }

    /// Enter on an empty line: no preexec, only precmd (`End`) and a new prompt.
    pub fn empty_enter(&mut self) {
        let think = self.rng.secs(0.3, 1.0);
        self.emit(think, 'o', "\r\n");
        if self.show_times {
            let end = self.padded(&format!("End   [{}]", self.wall()));
            self.out(0.003, &end);
        }
        self.draw_prompt();
    }

    /// Types and runs one command.
    pub fn run(&mut self, cmd: &Cmd) {
        let think = self.rng.secs(0.5, 3.0);
        for (i, c) in cmd.typed.chars().enumerate() {
            let after = if i == 0 { think } else { self.rng.secs(0.03, 0.2) };
            let echo = if c == '\n' {
                "\r\nfor> ".to_owned()
            } else {
                c.to_string()
            };
            self.emit(after, 'o', &echo);
        }
        let enter = self.rng.secs(0.05, 0.3);
        match self.prompt {
            Prompt::P10k { transient: true } => {
                // Up to the first prompt line, clear, minimal prompt with the command.
                let up = 1 + cmd.typed.matches('\n').count();
                let shown = cmd.typed.replace('\n', "\r\nfor> ");
                self.emit(enter, 'o', &format!("\x1b[{up}A\r\x1b[J\x1b[35m❯\x1b[0m {shown}\r\n"));
            }
            _ => self.emit(enter, 'o', "\r\n"),
        }
        let t_enter = self.t;
        let d3 = cmd.dollar3().to_owned();
        let logged = (!d3.contains("/hist")).then(|| self.log_entry(&d3));
        if self.show_times {
            let b = self.padded(&format!("Begin [{}]", self.wall()));
            self.out(0.004, &b);
        }
        let mut title = format!("\x1b]0;{}@{}: {} ", self.user, self.host, self.cwd);
        let (expanded, stopped) = print_p(&d3);
        title.push_str(&expanded);
        if !stopped {
            title.push('\x07');
        }
        self.out(0.001, &title);
        let gap = cmd.secs / (cmd.output.len() + 1) as f64;
        for l in &cmd.output {
            self.out(gap, &format!("{l}\r\n"));
        }
        self.pending += gap;
        if self.show_times {
            let end = self.padded(&format!(
                "End   [{}]",
                civil(self.started_at + (self.t + gap) as i64 + self.utc_offset)
            ));
            self.out(0.003, &end);
        }
        self.draw_prompt();
        let (logged, truncated) = logged.map_or((None, false), |(text, trunc)| (Some(text), trunc));
        self.expect.push(Expect {
            typed: cmd.typed.clone(),
            logged,
            truncated,
            banner: self.show_times,
            t: t_enter,
            // An unterminated title swallows the output until the next escape sequence.
            output: if stopped { Vec::new() } else { cmd.output.clone() },
        });
    }

    /// Appends the commands.log line; returns the entry as a tolerant parser recovers it.
    fn log_entry(&mut self, d3: &str) -> (String, bool) {
        let processed = d3.replace('\n', ";").replace('"', "\\\"");
        let last = self.events.last().map_or(0.0, |e| e.0);
        let prefix = format!("[{:.6}, \"m\", \"", last - 0.1);
        let (text, stopped) = zsh_echo(&format!("{prefix}{processed}\"]"));
        self.log.push_str(&text);
        if !stopped {
            self.log.push('\n');
        }
        let body = text.strip_prefix(prefix.as_str()).unwrap_or(&text);
        let body = body.strip_suffix("\"]").unwrap_or(body);
        (body.replace("\\\"", "\""), stopped)
    }

    /// The shell exits with `exit`.
    pub fn finish(mut self, exit: i32, with_log: bool) -> Legacy {
        self.emit(0.5, 'o', "\r\n");
        self.emit(0.01, 'x', &exit.to_string());
        let mut cast = String::new();
        let term = self
            .term
            .as_ref()
            .map(|t| format!(",\"type\":{}", json_string(t)))
            .unwrap_or_default();
        let _ = writeln!(
            cast,
            "{{\"version\":3,\"term\":{{\"cols\":{},\"rows\":{}{term}}},\"timestamp\":{},\"env\":{{\"SHELL\":\"/usr/bin/zsh\"}}}}",
            self.header_size.0, self.header_size.1, self.started_at
        );
        for (after, code, data) in &self.events {
            let _ = writeln!(cast, "[{after:.6}, \"{code}\", {}]", json_string(data));
        }
        Legacy {
            cast,
            log: with_log.then_some(self.log),
            expect: self.expect,
        }
    }
}

/// zsh `echo` (no `-E`): interprets backslash escapes; `\c` stops all output. Returns the
/// text and whether it was stopped.
pub fn zsh_echo(s: &str) -> (String, bool) {
    let mut out = String::new();
    let mut it = s.chars().peekable();
    while let Some(c) = it.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match it.next() {
            Some('a') => out.push('\x07'),
            Some('b') => out.push('\x08'),
            Some('c') => return (out, true),
            Some('e' | 'E') => out.push('\x1b'),
            Some('f') => out.push('\x0c'),
            Some('n') => out.push('\n'),
            Some('r') => out.push('\r'),
            Some('t') => out.push('\t'),
            Some('v') => out.push('\x0b'),
            Some('\\') => out.push('\\'),
            Some(o) => {
                out.push('\\');
                out.push(o);
            }
            None => out.push('\\'),
        }
    }
    (out, false)
}

/// zsh `print -P`: prompt expansion (`%%` is `%`, other sequences expand to nothing here,
/// `%F{…}` with its argument), then the `echo` escapes.
pub fn print_p(s: &str) -> (String, bool) {
    let mut expanded = String::new();
    let mut it = s.chars().peekable();
    while let Some(c) = it.next() {
        if c != '%' {
            expanded.push(c);
            continue;
        }
        match it.next() {
            Some('%') => expanded.push('%'),
            Some('F' | 'K') if it.peek() == Some(&'{') => {
                for c in it.by_ref() {
                    if c == '}' {
                        break;
                    }
                }
            }
            Some(_) | None => {}
        }
    }
    zsh_echo(&expanded)
}

/// asciicast JSON string literal (as asciinema writes it: `\u001b` for ESC).
pub fn json_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 || c == '\x7f' => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// `YYYY-MM-DD HH:MM:SS` of `secs` since the epoch on a naive clock.
pub fn civil(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    let tod = secs.rem_euclid(86_400);
    // Howard Hinnant's civil_from_days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02}",
        tod / 3600,
        tod / 60 % 60,
        tod % 60
    )
}

/// Named scenarios covering every importer path (§12.1 legacy aligner, I14).
pub fn corpus() -> Vec<(&'static str, Legacy)> {
    let mut v = Vec::new();

    let mut g = Gen::new(1);
    g.draw_prompt();
    g.run(&Cmd::new("ls").out(&["a.txt  b.txt"]));
    g.run(&Cmd::new("git status").out(&["On branch main", "nothing to commit, working tree clean"]));
    g.run(&Cmd::new("ll /tmp").alias("ls -l --color=tty /tmp").out(&["total 0"]));
    g.empty_enter();
    g.run(&Cmd::new("~/bin/hist foo").out(&["foo bar"]));
    g.run(&Cmd::new("echo \"quoted\" 'single'").out(&["quoted single"]));
    g.run(&Cmd::new("ls").out(&["a.txt  b.txt"]));
    g.run(&Cmd::new("date +%s").out(&["1700000123"]));
    g.run(&Cmd::new("cd /etc"));
    g.run(&Cmd::new("ls").out(&["passwd  hosts"]));
    v.push(("plain.cast", g.finish(0, true)));

    let mut g = Gen::new(2).with_prompt(Prompt::P10k { transient: true });
    g.draw_prompt();
    g.run(&Cmd::new("make -j8").out(&["cc -o x x.c", "done"]).secs(4.0));
    g.run(&Cmd::new("for i in 1 2; do\necho $i\ndone").out(&["1", "2"]));
    g.run(&Cmd::new("vim notes.txt").secs(20.0));
    g.run(
        &Cmd::new("gco feature/x")
            .alias("git checkout feature/x")
            .out(&["Switched to branch 'feature/x'"]),
    );
    v.push(("p10k.cast", g.finish(0, true)));

    let mut g = Gen::new(3).with_prompt(Prompt::P10k { transient: false });
    g.draw_prompt();
    g.run(&Cmd::new("cargo build --release").out(&["Finished"]));
    g.run(&Cmd::new("htop").secs(5.0));
    v.push(("p10k-plain.cast", g.finish(0, true)));

    let mut g = Gen::new(4).with_size(40, 20);
    g.draw_prompt();
    g.run(&Cmd::new("find / -name '*.conf' -newer /etc/hostname -print 2>/dev/null").out(&["/etc/x.conf"]));
    g.run(&Cmd::new("uptime").out(&["up 3 days"]));
    v.push(("wrapped.cast", g.finish(0, true)));

    let mut g = Gen::new(5);
    g.draw_prompt();
    g.run(&Cmd::new("printf 'a\\cb'").out(&["a"]));
    g.run(&Cmd::new("pwd").out(&["/home/alice/src"]));
    g.run(&Cmd::new("echo -e 'x\\ny'").out(&["x", "y"]));
    g.run(&Cmd::new("whoami").out(&["alice"]));
    v.push(("backslash.cast", g.finish(0, true)));

    let mut g = Gen::new(6);
    g.draw_prompt();
    g.run(&Cmd::new("echo one").out(&["one"]));
    g.show_times(false);
    g.run(&Cmd::new("echo two").out(&["two"]));
    g.run(&Cmd::new("echo three").out(&["three"]));
    g.show_times(true);
    g.run(&Cmd::new("echo four").out(&["four"]));
    v.push(("no-times.cast", g.finish(0, true)));

    // 2023-10-29 00:59:00 UTC: CEST (+2) ends at 01:00 UTC.
    let mut g = Gen::new(7).with_start(1_698_541_140, 7200);
    g.draw_prompt();
    g.run(&Cmd::new("echo before").out(&["before"]));
    g.idle(90.0);
    g.clock_step(-3600);
    g.run(&Cmd::new("echo after").out(&["after"]));
    v.push(("dst.cast", g.finish(0, true)));

    let mut g = Gen::new(8);
    g.draw_prompt();
    g.run(&Cmd::new("uname -a").out(&["Linux box"]));
    g.resize(60, 20);
    g.run(&Cmd::new("df -h").out(&["/dev/sda1 50G"]));
    v.push(("resize.cast", g.finish(0, true)));

    let mut g = Gen::new(9);
    g.draw_prompt();
    g.run(&Cmd::new("ls").out(&["x"]));
    v.push(("nolog.cast", g.finish(0, false)));

    let mut g = Gen::new(10).with_term(Some("dumb")).with_size(80, 24);
    g.show_times(false);
    v.push(("dumb.cast", g.finish(0, true)));

    v
}

/// A long, varied cast (`n` commands) for resumption and throughput tests.
pub fn long(seed: u64, n: usize) -> Legacy {
    const CMDS: &[&str] = &[
        "ls -la",
        "git status",
        "git diff --stat",
        "cargo test -p foo",
        "vim src/main.rs",
        "cd ..",
        "make",
        "grep -rn needle .",
        "ssh host uptime",
        "docker ps",
        "kubectl get pods -A",
        "journalctl -u nginx -n 50",
        "python3 -m http.server",
        "tail -f /var/log/syslog",
        "htop",
        "less README.md",
        "man zshall",
    ];
    let mut g = Gen::new(seed);
    let mut rng = Rng::new(seed ^ 0x5eed);
    g.draw_prompt();
    for i in 0..n {
        let base = CMDS[rng.below(CMDS.len() as u64) as usize];
        let typed = if rng.below(4) == 0 {
            format!("{base} # {i}")
        } else {
            base.to_owned()
        };
        let lines: Vec<String> = (0..rng.below(40))
            .map(|k| format!("{i:04}.{k:02} {base} output line"))
            .collect();
        let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
        g.run(&Cmd::new(&typed).out(&refs).secs(rng.secs(0.1, 5.0)));
        if rng.below(10) == 0 {
            g.empty_enter();
        }
    }
    g.finish(0, true)
}

#[cfg(test)]
mod self_tests {
    use super::*;

    #[test]
    fn helpers() {
        assert_eq!(civil(1_700_000_000), "2023-11-14 22:13:20");
        assert_eq!(civil(0), "1970-01-01 00:00:00");
        assert_eq!(zsh_echo("a\\tb\\\\c\\\"d"), ("a\tb\\c\\\"d".into(), false));
        assert_eq!(zsh_echo("x\\cy"), ("x".into(), true));
        assert_eq!(print_p("date +%s %% %F{red}x"), ("date + % x".into(), false));
        assert_eq!(json_string("a\"\u{1b}\n"), "\"a\\\"\\u001b\\n\"");
    }
}
