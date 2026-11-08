//! Scenario prerequisites.
//!
//! A scenario declares what it needs with [`need!`](crate::need). A missing prerequisite
//! skips the scenario with a `SKIP` line on stderr, unless it is fatal:
//! - a *broken* component (the `cinelog` build fails, the recorder misbehaves) always fails;
//! - with `CINELOG_IT_REQUIRE=1` (CI) every missing tool or component fails;
//! - the opt-in prerequisites (root, the password-sudo user) fail when `CINELOG_IT_ROOT=1`
//!   asks for them, or under `CINELOG_IT_REQUIRE=1` in a `--features root` build; otherwise
//!   they skip.

use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::time::Duration;

use crate::binary::{self, Flavour};
use crate::util::which;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Need {
    Zsh,
    Tmux,
    Screen,
    Python,
    Script,
    /// curl with HTTP/2 (the web request matrix of I22).
    Curl,
    /// `unshare -rm` plus nested `unshare -U --map-user` (I8 full disk, I15).
    UserNs,
    /// A runnable OpenSSH server for the localhost relay case of I6.
    Sshd,
    /// The new plugin (protocol v1) is in the repo.
    Plugin,
    /// `cinelog` builds and its recorder records (not the M1-0 fallback stub).
    Recorder,
    /// The testhooks build of `cinelog` (I16).
    Testhooks,
    /// The M2 index engine: `cinelog index --once` works (not the M2-0 stand-in).
    Index,
    /// Nested user namespaces with a subordinate uid range (`newuidmap`, /etc/subuid), a
    /// private pid namespace with its own `/proc` (hidepid) and devpts instances (I17).
    ProcNs,
    /// Root scenarios: `--features root`, `CINELOG_IT_ROOT=1` and passwordless sudo.
    Root,
    /// A password-requiring sudo user: `CINELOG_IT_PW_USER` and `CINELOG_IT_PW_PASS` (I6).
    PwUser,
}

impl Need {
    /// Prerequisites of scenarios that only run when explicitly enabled.
    fn opt_in(self) -> bool {
        matches!(self, Self::Root | Self::PwUser)
    }
}

fn env_is_1(key: &str) -> bool {
    std::env::var(key).is_ok_and(|v| v == "1")
}

pub fn strict() -> bool {
    env_is_1("CINELOG_IT_REQUIRE")
}

/// `CINELOG_IT_ROOT=1`: the run asks for the root scenarios.
pub fn root_requested() -> bool {
    env_is_1("CINELOG_IT_ROOT")
}

/// Root scenarios are enabled for this run.
pub fn root_enabled() -> bool {
    cfg!(feature = "root") && root_requested()
}

/// The run settings that decide whether a missing prerequisite fails or skips.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Policy {
    pub strict: bool,
    pub root_requested: bool,
    pub root_feature: bool,
}

impl Policy {
    pub fn current() -> Self {
        Self {
            strict: strict(),
            root_requested: root_requested(),
            root_feature: cfg!(feature = "root"),
        }
    }

    pub fn fatal(self, need: Need, broken: bool) -> bool {
        if broken {
            return true;
        }
        if need.opt_in() {
            self.root_requested || (self.strict && self.root_feature)
        } else {
            self.strict
        }
    }
}

/// Why a prerequisite check failed. `broken` marks a component that exists but does not
/// work (a failing build, a misbehaving recorder): that is a test failure, never a skip.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Failure {
    pub why: String,
    pub broken: bool,
}

impl Failure {
    fn broken(why: impl Into<String>) -> Self {
        Self {
            why: why.into(),
            broken: true,
        }
    }
}

impl From<String> for Failure {
    fn from(why: String) -> Self {
        Self { why, broken: false }
    }
}

impl From<&str> for Failure {
    fn from(why: &str) -> Self {
        why.to_owned().into()
    }
}

/// Outcome of a prerequisite check.
#[derive(Debug)]
pub struct Missing {
    pub need: Need,
    pub why: String,
    pub broken: bool,
}

impl Missing {
    pub fn fatal(&self) -> bool {
        Policy::current().fatal(self.need, self.broken)
    }
}

pub fn check(needs: &[Need]) -> Result<(), Missing> {
    for &need in needs {
        if let Err(f) = check_one(need) {
            return Err(Missing {
                need,
                why: f.why,
                broken: f.broken,
            });
        }
    }
    Ok(())
}

