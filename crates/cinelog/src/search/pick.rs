//! The fzf picker of `cinelog search` (§9.5): the hidden first field is the entry id, the
//! preview runs `cinelog show {1}`.
//!
//! The user's `FZF_DEFAULT_OPTS` (colours, layout, keys) stay in effect. The options that
//! change what fzf reads or prints are neutralised: `--multi` and `--print-query` are
//! switched off on the command line, `--read0` is honoured by feeding NUL-terminated lines,
//! and of the output (`--expect` key, `--print0`) the last record is the choice.

use std::ffi::OsString;
use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::thread::JoinHandle;

use super::text::shell_quote;

/// Result of an interactive pick.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Picked {
    /// The chosen line, as it was given.
    Line(String),
    /// The query typed in the picker matched nothing.
    NoMatch,
    /// Esc / Ctrl-C.
    Aborted,
}

/// An interactive picker that shows lines while they are still being produced.
pub trait Pick {
    fn start(&self) -> io::Result<Box<dyn Picking>>;
}

/// A running pick. Dropping it before [`Picking::finish`] ends the picker.
pub trait Picking {
    /// Shows `lines` (one batch); `false` once the picker no longer reads (it has exited).
    fn feed(&mut self, lines: &[String]) -> io::Result<bool>;
    /// Ends the input and waits for the choice.
    fn finish(self: Box<Self>) -> io::Result<Picked>;
}

/// fzf found on `$PATH`.
#[derive(Debug, Clone)]
pub struct Fzf {
    bin: PathBuf,
    /// This binary, for the preview command.
    exe: PathBuf,
    /// The user's fzf defaults turn on `--read0`: lines are fed NUL-terminated.
    read0: bool,
}

impl Fzf {
    /// `None` when fzf is not installed or there is no terminal to draw on.
    pub fn find() -> Option<Self> {
        std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/tty")
            .ok()?;
        let bin = find_in_path("fzf", &std::env::var_os("PATH")?)?;
        let exe = std::env::current_exe()
            .ok()
            .filter(|p| p.is_file())
            .unwrap_or_else(|| PathBuf::from("cinelog"));
        let read0 = default_opts().is_some_and(|o| reads_nul(&o));
        Some(Self { bin, exe, read0 })
    }

    /// fzf's arguments (§9.5).
    pub fn args(&self) -> Vec<OsString> {
        let preview = format!("{} show {{1}}", shell_quote(&self.exe.to_string_lossy()));
        [
            "--ansi",
            "--delimiter",
            "\t",
            "--with-nth",
            "2..",
            "--preview",
            &preview,
            "--preview-window",
            "right,50%",
            "--no-multi",
            "--no-print-query",
        ]
        .into_iter()
        .map(OsString::from)
        .collect()
    }
}

impl Pick for Fzf {
    fn start(&self) -> io::Result<Box<dyn Picking>> {
        let mut child = Command::new(&self.bin)
            .args(self.args())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()?;
        let stdin = child.stdin.take();
        let mut stdout = child.stdout.take().expect("stdout is piped");
        // Read concurrently so fzf never blocks on a full stdout pipe while we feed it.
        let choice = std::thread::spawn(move || {
            let mut buf = Vec::new();
            stdout.read_to_end(&mut buf).map(|_| buf)
        });
        Ok(Box::new(FzfRun {
            child,
            stdin,
            sep: if self.read0 { b'\0' } else { b'\n' },
            choice: Some(choice),
        }))
    }
}

struct FzfRun {
    child: Child,
    /// `None` once closed, or once fzf stopped reading.
    stdin: Option<ChildStdin>,
    /// Line terminator of the feed.
    sep: u8,
    choice: Option<JoinHandle<io::Result<Vec<u8>>>>,
}

impl Picking for FzfRun {
    fn feed(&mut self, lines: &[String]) -> io::Result<bool> {
        let Some(stdin) = &mut self.stdin else {
            return Ok(false);
        };
        let mut batch = Vec::with_capacity(lines.iter().map(|l| l.len() + 1).sum());
        for l in lines {
            batch.extend_from_slice(l.as_bytes());
            batch.push(self.sep);
        }
        match stdin.write_all(&batch) {
            Ok(()) => Ok(true),
            // fzf exits as soon as a line is chosen, possibly before reading everything.
            Err(e) if e.kind() == io::ErrorKind::BrokenPipe => {
                self.stdin = None;
                Ok(false)
            }
            Err(e) => Err(e),
        }
    }

