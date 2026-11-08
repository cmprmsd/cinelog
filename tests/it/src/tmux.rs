//! Driver for an isolated tmux server (`tmux -L <socket> -f /dev/null`).
//!
//! The harness only ever addresses servers by `-L` name with `TMUX_TMPDIR` pointing into
//! the sandbox's short socket dir, so it can never reach the user's own tmux.

use std::io::Write;
use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::{Context, Result, bail};

use crate::util::{scaled, wait_until};

#[derive(Debug, Clone)]
pub struct Tmux {
    pub socket: String,
    /// Environment for the tmux client processes (and so for a server they start).
    env: Vec<(String, String)>,
}

/// One pane, addressed by its stable `%N` id.
#[derive(Debug, Clone)]
pub struct Pane {
    pub tmux: Tmux,
    pub id: String,
    /// `#{pane_pid}`: the process tmux started, which execs into zsh and then the recorder.
    pub pid: u32,
}

impl Tmux {
    pub fn new(socket: impl Into<String>, env: Vec<(String, String)>) -> Self {
        Self {
            socket: socket.into(),
            env,
        }
    }

    fn command(&self) -> Command {
        let mut c = Command::new("tmux");
        c.env_clear()
            .envs(self.env.iter().map(|(k, v)| (k, v)))
            .args(["-L", &self.socket, "-f", "/dev/null"])
            .stdin(Stdio::null());
        c
    }

