//! Running `cinelog` subcommands and the indexer daemon (`serve --no-http`) inside a sandbox,
//! for the M2 scenarios (I11–I14).
//!
//! Every invocation uses the sandbox environment (`env -i` semantics), so the binary finds the
//! sandbox config, logs, state and runtime dirs exactly like a sandboxed shell would.

use std::fs;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use nix::sys::signal::{Signal, killpg};
use nix::unistd::Pid;
use serde_json::Value;

use crate::sandbox::Sandbox;
use crate::util::{output_with_timeout, scaled};

/// What the pre-integration stand-ins of the M2 contracts print (`IndexError::Unavailable`
/// and the `unavailable()` bodies of `serve`/`search`): the component has not landed yet.
pub const STUB_MARK: &str = "is not available in this build yet";

/// Captured result of one `cinelog` invocation.
#[derive(Debug, Clone)]
pub struct CliOutput {
    pub args: Vec<String>,
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

impl CliOutput {
    pub fn ok(&self) -> bool {
        self.code == Some(0)
    }

    /// The subcommand (or a component it needs) is still a contracts stand-in.
    pub fn is_stub(&self) -> bool {
        self.stdout.contains(STUB_MARK) || self.stderr.contains(STUB_MARK)
    }

    /// Fails unless the exit status is one of `codes`.
    pub fn expect(self, codes: &[i32]) -> Result<Self> {
        match self.code {
            Some(c) if codes.contains(&c) => Ok(self),
            _ => bail!("{self}\nexpected exit status {codes:?}"),
        }
    }

    pub fn expect_ok(self) -> Result<Self> {
        self.expect(&[0])
    }

    /// stdout as JSON Lines (blank lines ignored).
    pub fn json_lines(&self) -> Result<Vec<Value>> {
        self.stdout
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| {
                serde_json::from_str(l).with_context(|| format!("JSON line {l:?} of `cinelog {}`", self.args.join(" ")))
            })
            .collect()
    }

    /// stdout as one JSON document.
    pub fn json(&self) -> Result<Value> {
        serde_json::from_str(&self.stdout).with_context(|| format!("JSON output of `cinelog {}`", self.args.join(" ")))
    }
}

impl std::fmt::Display for CliOutput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "`cinelog {}` exited {:?}\n--- stdout\n{}\n--- stderr\n{}",
            self.args.join(" "),
            self.code,
            self.stdout,
            self.stderr
        )
    }
}

/// `sanitize_component(hostname)` as `cinelog-core` names the per-host DB dir (§6.1).
pub fn host_component() -> String {
    let host = nix::unistd::gethostname()
        .ok()
        .and_then(|h| h.into_string().ok())
        .filter(|h| !h.is_empty())
        .unwrap_or_else(|| "unknown".into());
    let out: String = host
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '-'
            }
        })
        .collect();
    if out.is_empty() || out == "." || out == ".." {
        "unknown".into()
    } else {
        out
    }
}

/// How long one CLI invocation may take (backfills of the scenario corpora included).
fn cli_timeout() -> Duration {
    scaled(300.0)
}

impl Sandbox {
    /// Runs `cinelog <args>` with the sandbox environment in the sandbox work dir.
    pub fn cinelog(&self, args: &[&str]) -> Result<CliOutput> {
        self.cinelog_in(&self.work, args)
    }

    pub fn cinelog_in(&self, cwd: &Path, args: &[&str]) -> Result<CliOutput> {
        let bin = self.bin.as_ref().context("this sandbox has no cinelog binary")?;
        let mut cmd = Command::new(bin);
        cmd.env_clear()
            .envs(self.env())
            .env("PWD", cwd)
            .current_dir(cwd)
            .args(args)
            .stdin(Stdio::null());
        let out = output_with_timeout(cmd, cli_timeout()).with_context(|| format!("cinelog {}", args.join(" ")))?;
        Ok(CliOutput {
            args: args.iter().map(|s| (*s).to_owned()).collect(),
            code: out.status.code(),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        })
    }

    /// `cinelog search --json <args>`: the hits (exit 0 with hits, 1 without any).
    pub fn search_json(&self, args: &[&str]) -> Result<Vec<Value>> {
        let mut v = vec!["search", "--json"];
        v.extend_from_slice(args);
        let out = self.cinelog(&v)?;
        if out.is_stub() {
            bail!("{out}\nsearch is still a stand-in");
        }
        out.expect(&[0, 1])?.json_lines()
    }

