//! Synthetic cinelog casts (docs/PROTOCOL.md §5), written the way `cinelog rec` writes them:
//! a v3 header, the `session` marker at t=0, `hello`, `cmd`/`done` around each command,
//! `stats` and `x`. They let the index scenarios (I12, I13) build corpora of any size and age
//! without recording for real. The result parses with the independent reader in
//! [`crate::cast`].

use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Result;
use serde_json::{Value, json};

use crate::util::{local_time, random_hex};

/// Deterministic xorshift64* stream for reproducible filler text.
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

    /// One filler word.
    pub fn word(&mut self) -> &'static str {
        const WORDS: &[&str] = &[
            "alpha",
            "bravo",
            "charlie",
            "delta",
            "echo",
            "foxtrot",
            "golf",
            "hotel",
            "india",
            "juliet",
            "kilo",
            "lima",
            "mike",
            "november",
            "oscar",
            "papa",
            "quebec",
            "romeo",
            "sierra",
            "tango",
            "uniform",
            "victor",
            "whiskey",
            "xray",
            "yankee",
            "zulu",
            "/usr/lib",
            "error:",
            "warning:",
            "ok",
            "[INFO]",
            "0x7ffd",
            "λ",
            "größe",
            "naïve",
            "─┼─",
        ];
        WORDS[self.below(WORDS.len() as u64) as usize]
    }

    /// `n` lines of filler text, each at most `width` columns wide.
    pub fn lines(&mut self, n: usize, width: usize) -> String {
        let mut out = String::new();
        for i in 0..n {
            let mut line = format!("{i:05} ");
            while line.chars().count() + 10 < width {
                line.push_str(self.word());
                line.push(' ');
            }
            out.push_str(line.trim_end());
            out.push('\n');
        }
        out
    }
}

/// A random UUIDv7 for a session that started at `epoch_ms`.
pub fn uuid_v7(epoch_ms: u64) -> String {
    let rand = random_hex(10);
    let ms = format!("{:012x}", epoch_ms & 0xffff_ffff_ffff);
    let variant = 8 + u8::from_str_radix(&rand[3..4], 16).unwrap_or(0) % 4;
    format!(
        "{}-{}-7{}-{:x}{}-{}",
        &ms[..8],
        &ms[8..],
        &rand[..3],
        variant,
        &rand[4..7],
        &rand[7..19]
    )
}

/// Expected facts of one synthetic command.
#[derive(Debug, Clone, PartialEq)]
pub struct SynthCommand {
    pub seq: u64,
    pub typed: String,
    pub exit: i32,
    /// Offset of the `cmd` marker (seconds since t=0).
    pub t_start: f64,
    pub t_end: f64,
    /// The plain output text (as written, `\n` line ends).
    pub output: String,
}

/// Builder of one synthetic cinelog cast.
#[derive(Debug, Clone)]
pub struct SynthCast {
    pub uuid: String,
    pub start_epoch_us: u64,
    pub host: String,
    pub user: String,
    pub uid: u32,
    pub cwd: String,
    pub cols: u16,
    pub rows: u16,
    /// Size at t=0 (the header's `term`).
    initial: (u16, u16),
    sh: u32,
    n: u64,
    seq: u64,
    t_us: u64,
    pending_us: u64,
    lines: Vec<String>,
    pub commands: Vec<SynthCommand>,
}

/// Compact cinelog marker JSON with `cl` and `k` first, as PROTOCOL §5.1 requires.
fn marker(kind: &str, fields: &[(&str, Value)]) -> String {
    let mut s = format!("{{\"cl\":1,\"k\":{}", json!(kind));
    for (k, v) in fields {
        let _ = write!(s, ",{}:{}", json!(k), v);
    }
    s.push('}');
    s
}

const PROMPT: &str = "\r\x1b[0m\x1b[27m\x1b[24m\x1b[Jclit% \x1b[K";

