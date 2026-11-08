//! `cinelog rec --check` (§5.2, PROTOCOL §6.3): one line, always exit 0.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use cinelog_core::logging::RecLog;
use cinelog_core::registry::{ProcessProbe, Registry, SystemProbe, TtyKey};
use cinelog_detect::{CheckRequest, Detector, Pid, ProcFs, RealProcFs, Reason, StartDecision, TtyIdentity};
use cinelog_proto::env;

use crate::ctx::Ctx;

/// Everything the decision depends on besides the context, resolved up front so the
/// decision itself is a pure function (and testable with fakes).
#[derive(Debug, Clone, Default)]
pub struct CheckInputs {
    pub tty: Option<TtyIdentity>,
    /// Path of fd 0's tty (`$TTY` of the shell).
    pub tty_path: Option<PathBuf>,
    /// The shell (`getppid()` of `rec --check`).
    pub shell_pid: Pid,
    /// The shell's `$PPID`.
    pub shell_ppid: Option<Pid>,
    /// `CINELOG_REC_PID` / `CINELOG_TTY` as inherited (a cache only).
    pub env_rec_pid: Option<String>,
    pub env_tty: Option<String>,
}

impl CheckInputs {
    pub fn capture() -> Self {
        let shell_pid = nix::unistd::getppid().as_raw().unsigned_abs();
        Self {
            tty: Some(cinelog_detect::tty_of_fd(0)),
            tty_path: nix::unistd::ttyname(std::io::stdin()).ok(),
            shell_pid,
            shell_ppid: RealProcFs::new().stat(shell_pid).ok().map(|s| s.ppid),
            env_rec_pid: std::env::var(env::REC_PID).ok(),
            env_tty: std::env::var(env::TTY).ok(),
        }
    }
}

/// The decision, plus a detail line for `rec.log` when it is `skip error`.
///
/// A broken config only matters for a recorder that would start now: a shell in a tty that
/// is already recorded still gets `recorded <uuid>` and attaches to its recorder.
pub fn decide(
    ctx: &Ctx,
    detector: &dyn Detector,
    probe: &dyn ProcessProbe,
    inputs: &CheckInputs,
) -> (StartDecision, Option<String>) {
    let skip = |reason| StartDecision::Skip { reason };
    let tty = match &inputs.tty {
        None | Some(TtyIdentity::NotATty) => return (skip(Reason::NotATty), None),
        Some(t) => t.clone(),
    };
    if ctx.is_disabled() {
        return (skip(Reason::Disabled), None);
    }
    if ctx.env.euid != 0
        && let Some(uuid) = env_cache_hit(&ctx.registry(), probe, inputs)
    {
        return (StartDecision::Recorded { uuid }, None);
    }
    let decision = detector.check_start(&CheckRequest {
        tty,
        euid: ctx.env.euid,
        start_pid: inputs.shell_pid,
        hidepid_fallback: ctx.config.record.hidepid_fallback,
    });
    match (decision, &ctx.config_error) {
        (StartDecision::Start { .. }, Some(e)) => (skip(Reason::Error), Some(format!("rec --check: {e}"))),
        (decision, _) => {
            let detail = matches!(decision, StartDecision::Skip { reason: Reason::Error })
                .then(|| "rec --check: /proc is unusable, holder detection failed".to_owned());
            (decision, detail)
        }
    }
}

/// `CINELOG_REC_PID`/`CINELOG_TTY` count only if the pid is the shell's parent, the tty is
/// fd 0's tty and the registry entry for that tty belongs to that live recorder.
fn env_cache_hit(registry: &Registry, probe: &dyn ProcessProbe, inputs: &CheckInputs) -> Option<String> {
    let rec_pid: Pid = inputs.env_rec_pid.as_deref()?.parse().ok()?;
    let tty = inputs.tty_path.as_deref()?;
    if Some(rec_pid) != inputs.shell_ppid || inputs.env_tty.as_deref().map(Path::new) != Some(tty) {
        return None;
    }
    let entry = registry.read(&TtyKey::from_path(tty)?).ok()??;
    (entry.pid == rec_pid && entry.is_alive(probe)).then_some(entry.uuid)
}

