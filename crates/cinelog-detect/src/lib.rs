//! Process-tree detection (plan §5.2, §5.3, §5.6), all behind the [`ProcFs`] trait so it
//! can be tested against fixture trees (`tests/fixtures/proc/**`).
//!
//! Frozen M1 contract: the traits and types below. The detection work package adds, at the
//! crate root:
//!
//! - `pub struct RealProcFs` implementing [`ProcFs`] (`RealProcFs::new()` reads `/proc`,
//!   `RealProcFs::at(root)` reads a fixture directory laid out like `/proc`).
//! - `pub struct ProcDetector<P: ProcFs, R: RegistryLookup>` implementing [`Detector`]
//!   (`ProcDetector::new(proc: P, registry: R)`).
//! - `pub fn tty_of_fd(fd: RawFd) -> TtyIdentity` (pts index from `st_rdev`, majors 136–143).

use std::fmt;
use std::io;
use std::path::PathBuf;
use std::str::FromStr;

use cinelog_core::config::HidepidFallback;
use cinelog_core::registry::RegistryEntry;
use cinelog_proto::{Attribution, Scope};

pub type Pid = u32;

/// Parsed `/proc/<pid>/stat` (fields after the parenthesised comm are numbered as in proc(5)).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcStat {
    pub pid: Pid,
    /// comm as it appears in stat (up to 15 bytes, may contain spaces and parentheses).
    pub comm: String,
    pub state: char,
    pub ppid: Pid,
    pub pgrp: Pid,
    pub session: Pid,
    /// Controlling tty device number (`tty_nr`), 0 if none.
    pub tty_nr: u64,
    /// Foreground pgrp of the controlling tty (`tpgid`), -1 if none.
    pub tpgid: i64,
    /// Field 22, clock ticks since boot.
    pub starttime: u64,
}

/// Real, effective, saved and filesystem uid from `/proc/<pid>/status`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Uids {
    pub real: u32,
    pub effective: u32,
    pub saved: u32,
    pub fs: u32,
}

/// One entry of `/proc/<pid>/fd`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FdEntry {
    pub fd: u32,
    pub target: PathBuf,
}

/// `hidepid=` mount option of `/proc`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hidepid {
    Off,
    /// `hidepid=1` / `noaccess`.
    NoAccess,
    /// `hidepid=2` / `invisible`.
    Invisible,
    /// `hidepid=4` / `ptraceable`.
    Ptraceable,
}

/// Read-only view of `/proc`. Errors are the raw `io::Error`s (EACCES and ENOENT matter).
pub trait ProcFs {
    /// Pid of the calling process (as `/proc/self` resolves).
    fn self_pid(&self) -> Pid;
    fn pids(&self) -> io::Result<Vec<Pid>>;
    fn stat(&self, pid: Pid) -> io::Result<ProcStat>;
    /// `/proc/<pid>/comm` without the trailing newline.
    fn comm(&self, pid: Pid) -> io::Result<String>;
    /// NUL-separated `/proc/<pid>/cmdline`.
    fn cmdline(&self, pid: Pid) -> io::Result<Vec<String>>;
    /// `readlink /proc/<pid>/exe`; may end in ` (deleted)` after an upgrade.
    fn exe(&self, pid: Pid) -> io::Result<PathBuf>;
    fn uids(&self, pid: Pid) -> io::Result<Uids>;
    /// Owner uid of the `/proc/<pid>` directory.
    fn owner_uid(&self, pid: Pid) -> io::Result<u32>;
    /// `/proc/<pid>/loginuid`; `None` when unset (4294967295).
    fn loginuid(&self, pid: Pid) -> io::Result<Option<u32>>;
    fn fds(&self, pid: Pid) -> io::Result<Vec<FdEntry>>;
    /// `tty-index:` line of `/proc/<pid>/fdinfo/<fd>`, `None` if absent.
    fn fdinfo_tty_index(&self, pid: Pid, fd: u32) -> io::Result<Option<u32>>;
    /// `/proc/sys/kernel/random/boot_id`.
    fn boot_id(&self) -> io::Result<String>;
    fn hidepid(&self) -> io::Result<Hidepid>;
}

/// The tty on fd 0 of the process being started.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TtyIdentity {
    /// `/dev/pts/<N>`.
    Pts(u32),
    /// A VT or serial tty, by device path.
    Other(PathBuf),
    NotATty,
}

/// How a holder was found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FoundVia {
    /// Nearest ancestor holding the master (preferred).
    Ancestor,
    /// Full `/proc` scan.
    Scan,
    /// No holder visible; nearest ancestor classified by comm.
    CommFallback,
}