impl SynthCast {
    /// A cast whose recording started at `start_epoch_us`, with the shell at its first prompt.
    pub fn new(start_epoch_us: u64, cols: u16, rows: u16) -> Self {
        let user = nix::unistd::User::from_uid(nix::unistd::getuid())
            .ok()
            .flatten()
            .map_or_else(|| "user".to_owned(), |u| u.name);
        let mut c = Self {
            uuid: uuid_v7(start_epoch_us / 1000),
            start_epoch_us,
            host: crate::cli::host_component(),
            user,
            uid: nix::unistd::getuid().as_raw(),
            cwd: "/srv/project".into(),
            cols,
            rows,
            initial: (cols, rows),
            sh: 40_000 + u32::try_from(start_epoch_us % 20_000).unwrap_or(0),
            n: 0,
            seq: 0,
            t_us: 0,
            pending_us: 0,
            lines: Vec::new(),
            commands: Vec::new(),
        };
        let session = marker(
            "session",
            &[
                ("uuid", json!(c.uuid)),
                ("part", json!(1)),
                ("start_epoch_us", json!(start_epoch_us)),
                ("boottime_us", json!(5_000_000_000u64)),
                ("tz", json!("UTC")),
                ("tz_offset_s", json!(0)),
                ("scope", json!("personal")),
                ("host", json!(c.host)),
                ("user", json!(c.user)),
                ("uid", json!(c.uid)),
                ("euid", json!(c.uid)),
                ("attribution", json!("none")),
                ("tty", json!("/dev/pts/77")),
                ("holder_comm", json!("terminator")),
                ("holder_pid", json!(4242)),
                ("rec_pid", json!(c.sh - 1)),
                ("rec_starttime", json!(1234)),
                ("boot_id", json!("00000000-0000-4000-8000-000000000000")),
                ("shell_pid", json!(c.sh)),
                ("input", json!(true)),
                ("rec_version", json!("0.10.0")),
                ("protocol", json!(1)),
            ],
        );
        c.event(0, "m", &session);
        c.event(9_000, "o", "\x1b]133;A\x07");
        let hello = marker(
            "hello",
            &[
                ("sh", json!(c.sh)),
                ("sst", json!(1240)),
                ("ppid", json!(c.sh - 1)),
                ("user", json!(c.user)),
                ("uid", json!(c.uid)),
                ("euid", json!(c.uid)),
                ("host", json!(c.host)),
                ("tty", json!("/dev/pts/78")),
                ("cwd", json!(c.cwd)),
                ("zsh", json!("5.9")),
                ("t", json!(c.epoch_at(c.t_us + 500))),
            ],
        );
        c.event(500, "m", &hello);
        c.event(1_000, "o", PROMPT);
        c
    }

    /// Seconds since t=0 of the last event.
    pub fn t(&self) -> f64 {
        self.t_us as f64 / 1e6
    }

    fn epoch_at(&self, t_us: u64) -> f64 {
        (self.start_epoch_us + t_us) as f64 / 1e6
    }

    /// Appends an event `interval_us` after the previous one.
    pub fn event(&mut self, interval_us: u64, code: &str, data: &str) {
        let interval_us = interval_us + std::mem::take(&mut self.pending_us);
        self.t_us += interval_us;
        self.lines.push(format!(
            "[{}.{:06}, {}, {}]",
            interval_us / 1_000_000,
            interval_us % 1_000_000,
            json!(code),
            json!(data)
        ));
    }

    /// Output text as the PTY delivers it (`\n` → `\r\n`), in reads of at most 4 KiB spread
    /// over `duration_us`.
    pub fn output(&mut self, text: &str, duration_us: u64) {
        let tty = text.replace('\n', "\r\n");
        let mut chunks = Vec::new();
        let mut rest = tty.as_str();
        while !rest.is_empty() {
            let mut cut = rest.len().min(4096);
            while !rest.is_char_boundary(cut) {
                cut -= 1;
            }
            chunks.push(&rest[..cut]);
            rest = &rest[cut..];
        }
        let step = duration_us / (chunks.len() as u64).max(1);
        for c in chunks {
            self.event(step.max(1), "o", c);
        }
    }

    /// Types `typed`, runs it for `duration_us` printing `output`, and returns to the prompt.
    pub fn command(&mut self, typed: &str, output: &str, exit: i32, duration_us: u64) -> u64 {
        self.begin(typed);
        self.note_output(output);
        self.output(output, duration_us);
        self.end(exit)
    }