fn tool(name: &str) -> Result<(), Failure> {
    which(name)
        .map(|_| ())
        .ok_or_else(|| format!("`{name}` is not installed").into())
}

fn check_one(need: Need) -> Result<(), Failure> {
    match need {
        Need::Zsh => tool("zsh"),
        Need::Tmux => tool("tmux"),
        Need::Screen => tool("screen"),
        Need::Python => tool("python3"),
        Need::Script => tool("script"),
        Need::Curl => tool("curl").and_then(|()| {
            let v = std::process::Command::new("curl")
                .arg("--version")
                .output()
                .map_err(|e| e.to_string())?;
            String::from_utf8_lossy(&v.stdout)
                .contains("HTTP2")
                .then_some(())
                .ok_or_else(|| "curl lacks HTTP/2".into())
        }),
        Need::UserNs => cached(&USERNS, probe_userns),
        Need::Sshd => cached(&SSHD, || crate::sshd::probe().map_err(Failure::from)),
        Need::Plugin => cached(&PLUGIN, probe_plugin),
        Need::Recorder => cached(&RECORDER, probe_recorder),
        Need::Index => cached(&INDEX, probe_index),
        Need::ProcNs => cached(&PROCNS, probe_procns),
        Need::Testhooks => cached(&TESTHOOKS, || {
            binary::cinelog(Flavour::Testhooks)
                .map(|_| ())
                .map_err(|e| Failure::broken(format!("{e:#}")))
        }),
        Need::Root => {
            if !cfg!(feature = "root") {
                return Err("root scenarios need `--features root`".into());
            }
            if !root_requested() {
                return Err("root scenarios need CINELOG_IT_ROOT=1 (throwaway machines only)".into());
            }
            tool("sudo")?;
            let ok = Command::new("sudo")
                .args(["-n", "true"])
                .stdin(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .is_ok_and(|s| s.success());
            ok.then_some(())
                .ok_or_else(|| "passwordless sudo is not available".into())
        }
        Need::PwUser => match (std::env::var("CINELOG_IT_PW_USER"), std::env::var("CINELOG_IT_PW_PASS")) {
            (Ok(u), Ok(p)) if !u.is_empty() && !p.is_empty() => Ok(()),
            _ => Err("set CINELOG_IT_PW_USER and CINELOG_IT_PW_PASS (a user with a password sudo rule)".into()),
        },
    }
}

type Probe = OnceLock<Result<(), Failure>>;
static USERNS: Probe = OnceLock::new();
static SSHD: Probe = OnceLock::new();
static PLUGIN: Probe = OnceLock::new();
static RECORDER: Probe = OnceLock::new();
static TESTHOOKS: Probe = OnceLock::new();
static INDEX: Probe = OnceLock::new();
static PROCNS: Probe = OnceLock::new();

fn cached(cell: &'static Probe, f: impl FnOnce() -> Result<(), Failure>) -> Result<(), Failure> {
    cell.get_or_init(f).clone()
}

fn probe_userns() -> Result<(), Failure> {
    tool("unshare")?;
    let dir = crate::util::scratch_dir().map_err(|e| Failure::broken(format!("{e:#}")))?;
    let (uid, gid) = (nix::unistd::getuid(), nix::unistd::getgid());
    let script = format!(
        "mount -t tmpfs -o size=64k tmpfs \"$1\" && exec unshare -U --map-user={uid} --map-group={gid} sh -c 'test \"$(id -u)\" = {uid}'"
    );
    let ok = Command::new("unshare")
        .args(["-rm", "sh", "-c", &script, "sh"])
        .arg(dir.path())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success());
    ok.then_some(())
        .ok_or_else(|| "unprivileged user+mount namespaces with --map-user are unavailable".into())
}

fn probe_plugin() -> Result<(), Failure> {
    let text = std::fs::read_to_string(binary::plugin_file()).map_err(|e| format!("cinelog.plugin.zsh: {e}"))?;
    if text.contains("CINELOG_NONCE_FD") {
        Ok(())
    } else {
        Err("cinelog.plugin.zsh is still the pre-rewrite plugin (no CINELOG_NONCE_FD handling)".into())
    }
}