/// Classification of the PTY master holder (§5.2 step 4), by comm and cmdline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HolderClass {
    /// A cinelog recorder with a matching registry entry.
    OwnRecorder { same_uid: bool },
    /// `sudo`, `su`, `doas` holding a master (`use_pty`).
    PrivRelay,
    /// `tmux: server`, `screen`, `zellij`.
    Multiplexer,
    /// `sshd`, `sshd-session`, `mosh-server`.
    Remote,
    /// `script`, terminal emulators and anything else.
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Holder {
    pub pid: Pid,
    pub uid: u32,
    pub comm: String,
    pub class: HolderClass,
    pub via: FoundVia,
}

/// Single-word reason tokens of [`StartDecision`] (kebab-case on the wire).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Reason {
    // start
    TmuxServer,
    Screen,
    Zellij,
    Sshd,
    MoshServer,
    Sudo,
    Su,
    Doas,
    /// Own recorder of another uid holds the pts (e.g. `su` without a PTY).
    NestedUid,
    Script,
    Terminal,
    Vt,
    Hidepid,
    Unknown,
    // skip
    NotATty,
    Disabled,
    HidepidSkip,
    Error,
}

impl Reason {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::TmuxServer => "tmux-server",
            Self::Screen => "screen",
            Self::Zellij => "zellij",
            Self::Sshd => "sshd",
            Self::MoshServer => "mosh-server",
            Self::Sudo => "sudo",
            Self::Su => "su",
            Self::Doas => "doas",
            Self::NestedUid => "nested-uid",
            Self::Script => "script",
            Self::Terminal => "terminal",
            Self::Vt => "vt",
            Self::Hidepid => "hidepid",
            Self::Unknown => "unknown",
            Self::NotATty => "not-a-tty",
            Self::Disabled => "disabled",
            Self::HidepidSkip => "hidepid",
            Self::Error => "error",
        }
    }

    const START: [Self; 14] = [
        Self::TmuxServer,
        Self::Screen,
        Self::Zellij,
        Self::Sshd,
        Self::MoshServer,
        Self::Sudo,
        Self::Su,
        Self::Doas,
        Self::NestedUid,
        Self::Script,
        Self::Terminal,
        Self::Vt,
        Self::Hidepid,
        Self::Unknown,
    ];
    const SKIP: [Self; 4] = [Self::NotATty, Self::Disabled, Self::HidepidSkip, Self::Error];
}

impl fmt::Display for Reason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Result of `cinelog rec --check`; its `Display` is the one-line output the plugin parses:
/// `start <scope> <reason>`, `recorded <uuid>` or `skip <reason>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StartDecision {
    Start { scope: Scope, reason: Reason },
    Recorded { uuid: String },
    Skip { reason: Reason },
}

impl fmt::Display for StartDecision {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Start { scope, reason } => write!(f, "start {scope} {reason}"),
            Self::Recorded { uuid } => write!(f, "recorded {uuid}"),
            Self::Skip { reason } => write!(f, "skip {reason}"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("not a rec --check line: {0:?}")]
pub struct DecisionParseError(pub String);

impl FromStr for StartDecision {
    type Err = DecisionParseError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let err = || DecisionParseError(s.to_owned());
        let find = |set: &[Reason], tok: &str| set.iter().copied().find(|r| r.as_str() == tok);
        let mut it = s.trim_end().split(' ');
        match (it.next(), it.next(), it.next(), it.next()) {
            (Some("start"), Some(scope), Some(reason), None) => Ok(Self::Start {
                scope: scope.parse().map_err(|_| err())?,
                reason: find(&Reason::START, reason).ok_or_else(err)?,
            }),
            (Some("recorded"), Some(uuid), None, None) => Ok(Self::Recorded { uuid: uuid.to_owned() }),
            (Some("skip"), Some(reason), None, None) => Ok(Self::Skip {
                reason: find(&Reason::SKIP, reason).ok_or_else(err)?,
            }),
            _ => Err(err()),
        }
    }
}

/// Input of [`Detector::check_start`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckRequest {
    pub tty: TtyIdentity,
    pub euid: u32,
    /// Pid whose ancestors are walked (normally `getppid()` of `cinelog rec --check`,
    /// i.e. the shell; for `rec` itself its own parent).
    pub start_pid: Pid,
    pub hidepid_fallback: HidepidFallback,
}