    /// A full-screen program: enters the alternate screen, draws `frames` (one per 100 ms),
    /// and leaves it again.
    pub fn alt_screen_command(&mut self, typed: &str, frames: &[String], exit: i32) -> u64 {
        self.begin(typed);
        self.event(2_000, "o", "\x1b[?1049h\x1b[H\x1b[2J");
        for f in frames {
            self.note_output(f);
            let mut screen = String::from("\x1b[H");
            for (i, line) in f.lines().take(usize::from(self.rows)).enumerate() {
                let _ = write!(screen, "\x1b[{};1H{line}\x1b[K", i + 1);
            }
            self.event(100_000, "o", &screen);
        }
        self.event(2_000, "o", "\x1b[?1049l");
        self.end(exit)
    }

    /// Output while no command runs (e.g. of a leading-space command, which has a `skip`).
    pub fn skipped_command(&mut self, echo: &str, output: &str) {
        self.event(40_000, "i", &format!(" {echo}"));
        self.event(100, "o", &format!(" {echo}"));
        self.event(20_000, "i", "\r");
        self.event(100, "o", "\r\n");
        let skip = marker(
            "skip",
            &[("sh", json!(self.sh)), ("t", json!(self.epoch_at(self.t_us)))],
        );
        self.event(300, "m", &skip);
        self.output(output, 1_000);
        self.event(500, "o", PROMPT);
    }

    /// A terminal resize (`r` event) followed by the shell's redraw.
    pub fn resize(&mut self, cols: u16, rows: u16) {
        self.cols = cols;
        self.rows = rows;
        self.event(250_000, "r", &format!("{cols}x{rows}"));
        self.event(1_000, "o", PROMPT);
    }

    /// Idle time at the prompt: the next event comes `us` later.
    pub fn idle(&mut self, us: u64) {
        self.pending_us += us;
    }

    fn begin(&mut self, typed: &str) {
        self.n += 1;
        self.seq += 1;
        self.event(60_000, "i", typed);
        self.event(200, "o", typed);
        self.event(30_000, "i", "\r");
        self.event(100, "o", "\x1b[?2004l\r\r\n\x1b]133;C\x07");
        let cmd = marker(
            "cmd",
            &[
                ("seq", json!(self.seq)),
                ("sh", json!(self.sh)),
                ("n", json!(self.n)),
                ("typed", json!(typed)),
                ("expanded", json!(typed)),
                ("cwd", json!(self.cwd)),
                ("user", json!(self.user)),
                ("euid", json!(self.uid)),
                ("host", json!(self.host)),
                ("t", json!(self.epoch_at(self.t_us + 400))),
            ],
        );
        self.event(400, "m", &cmd);
        self.commands.push(SynthCommand {
            seq: self.seq,
            typed: typed.to_owned(),
            exit: 0,
            t_start: self.t(),
            t_end: self.t(),
            output: String::new(),
        });
    }

    fn end(&mut self, exit: i32) -> u64 {
        self.event(50, "o", &format!("\x1b]133;D;{exit}\x07"));
        let done = marker(
            "done",
            &[
                ("seq", json!(self.seq)),
                ("sh", json!(self.sh)),
                ("n", json!(self.n)),
                ("exit", json!(exit)),
                ("pipestatus", json!([exit])),
                ("cwd", json!(self.cwd)),
                ("t", json!(self.epoch_at(self.t_us + 300))),
            ],
        );
        self.event(300, "m", &done);
        let t = self.t();
        if let Some(c) = self.commands.last_mut() {
            c.exit = exit;
            c.t_end = t;
        }
        self.event(50, "o", "\x1b]133;A\x07");
        self.event(500, "o", PROMPT);
        self.seq
    }

    /// Records `output` as the expected output of the last command (for assertions).
    pub fn note_output(&mut self, output: &str) {
        if let Some(c) = self.commands.last_mut() {
            c.output.push_str(output);
        }
    }

    /// The finished cast text: `exit`, then `stats` and the `x` event.
    pub fn finish(mut self, exit: i32) -> (String, Self) {
        self.event(300_000, "o", "\x1b[?2004l\r\r\n");
        let stats = marker(
            "stats",
            &[
                ("bad_markers", json!(0)),
                ("dropped_bytes", json!(0)),
                ("lossy_bytes", json!(0)),
                ("parts", json!(1)),
            ],
        );
        self.event(1_000, "m", &stats);
        self.event(0, "x", &exit.to_string());
        let header = format!(
            "{{\"version\":3,\"term\":{{\"cols\":{},\"rows\":{},\"type\":\"xterm-256color\"}},\"timestamp\":{},\"env\":{{\"SHELL\":\"/bin/zsh\",\"TERM\":\"xterm-256color\"}}}}",
            self.initial.0,
            self.initial.1,
            self.start_epoch_us / 1_000_000
        );
        let mut text = header;
        text.push('\n');
        for l in &self.lines {
            text.push_str(l);
            text.push('\n');
        }
        (text, self)
    }