/// Runs `cinelog rec -- /bin/sh -c 'printf "<%s>" "$CINELOG_FAILED"'` headless inside a
/// throwaway sandbox: the M1-0 stub falls back and prints `<recorder-unavailable>`. Only
/// that stub is a skip; a failing build or any other answer is a broken recorder.
fn probe_recorder() -> Result<(), Failure> {
    let bin = binary::cinelog(Flavour::Plain).map_err(|e| Failure::broken(format!("{e:#}")))?;
    let sb = crate::Sandbox::new().map_err(|e| Failure::broken(format!("sandbox: {e:#}")))?;
    let mut cmd = Command::new(&bin);
    cmd.env_clear()
        .envs(sb.env())
        .args(["rec", "--", "/bin/sh", "-c", "printf '<%s>' \"$CINELOG_FAILED\""])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let out = crate::util::output_with_timeout(cmd, Duration::from_secs(20))
        .map_err(|e| Failure::broken(format!("{e:#}")))?;
    let stdout = String::from_utf8_lossy(&out.stdout);
    if stdout.contains("<recorder-unavailable>") {
        return Err("`cinelog rec` is the M1-0 fallback stub (recorder not implemented yet)".into());
    }
    if !stdout.contains("<>") {
        return Err(Failure::broken(format!(
            "`cinelog rec` probe printed {stdout:?} (stderr {:?})",
            String::from_utf8_lossy(&out.stderr)
        )));
    }
    Ok(())
}

/// Runs `cinelog index --once` on an empty sandbox: the M2-0 stand-in answers with
/// [`crate::cli::STUB_MARK`] (a skip); anything but success is a broken index engine.
fn probe_index() -> Result<(), Failure> {
    let sb = crate::Sandbox::new().map_err(|e| Failure::broken(format!("sandbox: {e:#}")))?;
    let out = sb
        .cinelog(&["index", "--once"])
        .map_err(|e| Failure::broken(format!("{e:#}")))?;
    if out.is_stub() {
        return Err("the index engine is the M2-0 stand-in (not implemented yet)".into());
    }
    if !out.ok() {
        return Err(Failure::broken(format!("{out}")));
    }
    Ok(())
}

/// The namespace layout of I17 ([`crate::procns`]) can be built: a user namespace with
/// root and a subordinate range mapped, `/proc` with `hidepid=invisible`, a devpts
/// instance, and a nested user namespace entered with `nsenter`.
fn probe_procns() -> Result<(), Failure> {
    for t in ["unshare", "nsenter", "newuidmap", "newgidmap", "python3"] {
        tool(t)?;
    }
    crate::procns::probe().map_err(|e| Failure::from(format!("{e:#}")))
}

/// Checks prerequisites at the top of a scenario; returns from it when one is missing.
#[macro_export]
macro_rules! need {
    ($($n:expr),+ $(,)?) => {
        if let Err(m) = $crate::gate::check(&[$($n),+]) {
            if m.fatal() {
                panic!(
                    "prerequisite {:?} {}: {}",
                    m.need,
                    if m.broken { "is broken" } else { "is missing and required by this run" },
                    m.why
                );
            }
            eprintln!("SKIP {}: {:?}: {}", module_path!(), m.need, m.why);
            return;
        }
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    const LAX: Policy = Policy {
        strict: false,
        root_requested: false,
        root_feature: false,
    };

    #[test]
    fn broken_components_always_fail() {
        assert!(LAX.fatal(Need::Recorder, true));
        assert!(LAX.fatal(Need::Testhooks, true));
        assert!(!LAX.fatal(Need::Recorder, false), "the M1-0 stub is a skip");
    }

    #[test]
    fn strict_runs_fail_on_missing_tools() {
        let strict = Policy { strict: true, ..LAX };
        assert!(strict.fatal(Need::Screen, false));
        assert!(!LAX.fatal(Need::Screen, false));
    }

    #[test]
    fn opt_in_prerequisites_fail_once_requested() {
        for need in [Need::Root, Need::PwUser] {
            assert!(!LAX.fatal(need, false), "{need:?} skips by default");
            let strict_plain = Policy { strict: true, ..LAX };
            assert!(
                !strict_plain.fatal(need, false),
                "{need:?}: REQUIRE without the root feature"
            );
            let strict_root = Policy {
                strict: true,
                root_feature: true,
                ..LAX
            };
            assert!(strict_root.fatal(need, false), "{need:?}: REQUIRE with --features root");
            let requested = Policy {
                root_requested: true,
                ..LAX
            };
            assert!(requested.fatal(need, false), "{need:?}: CINELOG_IT_ROOT=1");
        }
    }
}