/// Foreground state of the recorded PTY (§5.3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Foreground {
    /// The shell's own pgrp, and its leader is not a multiplexer (`exec tmux` check).
    Shell,
    /// Some other job, no multiplexer or recorder found in it.
    Job { pgrp: Pid, leader_comm: String },
    /// A pgrp member or descendant has a comm from `pause_on_multiplexer`.
    Multiplexer { pid: Pid, comm: String, pgrp: Pid },
    /// A descendant in the fg pgrp is a cinelog recorder.
    Recorder { pid: Pid, starttime: u64, pgrp: Pid },
}

/// Claim made by a `nested-start` marker, to be corroborated against the process tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NestedClaim {
    pub pid: Pid,
    pub starttime: u64,
}

/// Session principal (§5.6).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Principal {
    pub scope: Scope,
    /// PERSONAL: the euid's name. TEAM: the attributed user, or `root`.
    pub user: String,
    /// TEAM: the attributed user (directory name under `/root/logs`).
    pub sudo_user: Option<String>,
    /// `$SUDO_USER` as found; never used for paths.
    pub claimed_user: Option<String>,
    pub attribution: Attribution,
    /// comm of the nearest `sshd-session`, `login`, `systemd`, or `unknown`, when attribution is `None`.
    pub login_via: Option<String>,
}

/// Input of [`Detector::principal`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrincipalRequest {
    pub euid: u32,
    /// Pid to start the ancestor walk from (the recorder itself).
    pub start_pid: Pid,
    /// Pid of the tmux client that created the pane (resolved by the recorder from
    /// `#{client_tty}`), walked instead of `start_pid` for shared root tmux servers.
    pub tmux_client_pid: Option<Pid>,
    pub claimed_user: Option<String>,
}

/// Detection operations used by `rec --check`, the recorder and `doctor`.
pub trait Detector {
    /// §5.2 start decision.
    fn check_start(&self, req: &CheckRequest) -> StartDecision;
    /// The PTY master holder of a pts, nearest ancestor of `start_pid` first, then full
    /// scan, then comm fallback.
    fn holder(&self, pts: u32, start_pid: Pid) -> Option<Holder>;
    /// §5.3: classify the foreground pgrp (`fg_pgrp` from `tcgetpgrp(master)`).
    fn foreground(&self, fg_pgrp: Pid, shell_pgrp: Pid, multiplexers: &[String]) -> Foreground;
    /// §5.3: true iff `fg_pgrp ≠ shell_pgrp` and a descendant in `fg_pgrp` has comm
    /// `cinelog` with the claimed pid and starttime.
    fn corroborate_nested(&self, fg_pgrp: Pid, shell_pgrp: Pid, claim: NestedClaim) -> bool;
    /// §5.6 attribution.
    fn principal(&self, req: &PrincipalRequest) -> Principal;
    /// Whether `fg_pgrp` contains a process whose comm is in `relays` (input rule (d)).
    fn relay_in_foreground(&self, fg_pgrp: Pid, relays: &[String]) -> bool;
    /// Starttime of a running pid (registry validation).
    fn starttime(&self, pid: Pid) -> Option<u64>;
    fn boot_id(&self) -> Option<String>;
}

/// Registry lookup used for the own-recorder check, so detection can be tested without a
/// runtime dir.
pub trait RegistryLookup {
    fn entry_for_pts(&self, pts: u32) -> Option<RegistryEntry>;
}

impl RegistryLookup for cinelog_core::registry::Registry {
    fn entry_for_pts(&self, pts: u32) -> Option<RegistryEntry> {
        self.read(&cinelog_core::TtyKey::Pts(pts)).ok().flatten()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decision_lines_roundtrip() {
        let cases = [
            StartDecision::Start {
                scope: Scope::Personal,
                reason: Reason::TmuxServer,
            },
            StartDecision::Start {
                scope: Scope::Team,
                reason: Reason::Sudo,
            },
            StartDecision::Recorded {
                uuid: "01930a3e-0000-7000-8000-00000000abcd".into(),
            },
            StartDecision::Skip {
                reason: Reason::NotATty,
            },
            StartDecision::Skip {
                reason: Reason::HidepidSkip,
            },
        ];
        for d in cases {
            let line = d.to_string();
            assert_eq!(line.parse::<StartDecision>().unwrap(), d, "{line}");
        }
        assert_eq!(
            StartDecision::Start {
                scope: Scope::Team,
                reason: Reason::TmuxServer
            }
            .to_string(),
            "start team tmux-server"
        );
        assert!("start personal".parse::<StartDecision>().is_err());
        assert!("skip tmux-server".parse::<StartDecision>().is_err());
        assert!("recorded a b".parse::<StartDecision>().is_err());
    }
}