    /// Writes the finished cast below `logs` at the path the recorder would use
    /// (`<YYYY-MM>/<host>_<YYYYmmddTHHMMSS.ffffff>_<id8>.cast`) and sets its mtime to the end
    /// of the recording.
    pub fn write(self, logs: &Path, exit: i32) -> Result<(PathBuf, Self)> {
        let (text, c) = self.finish(exit);
        let (month, stamp) = local_time(c.start_epoch_us);
        let id8 = &c.uuid[c.uuid.len() - 8..];
        let dir = logs.join(month);
        fs::create_dir_all(&dir)?;
        let path = dir.join(format!("{}_{stamp}_{id8}.cast", c.host));
        fs::write(&path, text)?;
        let end_us = c.start_epoch_us + c.t_us;
        crate::util::set_times(&path, end_us, end_us)?;
        Ok((path, c))
    }
}

/// Shape of a synthetic corpus ([`corpus`]).
#[derive(Debug, Clone)]
pub struct CorpusSpec {
    pub seed: u64,
    /// Number of casts.
    pub casts: usize,
    /// Commands per cast.
    pub commands: usize,
    /// Upper bound of output lines per command (the first cast gets ten times as many).
    pub max_lines: usize,
    /// Start of the newest cast (µs since the epoch); older ones start 3 h apart.
    pub newest_start_us: u64,
}

