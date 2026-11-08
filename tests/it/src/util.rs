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

/// Runs `cmd` to completion with captured output; kills it (and fails) after `timeout`.
pub fn output_with_timeout(mut cmd: Command, timeout: Duration) -> Result<Output> {
    use std::io::Read;
    let mut child = cmd
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("spawning command")?;
    let mut pipes = (child.stdout.take(), child.stderr.take());
    let reader = |p: Option<ChildStdoutOrErr>| {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            if let Some(mut p) = p {
                let _ = p.read_to_end(&mut buf);
            }
            buf
        })
    };
    let out_t = reader(pipes.0.take().map(ChildStdoutOrErr::Out));
    let err_t = reader(pipes.1.take().map(ChildStdoutOrErr::Err));
    let deadline = Instant::now() + timeout;
    let status = loop {
        if let Some(st) = child.try_wait()? {
            break st;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            bail!("command did not finish within {:.1}s", timeout.as_secs_f64());
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    Ok(Output {
        status,
        stdout: out_t.join().unwrap_or_default(),
        stderr: err_t.join().unwrap_or_default(),
    })
}

enum ChildStdoutOrErr {
    Out(std::process::ChildStdout),
    Err(std::process::ChildStderr),
}

impl std::io::Read for ChildStdoutOrErr {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self {
            Self::Out(p) => p.read(buf),
            Self::Err(p) => p.read(buf),
        }
    }
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
    fn ids_are_hex() {
        let id = unique_id();
        assert_eq!(id.len(), 8);
        assert!(id.bytes().all(|b| b.is_ascii_hexdigit()));
        assert_ne!(id, unique_id());
    }
}