    /// Runs a tmux command and returns its stdout.
    pub fn run(&self, args: &[&str]) -> Result<String> {
        let out = self
            .command()
            .args(args)
            .output()
            .with_context(|| format!("tmux {args:?}"))?;
        if !out.status.success() {
            bail!(
                "tmux -L {} {:?} failed: {}",
                self.socket,
                args,
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    /// Starts the server with a keeper session (so it outlives the scenario's panes) and
    /// the options the harness relies on.
    pub fn start_server(&self) -> Result<()> {
        self.run(&["new-session", "-d", "-s", "keeper", "-x", "80", "-y", "24", "cat"])?;
        for opt in [
            ["set-option", "-g", "status", "off"],
            ["set-option", "-g", "history-limit", "50000"],
            ["set-option", "-s", "escape-time", "0"],
            ["set-option", "-g", "remain-on-exit", "off"],
        ] {
            self.run(&opt)?;
        }
        Ok(())
    }

    pub fn alive(&self) -> bool {
        self.command()
            .arg("has-session")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    }

    pub fn kill_server(&self) {
        let _ = self
            .command()
            .arg("kill-server")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }

    fn pane_from(&self, printed: &str) -> Result<Pane> {
        let mut it = printed.trim().split(' ');
        let (Some(id), Some(pid)) = (it.next(), it.next()) else {
            bail!("unexpected tmux pane line {printed:?}");
        };
        Ok(Pane {
            tmux: self.clone(),
            id: id.to_owned(),
            pid: pid.parse().context("pane pid")?,
        })
    }

    /// New detached session running `argv` directly (no shell in between).
    pub fn new_session(&self, name: &str, cols: u16, rows: u16, argv: &[String]) -> Result<Pane> {
        let (c, r) = (cols.to_string(), rows.to_string());
        let mut args = vec![
            "new-session",
            "-d",
            "-P",
            "-F",
            "#{pane_id} #{pane_pid}",
            "-s",
            name,
            "-x",
            &c,
            "-y",
            &r,
            "--",
        ];
        args.extend(argv.iter().map(String::as_str));
        let out = self.run(&args)?;
        self.pane_from(&out)
    }

    /// New window in `session` running `argv`.
    pub fn new_window(&self, session: &str, argv: &[String]) -> Result<Pane> {
        let target = format!("{session}:");
        let mut args = vec![
            "new-window",
            "-d",
            "-P",
            "-F",
            "#{pane_id} #{pane_pid}",
            "-t",
            &target,
            "--",
        ];
        args.extend(argv.iter().map(String::as_str));
        let out = self.run(&args)?;
        self.pane_from(&out)
    }

    /// Splits the pane `target`, running the server's default command in the new pane.
    pub fn split(&self, target: &str) -> Result<Pane> {
        let out = self.run(&["split-window", "-d", "-P", "-F", "#{pane_id} #{pane_pid}", "-t", target])?;
        self.pane_from(&out)
    }

    /// Every pane of the server as `(pane_id, pane_pid)`.
    pub fn panes(&self) -> Result<Vec<Pane>> {
        let out = self.run(&["list-panes", "-a", "-F", "#{pane_id} #{pane_pid}"])?;
        out.lines()
            .filter(|l| !l.is_empty())
            .map(|l| self.pane_from(l))
            .collect()
    }

    /// `(client_tty, client_pid)` of every attached client.
    pub fn clients(&self) -> Result<Vec<(String, u32)>> {
        let out = self.run(&["list-clients", "-F", "#{client_tty} #{client_pid}"])?;
        Ok(out
            .lines()
            .filter_map(|l| {
                let (tty, pid) = l.split_once(' ')?;
                Some((tty.to_owned(), pid.parse().ok()?))
            })
            .collect())
    }

    /// Pane of an existing server by id (e.g. one listed by [`Tmux::panes`]).
    pub fn pane(&self, target: &str) -> Result<Pane> {
        let out = self.run(&["display-message", "-p", "-t", target, "#{pane_id} #{pane_pid}"])?;
        self.pane_from(&out)
    }
}

impl Pane {
    /// Sends `text` literally (no key-name interpretation).
    pub fn send_text(&self, text: &str) -> Result<()> {
        self.tmux
            .run(&["send-keys", "-t", &self.id, "-l", "--", text])
            .map(|_| ())
    }

    /// Sends key names such as `Enter`, `C-c`, `C-d`.
    pub fn send_keys(&self, keys: &[&str]) -> Result<()> {
        let mut args = vec!["send-keys", "-t", &self.id];
        args.extend_from_slice(keys);
        self.tmux.run(&args).map(|_| ())
    }

    /// Types `line` and presses Enter.
    pub fn type_line(&self, line: &str) -> Result<()> {
        self.send_text(line)?;
        self.send_keys(&["Enter"])
    }

    /// Pastes `text` as one bracketed paste (multi-line input lands in one zle buffer).
    pub fn paste(&self, text: &str) -> Result<()> {
        let buffer = format!("clit-{}", crate::util::unique_id());
        let mut child = self
            .tmux
            .command()
            .args(["load-buffer", "-b", &buffer, "-"])
            .stdin(Stdio::piped())
            .spawn()?;
        child.stdin.take().expect("piped stdin").write_all(text.as_bytes())?;
        if !child.wait()?.success() {
            bail!("tmux load-buffer failed");
        }
        self.tmux
            .run(&["paste-buffer", "-p", "-d", "-b", &buffer, "-t", &self.id])
            .map(|_| ())
    }

    /// Visible screen plus scrollback, wrapped lines joined, trailing spaces trimmed.
    pub fn capture(&self) -> Result<String> {
        self.tmux.run(&["capture-pane", "-p", "-J", "-S", "-", "-t", &self.id])
    }

    pub fn display(&self, format: &str) -> Result<String> {
        Ok(self
            .tmux
            .run(&["display-message", "-p", "-t", &self.id, format])?
            .trim_end_matches('\n')
            .to_owned())
    }

    /// Waits until some screen line satisfies `pred`; returns the whole capture.
    pub fn wait_line(&self, what: &str, timeout: Duration, pred: impl Fn(&str) -> bool) -> Result<String> {
        wait_until(&format!("pane {} to show {what}", self.id), timeout, || {
            let cap = self.capture().ok()?;
            cap.lines().any(|l| pred(l.trim_end())).then_some(cap)
        })
        .with_context(|| format!("last screen of {}:\n{}", self.id, self.capture().unwrap_or_default()))
    }

    /// Waits until a screen line contains `needle`.
    pub fn wait_text(&self, needle: &str, timeout: Duration) -> Result<String> {
        self.wait_line(&format!("{needle:?}"), timeout, |l| l.contains(needle))
    }

    /// Waits for the harness prompt (`clit% `/`clit# `) at the end of the screen.
    pub fn wait_prompt(&self, timeout: Duration) -> Result<()> {
        wait_until(&format!("prompt in pane {}", self.id), timeout, || {
            let cap = self.capture().ok()?;
            let last = cap.lines().rev().find(|l| !l.trim().is_empty())?;
            is_prompt(last).then_some(())
        })
        .with_context(|| format!("last screen of {}:\n{}", self.id, self.capture().unwrap_or_default()))
    }

    /// Runs a command in an *unrecorded or recorded* shell and waits for it to finish, using
    /// a sync token printed after it (`print -r -- CLITSYNC''N` shows `CLITSYNCN` only as
    /// output, never in the echoed command line).
    pub fn run_sync(&self, line: &str, timeout: Duration) -> Result<String> {
        self.type_line(line)?;
        self.sync(timeout)
    }

    pub fn sync(&self, timeout: Duration) -> Result<String> {
        let n = crate::util::unique_id();
        self.type_line(&format!("print -r -- CLITSYNC''{n}"))?;
        let token = format!("CLITSYNC{n}");
        self.wait_line(&token, timeout, |l| l == token)
    }

    pub fn kill(&self) -> Result<()> {
        self.tmux.run(&["kill-pane", "-t", &self.id]).map(|_| ())
    }

    /// The pane still exists on its server.
    pub fn exists(&self) -> bool {
        self.tmux
            .run(&["list-panes", "-a", "-F", "#{pane_id}"])
            .is_ok_and(|o| o.lines().any(|l| l == self.id))
    }

    /// Default wait for a shell to come up.
    pub fn startup_timeout() -> Duration {
        scaled(15.0)
    }
}

/// A line that starts with the harness prompt (an echoed command line).
pub fn is_prompt_line(line: &str) -> bool {
    line.starts_with("clit%") || line.starts_with("clit#")
}

pub fn is_prompt(line: &str) -> bool {
    let l = line.trim_end();
    l.ends_with("clit%") || l.ends_with("clit#")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_detection() {
        assert!(is_prompt("clit% "));
        assert!(is_prompt("clit#"));
        assert!(!is_prompt("clit% ls"));
    }

    /// Drives a real isolated tmux server with a plain `zsh -f` (no cinelog involved).
    #[test]
    fn drives_an_isolated_server() {
        if crate::util::which("tmux").is_none() || crate::util::which("zsh").is_none() {
            eprintln!("SKIP: tmux or zsh missing");
            return;
        }
        let sb = crate::Sandbox::with(crate::SandboxOptions {
            flavour: None,
            ..Default::default()
        })
        .unwrap();
        let t = sb.tmux().unwrap();
        let argv: Vec<String> = ["env", "-i", "PATH=/usr/bin:/bin", "TERM=xterm-256color", "PS1=clit%# "]
            .iter()
            .map(|s| s.to_string())
            .chain(["zsh".to_string(), "-f".to_string()])
            .collect();
        let p = t.new_session("s", 100, 30, &argv).unwrap();
        p.wait_prompt(Duration::from_secs(10)).unwrap();
        p.run_sync("echo hello-$((40+2))", Duration::from_secs(10)).unwrap();
        assert!(p.capture().unwrap().lines().any(|l| l == "hello-42"));
        p.paste("echo pasted-one\necho pasted-two").unwrap();
        p.send_keys(&["Enter"]).unwrap();
        p.wait_line("pasted-two", Duration::from_secs(10), |l| l == "pasted-two")
            .unwrap();
        assert!(p.capture().unwrap().lines().any(|l| l == "pasted-one"));
        assert_eq!(p.display("#{pane_width}x#{pane_height}").unwrap(), "100x30");
        let pid = p.pid;
        assert!(crate::procs::alive(pid));
        p.kill().unwrap();
        assert!(crate::procs::wait_gone(pid, Duration::from_secs(5)));
        assert!(!p.exists());
        drop(sb);
        assert!(!t.alive(), "the sandbox drop kills its tmux servers");
    }
}