    fn finish(mut self: Box<Self>) -> io::Result<Picked> {
        self.stdin = None;
        let stdout = self
            .choice
            .take()
            .expect("finish runs once")
            .join()
            .map_err(|_| io::Error::other("fzf reader thread panicked"))??;
        let status = self.child.wait()?;
        interpret(status.code(), &stdout)
    }
}

impl Drop for FzfRun {
    fn drop(&mut self) {
        if self.choice.is_some() {
            self.stdin = None;
            let _ = self.child.kill();
            let _ = self.child.wait();
            if let Some(t) = self.choice.take() {
                let _ = t.join();
            }
        }
    }
}

/// The user's fzf defaults: `$FZF_DEFAULT_OPTS_FILE`'s content, then `$FZF_DEFAULT_OPTS`.
fn default_opts() -> Option<String> {
    let file = std::env::var_os("FZF_DEFAULT_OPTS_FILE").and_then(|f| std::fs::read_to_string(f).ok());
    let env = std::env::var("FZF_DEFAULT_OPTS").ok();
    match (file, env) {
        (None, None) => None,
        (f, e) => Some(format!("{} {}", f.unwrap_or_default(), e.unwrap_or_default())),
    }
}

/// Whether fzf options leave `--read0` on (the last of `--read0` / `--no-read0` wins).
fn reads_nul(opts: &str) -> bool {
    opts.split_whitespace()
        .map(|w| w.trim_matches(|c| c == '\'' || c == '"'))
        .fold(false, |on, w| match w {
            "--read0" => true,
            "--no-read0" => false,
            _ => on,
        })
}

/// fzf exit status: 0 selected, 1 no match, 130 interrupted, 2 error (reported on stderr).
/// The choice is the last record: an `--expect` key line comes before it.
fn interpret(status: Option<i32>, stdout: &[u8]) -> io::Result<Picked> {
    let out = String::from_utf8_lossy(stdout);
    let line = out.split(['\n', '\0']).rfind(|l| !l.is_empty()).unwrap_or_default();
    match status {
        Some(0) if !line.is_empty() => Ok(Picked::Line(line.to_owned())),
        Some(0 | 1) => Ok(Picked::NoMatch),
        Some(130) => Ok(Picked::Aborted),
        Some(c) => Err(io::Error::other(format!("fzf failed with exit status {c}"))),
        None => Err(io::Error::other("fzf was killed by a signal")),
    }
}