pub fn run() -> ExitCode {
    let ctx = Ctx::load();
    let (decision, detail) = decide(&ctx, &ctx.detector(), &SystemProbe, &CheckInputs::capture());
    if let Some(detail) = detail {
        let _ = RecLog::new(&ctx.paths.rec_log).append(&detail);
    }
    println!("{decision}");
    ExitCode::SUCCESS
}

#[cfg(test)]
pub(crate) mod tests {
    use std::cell::Cell;

    use cinelog_core::paths::{EnvSnapshot, SystemRoot};
    use cinelog_core::registry::{REGISTRY_FORMAT, RecState, RegistryEntry};
    use cinelog_core::{Nonce, Scope};
    use cinelog_detect::{Foreground, Holder, NestedClaim, Principal, PrincipalRequest};

    use super::*;

    /// A detector that answers `check_start` with a fixed decision and records the request.
    pub(crate) struct FakeDetector {
        pub decision: StartDecision,
        pub holder: Option<Holder>,
        pub calls: Cell<u32>,
    }

    impl FakeDetector {
        pub fn new(decision: StartDecision) -> Self {
            Self {
                decision,
                holder: None,
                calls: Cell::new(0),
            }
        }
    }

    impl Detector for FakeDetector {
        fn check_start(&self, req: &CheckRequest) -> StartDecision {
            assert_eq!(req.start_pid, 4242);
            self.calls.set(self.calls.get() + 1);
            self.decision.clone()
        }
        fn holder(&self, _pts: u32, _start_pid: Pid) -> Option<Holder> {
            self.holder.clone()
        }
        fn foreground(&self, _: Pid, _: Pid, _: &[String]) -> Foreground {
            Foreground::Shell
        }
        fn corroborate_nested(&self, _: Pid, _: Pid, _: NestedClaim) -> bool {
            false
        }
        fn principal(&self, _: &PrincipalRequest) -> Principal {
            unreachable!("not used by the CLI")
        }
        fn relay_in_foreground(&self, _: Pid, _: &[String]) -> bool {
            false
        }
        fn starttime(&self, _: Pid) -> Option<u64> {
            None
        }
        fn boot_id(&self) -> Option<String> {
            None
        }
    }

    pub(crate) struct FakeProbe;
    impl ProcessProbe for FakeProbe {
        fn starttime(&self, pid: u32) -> Option<u64> {
            (pid == 777).then_some(5)
        }
        fn boot_id(&self) -> Option<String> {
            Some("boot".into())
        }
    }

    pub(crate) fn test_env(dir: &Path) -> EnvSnapshot {
        EnvSnapshot {
            uid: 1000,
            euid: 1000,
            user: "alice".into(),
            home: dir.join("home"),
            xdg_config_home: Some(dir.join("config")),
            xdg_state_home: Some(dir.join("state")),
            xdg_runtime_dir: Some(dir.join("run")),
        }
    }

    pub(crate) fn entry(pid: u32, uuid: &str) -> RegistryEntry {
        RegistryEntry {
            format: REGISTRY_FORMAT,
            pid,
            starttime: 5,
            boot_id: "boot".into(),
            uuid: uuid.into(),
            nonce: Nonce::from_bytes([7; 16]),
            scope: Scope::Personal,
            cast: "/l/x.cast".into(),
            pts: "/dev/pts/9".into(),
            host_tty: "/dev/pts/1".into(),
            shell_pid: 4242,
            parent_uuid: None,
            state: RecState::Live,
            state_reason: None,
            part: 1,
            version: cinelog_core::VERSION.into(),
            opts: String::new(),
        }
    }

    fn inputs() -> CheckInputs {
        CheckInputs {
            tty: Some(TtyIdentity::Pts(9)),
            tty_path: Some("/dev/pts/9".into()),
            shell_pid: 4242,
            shell_ppid: Some(777),
            env_rec_pid: None,
            env_tty: None,
        }
    }

    fn start() -> StartDecision {
        StartDecision::Start {
            scope: Scope::Personal,
            reason: Reason::TmuxServer,
        }
    }

