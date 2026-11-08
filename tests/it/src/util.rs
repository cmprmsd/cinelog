//! Small helpers: scaled timeouts, polling, ids, PATH lookup, shell quoting.

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

/// Poll interval of [`wait_until`].
const POLL: Duration = Duration::from_millis(50);

/// `CINELOG_IT_TIMEOUT_SCALE` (default 1.0) multiplies every harness timeout, for slow runners.
pub fn timeout_scale() -> f64 {
    std::env::var("CINELOG_IT_TIMEOUT_SCALE")
        .ok()
        .and_then(|s| s.parse::<f64>().ok())
        .filter(|f| f.is_finite() && *f > 0.0)
        .unwrap_or(1.0)
}

/// `secs` seconds, multiplied by [`timeout_scale`].
pub fn scaled(secs: f64) -> Duration {
    Duration::from_secs_f64(secs * timeout_scale())
}

/// Polls `f` until it returns `Some`, or fails after `timeout` with a message naming `what`.
pub fn wait_until<T>(what: &str, timeout: Duration, mut f: impl FnMut() -> Option<T>) -> Result<T> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(v) = f() {
            return Ok(v);
        }
        if Instant::now() >= deadline {
            bail!("timed out after {:.1}s waiting for {what}", timeout.as_secs_f64());
        }
        std::thread::sleep(POLL);
    }
}

/// Sleeps for a fixed (unscaled) duration; used for "nothing happens within X" checks.
pub fn settle(d: Duration) {
    std::thread::sleep(d);
}

/// How long [`output_with_timeout`] waits, after the child exited, for its stdout and stderr
/// to reach EOF before it blames a descendant that inherited them.
const PIPE_GRACE: f64 = 3.0;

/// Runs `cmd` to completion with captured output, in its own process group. Fails (after
/// killing the whole group) when the child is still running after `timeout`, or when it
/// exited but a descendant keeps its stdout or stderr open: a background job left behind
/// must make the scenario fail, not hang it.
pub fn output_with_timeout(mut cmd: Command, timeout: Duration) -> Result<Output> {
    use std::io::Read;
    use std::os::unix::process::CommandExt;
    use std::sync::mpsc;

    let mut child = cmd
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .spawn()
        .context("spawning command")?;
    let pgid = child.id();
    let kill_group = || {
        let _ = nix::sys::signal::killpg(
            nix::unistd::Pid::from_raw(pgid as i32),
            nix::sys::signal::Signal::SIGKILL,
        );
    };

    let (tx, rx) = mpsc::channel::<(bool, Vec<u8>)>();
    let spawn_reader = |is_err: bool, pipe: Option<Box<dyn Read + Send>>| {
        let tx = tx.clone();
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            if let Some(mut p) = pipe {
                let _ = p.read_to_end(&mut buf);
            }
            let _ = tx.send((is_err, buf));
        });
    };
    spawn_reader(false, child.stdout.take().map(|p| Box::new(p) as Box<dyn Read + Send>));
    spawn_reader(true, child.stderr.take().map(|p| Box::new(p) as Box<dyn Read + Send>));
    drop(tx);

    let deadline = Instant::now() + timeout;
    let status = loop {
        if let Some(st) = child.try_wait()? {
            break st;
        }
        if Instant::now() >= deadline {
            kill_group();
            let _ = child.wait();
            bail!("command did not finish within {:.1}s", timeout.as_secs_f64());
        }
        std::thread::sleep(Duration::from_millis(10));
    };

    let pipe_deadline = Instant::now() + scaled(PIPE_GRACE);
    let (mut stdout, mut stderr) = (None, None);
    while stdout.is_none() || stderr.is_none() {
        let left = pipe_deadline.saturating_duration_since(Instant::now());
        match rx.recv_timeout(left) {
            Ok((false, b)) => stdout = Some(b),
            Ok((true, b)) => stderr = Some(b),
            Err(_) => {
                kill_group();
                bail!(
                    "command exited ({status}), but a descendant still held its {} open {:.1}s later",
                    match (stdout.is_none(), stderr.is_none()) {
                        (true, true) => "stdout and stderr",
                        (true, false) => "stdout",
                        _ => "stderr",
                    },
                    scaled(PIPE_GRACE).as_secs_f64()
                );
            }
        }
    }
    // Members of the group that closed their pipes but linger are not the caller's business.
    kill_group();
    Ok(Output {
        status,
        stdout: stdout.unwrap_or_default(),
        stderr: stderr.unwrap_or_default(),
    })
}

/// `CINELOG_IT_TMPDIR`, else the system temp dir: where sandboxes and probe dirs live.
pub fn scratch_base() -> PathBuf {
    std::env::var_os("CINELOG_IT_TMPDIR")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
}

/// A fresh `clit-*` directory below [`scratch_base`], removed when dropped.
pub fn scratch_dir() -> Result<tempfile::TempDir> {
    let base = scratch_base();
    std::fs::create_dir_all(&base).with_context(|| format!("creating {}", base.display()))?;
    tempfile::Builder::new()
        .prefix("clit-")
        .tempdir_in(&base)
        .with_context(|| format!("creating a temp dir in {}", base.display()))
}

/// 8 random lowercase hex chars.
pub fn unique_id() -> String {
    random_hex(4)
}

pub fn random_hex(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    getrandom::fill(&mut buf).expect("getrandom");
    buf.iter().map(|b| format!("{b:02x}")).collect()
}