fn find_in_path(name: &str, path: &std::ffi::OsStr) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    std::env::split_paths(path)
        .filter(|d| d.is_absolute())
        .map(|d| d.join(name))
        .find(|p| std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0))
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    #[test]
    fn arguments_hide_the_id_and_preview_with_show() {
        let f = Fzf {
            bin: "/usr/bin/fzf".into(),
            exe: "/home/a b/.zim/modules/cinelog/bin/cinelog".into(),
            read0: false,
        };
        let args: Vec<String> = f.args().into_iter().map(|a| a.into_string().unwrap()).collect();
        assert_eq!(
            args,
            [
                "--ansi",
                "--delimiter",
                "\t",
                "--with-nth",
                "2..",
                "--preview",
                "'/home/a b/.zim/modules/cinelog/bin/cinelog' show {1}",
                "--preview-window",
                "right,50%",
                "--no-multi",
                "--no-print-query",
            ]
        );
    }

    #[test]
    fn exit_statuses() {
        let p = |c, out: &[u8]| interpret(c, out).map_err(|e| e.to_string());
        assert_eq!(p(Some(0), b"u:1\tx\n"), Ok(Picked::Line("u:1\tx".into())));
        assert_eq!(p(Some(0), b""), Ok(Picked::NoMatch));
        assert_eq!(p(Some(1), b""), Ok(Picked::NoMatch));
        assert_eq!(p(Some(130), b""), Ok(Picked::Aborted));
        assert_eq!(p(Some(2), b""), Err("fzf failed with exit status 2".into()));
        assert!(p(None, b"").is_err());
    }

    /// Regression: `--expect` / `--print0` in `FZF_DEFAULT_OPTS` put other records around
    /// the choice; the first line used to be taken.
    #[test]
    fn the_choice_is_the_last_record() {
        let p = |out: &[u8]| interpret(Some(0), out).unwrap();
        assert_eq!(p(b"\nu:1\tx\n"), Picked::Line("u:1\tx".into()), "--expect, Enter");
        assert_eq!(p(b"ctrl-x\nu:1\tx\n"), Picked::Line("u:1\tx".into()));
        assert_eq!(p(b"u:1\tx\0"), Picked::Line("u:1\tx".into()), "--print0");
        assert_eq!(p(b"\n\n"), Picked::NoMatch);
    }

    #[test]
    fn read0_in_the_defaults() {
        assert!(!reads_nul("--height 40% --layout=reverse"));
        assert!(reads_nul("--height 40% --read0"));
        assert!(reads_nul("'--read0' --ansi"));
        assert!(!reads_nul("--read0 --no-read0"));
        assert!(!reads_nul(""));
    }

    #[test]
    fn path_lookup() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("fzf");
        std::fs::write(&bin, "#!/bin/sh\n").unwrap();
        let path = std::env::join_paths(["relative", dir.path().to_str().unwrap()]).unwrap();
        assert_eq!(find_in_path("fzf", &path), None, "not executable");
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(find_in_path("fzf", &path), Some(bin));
        assert_eq!(find_in_path("nope", &path), None);
    }

    fn stand_in(dir: &std::path::Path, script: &str) -> Fzf {
        let bin = dir.join("fzf");
        std::fs::write(&bin, format!("#!/bin/sh\n{script}\n")).unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        Fzf {
            bin,
            exe: "cinelog".into(),
            read0: false,
        }
    }

    /// Starts a stand-in. A test running in parallel may fork while the script is still open
    /// for writing; that child holds the write fd until it execs, so retry ETXTBSY briefly.
    fn start(f: &Fzf) -> Box<dyn Picking> {
        for _ in 0..200 {
            match f.start() {
                Err(e) if e.kind() == io::ErrorKind::ExecutableFileBusy => {
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
                other => return other.unwrap(),
            }
        }
        panic!("stand-in fzf stayed busy");
    }

    /// A stand-in fzf that picks the second line, to exercise the pipe plumbing.
    #[test]
    fn real_process_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let f = stand_in(dir.path(), "sed -n 2p");
        let lines: Vec<String> = (0..20_000).map(|i| format!("id{i}\tline {i}")).collect();
        let mut run = start(&f);
        for batch in lines.chunks(500) {
            assert!(run.feed(batch).unwrap());
        }
        assert_eq!(run.finish().unwrap(), Picked::Line("id1\tline 1".into()));
    }

    #[test]
    fn read0_feeds_nul_terminated_lines() {
        let dir = tempfile::tempdir().unwrap();
        let mut f = stand_in(dir.path(), "tr '\\000\\n' '\\n#' | sed -n 2p");
        f.read0 = true;
        let mut run = start(&f);
        assert!(run.feed(&["id0\ta".to_owned(), "id1\tb".to_owned()]).unwrap());
        assert_eq!(run.finish().unwrap(), Picked::Line("id1\tb".into()));
    }

    /// fzf may choose (and exit) while lines are still coming: feeding then reports `false`.
    #[test]
    fn early_choice_stops_the_feed() {
        let dir = tempfile::tempdir().unwrap();
        let f = stand_in(dir.path(), "head -n 1");
        let mut run = start(&f);
        let batch: Vec<String> = (0..1000).map(|i| format!("id{i}\t{}", "x".repeat(200))).collect();
        let mut fed = 0;
        while run.feed(&batch).unwrap() {
            fed += 1;
            assert!(fed < 10_000, "the feed never noticed that fzf exited");
        }
        assert!(!run.feed(&batch).unwrap());
        match run.finish().unwrap() {
            Picked::Line(l) => assert!(l.starts_with("id0\t")),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn fzf_errors_are_errors() {
        let dir = tempfile::tempdir().unwrap();
        let f = stand_in(dir.path(), "cat >/dev/null; exit 2");
        let mut run = start(&f);
        run.feed(&["a\tb".to_owned()]).unwrap();
        let e = run.finish().unwrap_err().to_string();
        assert!(e.contains("exit status 2"), "{e}");
    }

    #[test]
    fn dropping_a_running_pick_ends_fzf() {
        let dir = tempfile::tempdir().unwrap();
        let f = stand_in(dir.path(), "exec sleep 30");
        let started = std::time::Instant::now();
        drop(start(&f));
        assert!(started.elapsed() < std::time::Duration::from_secs(10));
    }
}