/// Writes a corpus below `logs`: varied commands (wrapped and UTF-8 output, alt-screen
/// programs, resizes, leading-space commands, failures, idle gaps) in casts that ended
/// before `newest_start_us` plus their duration. Returns the files with their builders.
pub fn corpus(logs: &Path, spec: &CorpusSpec) -> Result<Vec<(PathBuf, SynthCast)>> {
    const CMDS: &[&str] = &[
        "make -j8",
        "cargo test",
        "git log --stat",
        "journalctl -n 200",
        "ls -la /usr/lib",
        "grep -rn TODO .",
        "kubectl get pods -A",
        "dmesg | tail -50",
        "python3 report.py",
    ];
    let mut rng = Rng::new(spec.seed);
    let mut out = Vec::with_capacity(spec.casts);
    for i in 0..spec.casts {
        let start = spec.newest_start_us - (i as u64) * 3 * 3_600_000_000;
        let (cols, rows) = if i % 3 == 2 { (80, 24) } else { (120, 40) };
        let mut c = SynthCast::new(start, cols, rows);
        let max_lines = if i == 0 { spec.max_lines * 10 } else { spec.max_lines };
        for k in 0..spec.commands {
            let base = CMDS[rng.below(CMDS.len() as u64) as usize];
            let typed = format!("{base} # c{i}k{k}");
            match rng.below(20) {
                0 => {
                    let frames: Vec<String> = (0..3)
                        .map(|f| rng.lines(rows as usize - 2, cols as usize - 4) + &format!("frame {f} of c{i}k{k}"))
                        .collect();
                    c.alt_screen_command(&format!("top # c{i}k{k}"), &frames, 0);
                }
                1 => c.skipped_command(&format!("echo hidden c{i}k{k}"), "hidden\n"),
                2 => c.resize(if c.cols == 120 { 100 } else { 120 }, rows),
                _ => {
                    let n = rng.below(max_lines as u64 + 1) as usize;
                    // Some lines are wider than the terminal and soft-wrap.
                    let width = if rng.below(4) == 0 {
                        usize::from(c.cols) + 40
                    } else {
                        usize::from(c.cols) - 10
                    };
                    let text = format!("{}end of c{i}k{k}\n", rng.lines(n, width));
                    let exit = if rng.below(8) == 0 { 1 + rng.below(2) as i32 } else { 0 };
                    let duration = 50_000 + rng.below(3_000_000);
                    c.command(&typed, &text, exit, duration);
                }
            }
            if rng.below(10) == 0 {
                c.idle(rng.below(600) * 1_000_000);
            }
        }
        out.push(c.write(logs, 0)?);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cast::Cast;

    fn sample(start_us: u64) -> SynthCast {
        let mut c = SynthCast::new(start_us, 100, 30);
        let mut rng = Rng::new(7);
        let out = rng.lines(300, 90);
        c.command("make all", &out, 0, 2_000_000);
        c.skipped_command("secret-ish", "x\n");
        c.resize(80, 24);
        c.alt_screen_command("top", &["frame one".to_owned(), "frame two\nline".to_owned()], 0);
        c.idle(5_000_000);
        c.command("false", "", 1, 1_000);
        c
    }

    #[test]
    fn synthetic_casts_parse_like_recorded_ones() {
        let start = 1_700_000_000_250_000;
        let (text, c) = sample(start).finish(0);
        let cast = Cast::parse(Path::new("x.cast"), text.as_bytes()).unwrap();
        let session = cast.session().expect("session marker at t=0");
        assert_eq!(session.str("uuid"), Some(c.uuid.as_str()));
        assert_eq!(session.u64("start_epoch_us"), Some(start));
        assert_eq!(cast.markers_of("cmd").len(), 3);
        assert_eq!(cast.markers_of("done").len(), 3);
        assert_eq!(cast.markers_of("skip").len(), 1);
        assert_eq!(cast.done_for(3).and_then(|d| d.i64("exit")), Some(1));
        assert_eq!(cast.exit_status(), Some("0"));
        assert!(cast.output().contains("frame two"));
        assert!(!cast.alt_screen_after(cast.events.len() - 1));
        assert_eq!(c.commands.len(), 3);
        assert_eq!(c.commands[0].output.lines().count(), 300);
        assert!(c.commands.windows(2).all(|w| w[0].t_end <= w[1].t_start));
        assert!(c.commands[2].t_start - c.commands[1].t_end >= 5.0, "idle time");
        // Every output read is at most 4 KiB and never splits a UTF-8 sequence.
        assert!(text.lines().skip(1).all(|l| l.len() < 4096 * 6 + 64));
    }

    #[test]
    fn corpus_is_deterministic_in_shape() {
        let d = crate::util::scratch_dir().unwrap();
        let spec = CorpusSpec {
            seed: 3,
            casts: 3,
            commands: 30,
            max_lines: 20,
            newest_start_us: 1_700_000_000_000_000,
        };
        let a = corpus(&d.path().join("a"), &spec).unwrap();
        let b = corpus(&d.path().join("b"), &spec).unwrap();
        assert_eq!(a.len(), 3);
        for ((pa, ca), (pb, cb)) in a.iter().zip(&b) {
            assert_eq!(ca.commands, cb.commands);
            assert_eq!(fs::metadata(pa).unwrap().len(), fs::metadata(pb).unwrap().len());
            let cast = Cast::load(pa).unwrap();
            assert_eq!(cast.markers_of("done").len(), ca.commands.len());
            assert_eq!(cast.bad_lines, 0);
        }
        assert!(a[0].1.start_epoch_us > a[1].1.start_epoch_us);
    }

    #[test]
    fn rng_and_uuid() {
        let mut a = Rng::new(1);
        let mut b = Rng::new(1);
        assert_eq!(a.lines(5, 40), b.lines(5, 40));
        assert!(a.lines(20, 40).lines().all(|l| l.chars().count() <= 40));
        let u = uuid_v7(1_700_000_000_123);
        let re = regex::Regex::new("^[0-9a-f]{8}-[0-9a-f]{4}-7[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$").unwrap();
        assert!(re.is_match(&u), "{u}");
        assert!(u.starts_with("018bcfe5"), "{u}: the time prefix");
    }

    #[test]
    fn write_uses_the_recorder_layout() {
        let d = crate::util::scratch_dir().unwrap();
        let start = 1_700_000_000_000_000;
        let (path, c) = sample(start).write(d.path(), 0).unwrap();
        let (month, stamp) = local_time(start);
        assert_eq!(path.parent().unwrap(), d.path().join(&month));
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        assert!(name.starts_with(&format!("{}_{stamp}_", c.host)), "{name}");
        assert!(name.ends_with(&format!("{}.cast", &c.uuid[c.uuid.len() - 8..])));
        use std::os::unix::fs::MetadataExt;
        let mtime = fs::metadata(&path).unwrap().mtime();
        assert_eq!(mtime as u64, (start + c.t_us) / 1_000_000);
        assert!(Cast::load(&path).unwrap().session().is_some());
    }
}