/// First executable `name` on `$PATH` of the test process.
pub fn which(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|d| d.join(name))
        .find(|p| is_executable(p))
}

pub fn is_executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    p.metadata()
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

/// POSIX single-quote quoting.
pub fn sh_quote(s: &str) -> String {
    if !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"/._-+=:,@%".contains(&b))
    {
        return s.to_owned();
    }
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// Screen lines that are cinelog warnings: every non-prompt line mentioning cinelog or
/// "not recording", except the informational session line of the MOTD
/// (`cinelog: session <id8> → <cast>`) and its disk-space line (which names a mount point
/// that may well contain "cinelog").
pub fn warning_lines(screen: &str) -> Vec<String> {
    static RE: std::sync::OnceLock<(regex::Regex, regex::Regex)> = std::sync::OnceLock::new();
    let (warn, info) = RE.get_or_init(|| {
        (
            regex::Regex::new(r"(?i)cinelog|not recording").expect("regex"),
            regex::Regex::new(r"^(cinelog: session [0-9a-f]{8}\b|Running out of disk space )").expect("regex"),
        )
    });
    screen
        .lines()
        .filter(|l| !crate::tmux::is_prompt_line(l) && !l.contains("CLITSYNC"))
        .filter(|l| warn.is_match(l) && !info.is_match(l))
        .map(str::to_owned)
        .collect()
}

/// Base64 (standard alphabet, no padding), as the protocol's `enc=b`.
pub fn base64_nopad(data: &[u8]) -> String {
    const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let n = chunk
            .iter()
            .enumerate()
            .fold(0u32, |acc, (i, b)| acc | u32::from(*b) << (16 - 8 * i));
        for i in 0..=chunk.len() {
            out.push(A[(n >> (18 - 6 * i) & 63) as usize] as char);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_rfc4648_without_padding() {
        assert_eq!(base64_nopad(b""), "");
        assert_eq!(base64_nopad(b"f"), "Zg");
        assert_eq!(base64_nopad(b"fo"), "Zm8");
        assert_eq!(base64_nopad(b"foo"), "Zm9v");
        assert_eq!(base64_nopad(b"foobar"), "Zm9vYmFy");
        assert_eq!(base64_nopad(&[0xff, 0xfe]), "//4");
    }

    #[test]
    fn quoting() {
        assert_eq!(sh_quote("/usr/bin/zsh"), "/usr/bin/zsh");
        assert_eq!(sh_quote("a b"), "'a b'");
        assert_eq!(sh_quote("it's"), r"'it'\''s'");
        assert_eq!(sh_quote(""), "''");
    }

    #[test]
    fn wait_until_times_out() {
        let mut n = 0;
        assert_eq!(
            wait_until("n", Duration::from_secs(1), || {
                n += 1;
                (n == 3).then_some(n)
            })
            .unwrap(),
            3
        );
        assert!(wait_until::<()>("never", Duration::from_millis(120), || None).is_err());
    }

    #[test]
    fn warnings_skip_prompts_sync_lines_and_the_session_line() {
        let screen = "cinelog: session 0123abcd → /x.cast\n\
                      clit% cinelog resume\n\
                      CLITSYNC12\n\
                      cinelog: not recording: error\n\
                      Uptime: up 3 hours\n\
                      Running out of disk space \"/tmp/cinelog-x (100%)\"\n";
        assert_eq!(warning_lines(screen), vec!["cinelog: not recording: error".to_owned()]);
    }

    #[test]
    fn output_with_timeout_captures_both_streams() {
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "echo out; echo err >&2; exit 3"]);
        let out = output_with_timeout(cmd, Duration::from_secs(10)).unwrap();
        assert_eq!(out.status.code(), Some(3));
        assert_eq!(out.stdout, b"out\n");
        assert_eq!(out.stderr, b"err\n");
    }

    #[test]
    fn output_with_timeout_kills_a_hanging_child() {
        let mut cmd = Command::new("sleep");
        cmd.arg("30");
        let start = Instant::now();
        let err = output_with_timeout(cmd, Duration::from_millis(300)).unwrap_err();
        assert!(err.to_string().contains("did not finish"), "{err:#}");
        assert!(start.elapsed() < Duration::from_secs(10));
    }

    /// Regression: a background job that inherits the pipes used to block the reader joins
    /// until it exited, hanging the test instead of failing it.
    #[test]
    fn output_with_timeout_fails_fast_on_a_descendant_holding_the_pipes() {
        let marker = unique_id();
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "CLIT_OWT=\"$1\" sleep 30 & echo started", "sh", &marker]);
        let start = Instant::now();
        let err = output_with_timeout(cmd, Duration::from_secs(60)).unwrap_err();
        assert!(err.to_string().contains("held its stdout and stderr open"), "{err:#}");
        assert!(start.elapsed() < scaled(PIPE_GRACE) + Duration::from_secs(5));
        wait_until("the background job to be killed", Duration::from_secs(5), || {
            crate::procs::with_env("CLIT_OWT", &marker).is_empty().then_some(())
        })
        .unwrap();
    }

    #[test]
    fn ids_are_hex() {
        let id = unique_id();
        assert_eq!(id.len(), 8);
        assert!(id.bytes().all(|b| b.is_ascii_hexdigit()));
        assert_ne!(id, unique_id());
    }
}