    #[test]
    fn delegates_to_the_detector() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = Ctx::with(test_env(dir.path()), SystemRoot::real());
        let det = FakeDetector::new(start());
        assert_eq!(decide(&ctx, &det, &FakeProbe, &inputs()), (start(), None));
        assert_eq!(det.calls.get(), 1);
    }

    #[test]
    fn not_a_tty_and_kill_switch_win_before_detection() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = Ctx::with(test_env(dir.path()), SystemRoot::real());
        let det = FakeDetector::new(start());
        let mut i = inputs();
        i.tty = Some(TtyIdentity::NotATty);
        assert_eq!(decide(&ctx, &det, &FakeProbe, &i).0.to_string(), "skip not-a-tty");

        std::fs::create_dir_all(dir.path().join("config/cinelog")).unwrap();
        std::fs::write(dir.path().join("config/cinelog/disable"), b"").unwrap();
        let ctx = Ctx::with(test_env(dir.path()), SystemRoot::real());
        assert_eq!(decide(&ctx, &det, &FakeProbe, &inputs()).0.to_string(), "skip disabled");
        assert_eq!(det.calls.get(), 0);
    }

    #[test]
    fn broken_config_is_an_error_with_detail() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("config/cinelog")).unwrap();
        std::fs::write(dir.path().join("config/cinelog/config.toml"), "[record\n").unwrap();
        let ctx = Ctx::with(test_env(dir.path()), SystemRoot::real());
        let (d, detail) = decide(&ctx, &FakeDetector::new(start()), &FakeProbe, &inputs());
        assert_eq!(d.to_string(), "skip error");
        assert!(detail.unwrap().contains("config.toml"));
    }

    /// A config typo made during a recorded session must not detach new subshells.
    #[test]
    fn broken_config_does_not_hide_a_running_recorder() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("config/cinelog")).unwrap();
        std::fs::write(dir.path().join("config/cinelog/config.toml"), "[record\n").unwrap();
        let ctx = Ctx::with(test_env(dir.path()), SystemRoot::real());
        assert!(ctx.config_error.is_some());
        let recorded = StartDecision::Recorded { uuid: "u-1".into() };
        let det = FakeDetector::new(recorded.clone());
        assert_eq!(decide(&ctx, &det, &FakeProbe, &inputs()), (recorded.clone(), None));

        let reg = ctx.registry();
        reg.ensure_dir().unwrap();
        reg.write(&TtyKey::Pts(9), &entry(777, "u-1")).unwrap();
        let mut i = inputs();
        i.env_rec_pid = Some("777".into());
        i.env_tty = Some("/dev/pts/9".into());
        let det = FakeDetector::new(start());
        assert_eq!(decide(&ctx, &det, &FakeProbe, &i), (recorded, None));
        assert_eq!(det.calls.get(), 0, "answered from the env cache");

        let skip = StartDecision::Skip {
            reason: Reason::HidepidSkip,
        };
        let det = FakeDetector::new(skip.clone());
        assert_eq!(decide(&ctx, &det, &FakeProbe, &inputs()).0, skip);
    }

    #[test]
    fn env_cache_needs_parent_tty_and_registry() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = Ctx::with(test_env(dir.path()), SystemRoot::real());
        let reg = ctx.registry();
        reg.ensure_dir().unwrap();
        reg.write(&TtyKey::Pts(9), &entry(777, "u-1")).unwrap();
        let det = FakeDetector::new(start());
        let mut i = inputs();
        i.env_rec_pid = Some("777".into());
        i.env_tty = Some("/dev/pts/9".into());
        let recorded = StartDecision::Recorded { uuid: "u-1".into() };
        assert_eq!(decide(&ctx, &det, &FakeProbe, &i).0, recorded);
        assert_eq!(det.calls.get(), 0);

        // Wrong parent, wrong tty, or a dead recorder: the cache is ignored.
        for tweak in [
            |i: &mut CheckInputs| i.shell_ppid = Some(1),
            |i: &mut CheckInputs| i.env_tty = Some("/dev/pts/8".into()),
            |i: &mut CheckInputs| i.env_rec_pid = Some("778".into()),
        ] {
            let mut j = i.clone();
            tweak(&mut j);
            assert_eq!(decide(&ctx, &det, &FakeProbe, &j).0, start());
        }
        reg.write(&TtyKey::Pts(9), &entry(778, "u-1")).unwrap();
        assert_eq!(decide(&ctx, &det, &FakeProbe, &i).0, start());
    }
}