    /// Polls `cinelog search --json <args>` until a hit satisfies `pred`; returns it and
    /// how long that took.
    pub fn wait_hit(
        &self,
        args: &[&str],
        timeout: Duration,
        pred: impl Fn(&Value) -> bool,
    ) -> Result<(Value, Duration)> {
        let start = Instant::now();
        let mut last = Vec::new();
        loop {
            let error = match self.search_json(args) {
                Ok(hits) => {
                    last = hits;
                    None
                }
                Err(e) => Some(e),
            };
            if let Some(h) = last.iter().find(|h| pred(h)) {
                return Ok((h.clone(), start.elapsed()));
            }
            if start.elapsed() >= timeout {
                let why = error.map_or_else(String::new, |e| format!("; the last search failed: {e:#}"));
                bail!(
                    "no matching hit for `cinelog search --json {}` within {:.1}s; last hits: {last:?}{why}",
                    args.join(" "),
                    timeout.as_secs_f64()
                );
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// `cinelog status --json`.
    pub fn status_json(&self) -> Result<Value> {
        self.cinelog(&["status", "--json"])?.expect_ok()?.json()
    }

    /// The per-host DB dir below the sandbox's default `paths.state`.
    pub fn db_dir(&self) -> PathBuf {
        db_dir_in(&self.state_dir())
    }

    pub fn index_db(&self) -> PathBuf {
        self.db_dir().join("index.db")
    }
}

/// The per-host DB dir below `state` (`paths.state`).
pub fn db_dir_in(state: &Path) -> PathBuf {
    state.join(host_component())
}

/// A `cinelog serve` daemon of a sandbox (`--no-http` unless started with
/// [`Serve::start_web`]), in its own process group. Dropping it kills the group.
#[derive(Debug)]
pub struct Serve {
    child: Child,
    pub log: PathBuf,
}

impl Serve {
    /// Starts `cinelog serve --no-http <extra>`; its stdout and stderr go to `work/<name>.log`.
    pub fn start(sb: &Sandbox, name: &str, extra: &[&str]) -> Result<Self> {
        let mut args = vec!["--no-http"];
        args.extend_from_slice(extra);
        Self::spawn(sb, name, &args)
    }

    /// Starts `cinelog serve <extra>` with the web server (the sandbox config binds it to an
    /// ephemeral loopback port).
    pub fn start_web(sb: &Sandbox, name: &str, extra: &[&str]) -> Result<Self> {
        Self::spawn(sb, name, extra)
    }

    fn spawn(sb: &Sandbox, name: &str, args: &[&str]) -> Result<Self> {
        let bin = sb.bin.as_ref().context("this sandbox has no cinelog binary")?;
        let log = sb.work.join(format!("{name}.log"));
        let out = fs::File::create(&log)?;
        let child = Command::new(bin)
            .env_clear()
            .envs(sb.env())
            .current_dir(&sb.work)
            .arg("serve")
            .args(args)
            .stdin(Stdio::null())
            .stdout(out.try_clone()?)
            .stderr(out)
            .process_group(0)
            .spawn()
            .context("spawning cinelog serve")?;
        Ok(Self { child, log })
    }

    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    /// The exit status, once the daemon has exited.
    pub fn exited(&mut self) -> Option<ExitStatus> {
        self.child.try_wait().ok().flatten()
    }

    pub fn log_text(&self) -> String {
        fs::read_to_string(&self.log).unwrap_or_default()
    }

    fn signal_group(&self, sig: Signal) {
        let _ = killpg(Pid::from_raw(self.child.id() as i32), sig);
    }

    /// SIGKILL, as a crash or an OOM kill would end it (no cleanup, no final commit).
    pub fn kill_hard(mut self) -> Result<()> {
        self.signal_group(Signal::SIGKILL);
        self.child.wait()?;
        Ok(())
    }

    /// SIGTERM and a clean exit within `grace`.
    pub fn stop(mut self, grace: Duration) -> Result<ExitStatus> {
        self.signal_group(Signal::SIGTERM);
        let deadline = Instant::now() + grace;
        loop {
            if let Some(st) = self.child.try_wait()? {
                return Ok(st);
            }
            if Instant::now() >= deadline {
                self.signal_group(Signal::SIGKILL);
                let _ = self.child.wait();
                bail!(
                    "cinelog serve ignored SIGTERM for {:.1}s\n{}",
                    grace.as_secs_f64(),
                    self.log_text()
                );
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

impl Drop for Serve {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            self.signal_group(Signal::SIGKILL);
            let _ = self.child.wait();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn out(code: Option<i32>, stdout: &str, stderr: &str) -> CliOutput {
        CliOutput {
            args: vec!["x".into()],
            code,
            stdout: stdout.into(),
            stderr: stderr.into(),
        }
    }

    #[test]
    fn stub_and_exit_checks() {
        let stub = out(
            Some(2),
            "",
            "cinelog: index: the index engine is not available in this build yet\n",
        );
        assert!(stub.is_stub());
        assert!(stub.clone().expect_ok().is_err());
        assert!(stub.expect(&[2]).is_ok());
        let ok = out(Some(0), "{\"a\":1}\n\n{\"a\":2}\n", "");
        assert!(!ok.is_stub());
        assert_eq!(ok.json_lines().unwrap().len(), 2);
        assert!(out(None, "", "").expect(&[0, 1]).is_err(), "killed by a signal");
    }

    #[test]
    fn host_component_is_a_safe_path_component() {
        let h = host_component();
        assert!(!h.is_empty() && !h.contains('/') && h != "." && h != "..");
    }

    #[test]
    fn serve_guard_kills_its_group() {
        use std::os::unix::fs::PermissionsExt;
        let mut sb = Sandbox::with(crate::SandboxOptions {
            flavour: None,
            ..Default::default()
        })
        .unwrap();
        // Any long-running program with a child stands in for the daemon.
        let fake = sb.work.join("fake-serve");
        fs::write(&fake, "#!/bin/sh\nsleep 30 &\nexec sleep 30\n").unwrap();
        fs::set_permissions(&fake, fs::Permissions::from_mode(0o755)).unwrap();
        sb.bin = Some(fake);
        let mut s = Serve::start(&sb, "serve", &[]).unwrap();
        let pid = s.pid();
        assert!(s.exited().is_none());
        drop(s);
        assert!(crate::procs::wait_gone(pid, Duration::from_secs(5)));
        assert!(
            crate::util::wait_until("the group to die", Duration::from_secs(5), || sb
                .processes()
                .is_empty()
                .then_some(()))
            .is_ok()
        );
    }
}
