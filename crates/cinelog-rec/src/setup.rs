//! Preflight of `cinelog rec` (plan §5.1, §5.2, §5.6): everything that can fail happens
//! here, before the fork, so a failure can still exec the shell unrecorded.

use std::ffi::OsString;
use std::os::fd::{AsFd, AsRawFd};
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use cinelog_core::clock::{self, Clock};
use cinelog_core::config::{Autostart, Config, PathsConfig};
use cinelog_core::ids::{self, SessionId};
use cinelog_core::logging::RecLog;
use cinelog_core::paths::{self, EnvSnapshot, Paths, SystemRoot};
use cinelog_core::registry::{REGISTRY_FORMAT, RecState, Registry, RegistryEntry, TtyKey};
use cinelog_detect::{
    CheckRequest, Detector, Holder, HolderClass, PrincipalRequest, ProcDetector, RealProcFs, StartDecision,
    TtyIdentity, tty_of_fd,
};
use cinelog_proto::{Extra, PROTOCOL_VERSION, Scope, SessionInfo, env};

use crate::RecRequest;
use crate::pty::{self, ChildSpec};
use crate::recorder::{HostIo, Launch, ProcessEffects};
use crate::tmux::{TmuxClient, TmuxEnv};
use crate::writer::{FileFactory, HeaderInfo};

/// Why recording does not start; the shell is exec'd unrecorded with this reason.
#[derive(Debug)]
pub(crate) struct Fallback {
    pub reason: String,
    pub log: Option<RecLog>,
}

fn system_root() -> SystemRoot {
    #[cfg(feature = "testhooks")]
    if let Some(p) = std::env::var_os("CINELOG_TESTHOOK_SYSROOT") {
        return SystemRoot::prefixed(p);
    }
    SystemRoot::real()
}

/// Kill switch (§5.6, §9.1): TEAM only `/etc/cinelog/disable`, PERSONAL also `$CINELOG_DISABLE`.
fn disabled(scope: Scope, disable_file: &Path) -> bool {
    let env_switch = scope == Scope::Personal && std::env::var_os(env::DISABLE).is_some_and(|v| !v.is_empty());
    env_switch || disable_file.exists()
}

/// Largest file read from another user's registry or `/proc/<pid>/environ`.
const FOREIGN_READ_MAX: u64 = 1 << 20;

/// Reads at most [`FOREIGN_READ_MAX`] bytes of a regular file without following a final
/// symlink or blocking on a FIFO (the path may be under another user's control).
fn read_foreign(path: &Path, owner: Option<u32>) -> Option<Vec<u8>> {
    use std::io::Read;
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
    let f = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_NOCTTY)
        .open(path)
        .ok()?;
    let meta = f.metadata().ok()?;
    if !meta.is_file() || owner.is_some_and(|uid| meta.uid() != uid) {
        return None;
    }
    let mut buf = Vec::new();
    f.take(FOREIGN_READ_MAX).read_to_end(&mut buf).ok()?;
    Some(buf)
}

/// Registry directory of a PERSONAL recorder of `uid`, by the location rule of
/// [`Paths::personal`] applied to that recorder's own environment. Without it (not
/// readable) the systemd default `/run/user/<uid>` is assumed.
fn personal_registry_dir(environ: Option<&[u8]>, uid: u32, pw_home: PathBuf) -> PathBuf {
    use std::os::unix::ffi::OsStrExt;
    let var = |k: &str| {
        environ?
            .split(|&b| b == 0)
            .find_map(|kv| kv.strip_prefix(k.as_bytes())?.strip_prefix(b"="))
            .map(|v| PathBuf::from(std::ffi::OsStr::from_bytes(v)))
            .filter(|p| p.is_absolute())
    };
    let snap = EnvSnapshot {
        uid,
        euid: uid,
        user: String::new(),
        home: var("HOME").unwrap_or(pw_home),
        xdg_config_home: var("XDG_CONFIG_HOME"),
        xdg_state_home: var("XDG_STATE_HOME"),
        xdg_runtime_dir: match environ {
            Some(_) => var("XDG_RUNTIME_DIR"),
            None => Some(PathBuf::from(format!("/run/user/{uid}"))),
        },
    };
    Paths::personal(&snap, &PathsConfig::default(), "").runtime_dir
}

/// Registry directory of the outer recorder `h`, which runs under another uid.
fn foreign_registry_dir(h: &Holder, root: &SystemRoot) -> PathBuf {
    if h.uid == 0 {
        return Paths::team(root, &PathsConfig::default(), Path::new("/root")).runtime_dir;
    }
    let environ = read_foreign(Path::new(&format!("/proc/{}/environ", h.pid)), None);
    let pw_home = nix::unistd::User::from_uid(nix::unistd::Uid::from_raw(h.uid))
        .ok()
        .flatten()
        .map_or_else(|| PathBuf::from("/"), |u| u.dir);
    personal_registry_dir(environ.as_deref(), h.uid, pw_home)
}

/// Session of the outer recorder when it runs under another uid and we may read its
/// registry (root inside a user's recording, `su` without a PTY). The entry must be
/// owned by that uid and pass the registry validation (§5.1): pid, starttime, boot_id.
fn parent_uuid(
    holder: Option<&Holder>,
    tty: &TtyIdentity,
    registry_dir: impl FnOnce(&Holder) -> PathBuf,
    detector: &dyn Detector,
) -> Option<String> {
    let h = holder?;
    let (HolderClass::OwnRecorder { same_uid: false }, TtyIdentity::Pts(n)) = (&h.class, tty) else {
        return None;
    };
    let path = Registry::new(registry_dir(h)).path_for(&TtyKey::Pts(*n));
    let entry: RegistryEntry = serde_json::from_slice(&read_foreign(&path, Some(h.uid))?).ok()?;
    let valid = entry.pid == h.pid
        && detector.starttime(h.pid) == Some(entry.starttime)
        && detector.boot_id().as_deref() == Some(entry.boot_id.as_str());
    valid.then_some(entry.uuid)
}

/// Free space a log dir needs before a recording starts (PROTOCOL §6.4): a full disk falls
/// back to an unrecorded shell instead of degrading on the first write.
const MIN_FREE_BYTES: u64 = 64 * 1024;

fn check_free_space(dir: &Path, euid: u32) -> Result<(), String> {
    let st = nix::sys::statvfs::statvfs(dir).map_err(|e| format!("statvfs: {e}"))?;
    let blocks = if euid == 0 {
        st.blocks_free()
    } else {
        st.blocks_available()
    };
    let free = blocks.saturating_mul(st.fragment_size());
    let inodes = if euid == 0 {
        st.files_free()
    } else {
        st.files_available()
    };
    if free < MIN_FREE_BYTES {
        return Err(format!("no space left ({free} bytes free, need {MIN_FREE_BYTES})"));
    }
    // Filesystems without inode accounting report 0 total files.
    if st.files() > 0 && inodes == 0 {
        return Err("no space left (no free inodes)".into());
    }
    Ok(())
}

pub(crate) fn prepare(req: &RecRequest) -> Result<(Launch, PathBuf), Fallback> {
    let env_snap = EnvSnapshot::capture();
    let scope = env_snap.scope();
    let root = system_root();
    let host = paths::hostname();
    let defaults = Paths::resolve(&env_snap, &root, &PathsConfig::default(), &host);
    let fail = |reason: String, log: &RecLog| Fallback {
        reason,
        log: Some(log.clone()),
    };
    let log = RecLog::new(defaults.rec_log.clone());
    if disabled(scope, &defaults.disable_file) {
        return Err(Fallback {
            reason: "disabled".into(),
            log: None,
        });
    }
    let cfg = Config::load(&Paths::config_file_for(scope, &env_snap, &root))
        .map_err(|e| fail(format!("config: {e}"), &log))?;
    let paths = Paths::resolve(&env_snap, &root, &cfg.paths, &host);
    let log = RecLog::new(paths.rec_log.clone());
    if !paths.logs_root.is_absolute() {
        return Err(fail(
            format!("paths.logs must be absolute: {}", paths.logs_root.display()),
            &log,
        ));
    }

    let stdin = std::io::stdin();
    let is_tty = pty::is_tty(0);
    let tty = tty_of_fd(0);
    let own_pid = std::process::id();
    let ppid = nix::unistd::getppid().as_raw() as u32;
    let registry = Registry::new(paths.runtime_dir.clone());
    let detector: Arc<dyn Detector + Send + Sync> = Arc::new(ProcDetector::new(RealProcFs::new(), registry.clone()));

    // §5.2 is also run here: the plugin's `rec --check` may be stale by now. Headless use
    // (benchmarks, `cinelog rec -- cmd < file`) records without a decision.
    if is_tty {
        let decision = detector.check_start(&CheckRequest {
            tty: tty.clone(),
            euid: env_snap.euid,
            start_pid: ppid,
            hidepid_fallback: cfg.record.hidepid_fallback,
        });
        match decision {
            StartDecision::Start { .. } => {}
            StartDecision::Recorded { .. } => return Err(fail("already-recorded".into(), &log)),
            StartDecision::Skip { reason } => return Err(fail(reason.to_string(), &log)),
        }
    }
    let holder = match tty {
        TtyIdentity::Pts(n) => detector.holder(n, ppid),
        _ => None,
    };
    let host_tty = nix::unistd::ttyname(stdin.as_fd())
        .map(|p| p.display().to_string())
        .unwrap_or_default();

    let tmux_env = holder
        .as_ref()
        .filter(|h| h.class == HolderClass::Multiplexer && h.comm.starts_with("tmux"))
        .and_then(|h| TmuxEnv::from_env().filter(|e| e.server_pid == h.pid));
    let tmux = tmux_env.clone().map(|e| TmuxClient::new(e, env_snap.euid));
    let tmux_client_pid = tmux
        .as_ref()
        .filter(|_| env_snap.euid == 0)
        .and_then(TmuxClient::client_pid);
    let principal = detector.principal(&PrincipalRequest {
        euid: env_snap.euid,
        start_pid: own_pid,
        tmux_client_pid,
        claimed_user: std::env::var("SUDO_USER").ok(),
    });

    let id = SessionId::new_v7();
    let nonce = ids::new_nonce().map_err(|e| fail(format!("nonce: {e}"), &log))?;
    let start = jiff::Zoned::now();
    let cast_dir = paths.cast_dir(principal.sudo_user.as_deref(), &start);
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&cast_dir)
        .map_err(|e| fail(format!("logs: {}: {e}", cast_dir.display()), &log))?;
    check_free_space(&cast_dir, env_snap.euid).map_err(|e| fail(format!("logs: {}: {e}", cast_dir.display()), &log))?;
    registry
        .ensure_dir()
        .map_err(|e| fail(format!("registry: {e}"), &log))?;

    let host_termios = is_tty
        .then(|| nix::sys::termios::tcgetattr(stdin.as_fd()).ok())
        .flatten();
    let winsize = pty::winsize(0)
        .or_else(|| pty::winsize(1))
        .unwrap_or(pty::DEFAULT_WINSIZE);
    let pair = nix::pty::openpty(Some(&winsize), host_termios.as_ref()).map_err(|e| fail(format!("pty: {e}"), &log))?;
    for fd in [pair.master.as_raw_fd(), pair.slave.as_raw_fd()] {
        pty::set_cloexec(fd).map_err(|e| fail(format!("pty: {e}"), &log))?;
    }
    let slave_path = nix::unistd::ttyname(pair.slave.as_fd()).map_err(|e| fail(format!("pty name: {e}"), &log))?;
    let key =
        TtyKey::from_path(&slave_path).ok_or_else(|| fail(format!("pty name: {}", slave_path.display()), &log))?;
    let nonce_read = pty::nonce_pipe(format!("{}\n", nonce.to_hex()).as_bytes())
        .map_err(|e| fail(format!("nonce pipe: {e}"), &log))?;

    let slave_str = slave_path.display().to_string();
    let opts = cfg.shell.to_opts(None).to_env_value();
    let extra = [
        (env::NONCE_FD, nonce_read.as_raw_fd().to_string()),
        (env::REC_PID, own_pid.to_string()),
        (env::TTY, slave_str.clone()),
        (env::SESSION, id.to_string()),
        (env::OPTS, opts.clone()),
    ];
    let mut args: Vec<OsString> = req.args.clone();
    if cfg.record.inner_login && !args.iter().any(|a| a == "-l" || a == "--login") {
        args.insert(0, "-l".into());
    }
    let child = ChildSpec::new(
        req.shell.as_os_str(),
        &args,
        pty::child_env(std::env::vars_os(), &extra),
    )
    .map_err(|e| fail(format!("shell: {e}"), &log))?;

    let rec_starttime = detector.starttime(own_pid).unwrap_or(0);
    let boot_id = detector.boot_id().unwrap_or_default();
    let tmux_identity = tmux_env
        .as_ref()
        .map(|e| e.identity(detector.starttime(e.server_pid).unwrap_or(0)));
    let parent = parent_uuid(
        holder.as_ref(),
        &tty,
        |h| foreign_registry_dir(h, &root),
        detector.as_ref(),
    );
    let session = SessionInfo {
        uuid: id.to_string(),
        part: 1,
        continues: None,
        start_epoch_us: 0,
        boottime_us: 0,
        tz: String::new(),
        tz_offset_s: 0,
        scope,
        host: host.clone(),
        user: principal.user.clone(),
        uid: env_snap.uid,
        euid: env_snap.euid,
        sudo_user: principal.sudo_user.clone(),
        claimed_user: principal.claimed_user.clone(),
        attribution: principal.attribution,
        login_via: principal.login_via.clone(),
        tty: host_tty.clone(),
        holder_comm: holder.as_ref().map(|h| h.comm.clone()),
        holder_pid: holder.as_ref().map(|h| h.pid),
        rec_pid: own_pid,
        rec_starttime,
        boot_id: boot_id.clone(),
        shell_pid: 0,
        tmux: tmux_identity,
        parent_uuid: parent.clone(),
        input: cfg.record.input,
        rec_version: cinelog_core::VERSION.into(),
        protocol: PROTOCOL_VERSION,
        extra: Extra::new(),
    };
    let header = HeaderInfo {
        cols: winsize.ws_col,
        rows: winsize.ws_row,
        term: std::env::var("TERM").ok().filter(|t| !t.is_empty()),
        shell: Some(req.shell.display().to_string()),
    };

    // The cast file is created last, so that every failure above leaves nothing behind.
    let name = paths::cast_file_name(&host, &start, &id, 1);
    let cast_path = cast_dir.join(&name);
    let first =
        cinelog_cast::create(&cast_path).map_err(|e| fail(format!("cast: {}: {e}", cast_path.display()), &log))?;
    let entry = RegistryEntry {
        format: REGISTRY_FORMAT,
        pid: own_pid,
        starttime: rec_starttime,
        boot_id,
        uuid: id.to_string(),
        nonce,
        scope,
        cast: cast_path.clone(),
        pts: slave_str,
        host_tty,
        shell_pid: 0,
        parent_uuid: parent,
        state: RecState::Live,
        state_reason: None,
        part: 1,
        version: cinelog_core::VERSION.into(),
        opts,
    };
    let factory = FileFactory::new(
        cast_dir,
        move |part| paths::cast_file_name(&host, &start, &id, part),
        (Box::new(first), cast_path.clone()),
    );
    let clock: Arc<dyn Clock> = Arc::from(clock::system_clock());

    let autostart =
        (scope == Scope::Personal && cfg.server.autostart != Autostart::Off).then(crate::autostart::self_exe);
    let launch = Launch {
        child,
        master: pair.master,
        slave: pair.slave,
        nonce_read,
        nonce,
        winsize,
        host_termios,
        host: HostIo {
            input: 0,
            output: 1,
            is_tty,
            announce: matches!(tty, TtyIdentity::Pts(_)),
        },
        effects: ProcessEffects {
            mirror_cmdline: true,
            mirror_cwd: true,
            ignore_job_signals: true,
        },
        clock,
        detector,
        record: cfg.record,
        session,
        header,
        factory: Box::new(factory),
        registry: Some((registry, key, entry)),
        log: Some(log),
        tmux,
        autostart,
    };
    Ok((launch, cast_path))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_full_log_dir_is_refused_before_the_fork() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(check_free_space(dir.path(), 1000), Ok(()));
        // procfs reports no blocks at all, like a full filesystem.
        let err = check_free_space(Path::new("/proc"), 1000).unwrap_err();
        assert!(err.contains("no space left"), "{err}");
        assert!(check_free_space(&dir.path().join("missing"), 1000).is_err());
    }

    #[test]
    fn kill_switch_rules() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("disable");
        assert!(!disabled(Scope::Team, &f));
        std::fs::write(&f, b"").unwrap();
        assert!(disabled(Scope::Team, &f));
        assert!(disabled(Scope::Personal, &f));
    }

    fn holder(class: HolderClass) -> Holder {
        Holder {
            pid: std::process::id(),
            uid: nix::unistd::geteuid().as_raw(),
            comm: "cinelog".into(),
            class,
            via: cinelog_detect::FoundVia::Ancestor,
        }
    }

    fn real_detector() -> ProcDetector<RealProcFs, cinelog_detect::NoRegistry> {
        ProcDetector::new(RealProcFs::new(), cinelog_detect::NoRegistry)
    }

    /// A registry entry for this test process on pts 7 in `dir`, edited by `edit`.
    fn write_entry(dir: &Path, edit: impl FnOnce(&mut RegistryEntry)) {
        let d = real_detector();
        let mut entry = RegistryEntry {
            format: REGISTRY_FORMAT,
            pid: std::process::id(),
            starttime: d.starttime(std::process::id()).unwrap(),
            boot_id: d.boot_id().unwrap(),
            uuid: "outer-uuid".into(),
            nonce: cinelog_proto::Nonce::from_bytes([1; 16]),
            scope: Scope::Personal,
            cast: "/x.cast".into(),
            pts: "/dev/pts/7".into(),
            host_tty: String::new(),
            shell_pid: 1,
            parent_uuid: None,
            state: RecState::Live,
            state_reason: None,
            part: 1,
            version: String::new(),
            opts: String::new(),
        };
        edit(&mut entry);
        let reg = Registry::new(dir);
        reg.ensure_dir().unwrap();
        reg.write(&TtyKey::Pts(7), &entry).unwrap();
    }

    #[test]
    fn parent_uuid_needs_a_foreign_own_recorder() {
        let dir = tempfile::tempdir().unwrap();
        write_entry(dir.path(), |_| {});
        let d = real_detector();
        let at = |_: &Holder| dir.path().to_path_buf();
        let foreign = holder(HolderClass::OwnRecorder { same_uid: false });
        assert_eq!(
            parent_uuid(Some(&foreign), &TtyIdentity::Pts(7), at, &d).as_deref(),
            Some("outer-uuid")
        );
        let same = holder(HolderClass::OwnRecorder { same_uid: true });
        assert_eq!(parent_uuid(Some(&same), &TtyIdentity::Pts(7), at, &d), None);
        assert_eq!(parent_uuid(None, &TtyIdentity::Pts(7), at, &d), None);
        assert_eq!(parent_uuid(Some(&foreign), &TtyIdentity::Pts(8), at, &d), None);
        let other_owner = Holder {
            uid: foreign.uid.wrapping_add(1),
            ..foreign.clone()
        };
        assert_eq!(
            parent_uuid(Some(&other_owner), &TtyIdentity::Pts(7), at, &d),
            None,
            "the entry must be owned by the outer recorder's uid"
        );
    }

    #[test]
    fn parent_uuid_validates_the_entry() {
        let foreign = holder(HolderClass::OwnRecorder { same_uid: false });
        let d = real_detector();
        for edit in [
            (|e: &mut RegistryEntry| e.starttime += 1) as fn(&mut RegistryEntry),
            |e| e.boot_id = "00000000-0000-0000-0000-000000000000".into(),
            |e| e.pid += 1,
        ] {
            let dir = tempfile::tempdir().unwrap();
            write_entry(dir.path(), edit);
            let at = |_: &Holder| dir.path().to_path_buf();
            assert_eq!(parent_uuid(Some(&foreign), &TtyIdentity::Pts(7), at, &d), None);
        }
        // A symlinked entry (it lives in another user's directory) is not followed.
        let dir = tempfile::tempdir().unwrap();
        let real = tempfile::tempdir().unwrap();
        write_entry(real.path(), |_| {});
        std::os::unix::fs::symlink(real.path().join("pts-7.json"), dir.path().join("pts-7.json")).unwrap();
        let at = |_: &Holder| dir.path().to_path_buf();
        assert_eq!(parent_uuid(Some(&foreign), &TtyIdentity::Pts(7), at, &d), None);
    }

    #[test]
    fn outer_registry_dir_follows_the_location_rule() {
        let home = PathBuf::from("/home/alice");
        let env = b"HOME=/h\0XDG_RUNTIME_DIR=/run/user/1000\0X=1\0";
        assert_eq!(
            personal_registry_dir(Some(env), 1000, home.clone()),
            Path::new("/run/user/1000/cinelog")
        );
        let env = b"HOME=/h\0XDG_STATE_HOME=/st\0";
        assert_eq!(
            personal_registry_dir(Some(env), 1000, home.clone()),
            Path::new("/st/cinelog/run"),
            "no XDG_RUNTIME_DIR: the state dir"
        );
        assert_eq!(
            personal_registry_dir(Some(b"HOME=/h\0"), 1000, home.clone()),
            Path::new("/h/.local/state/cinelog/run")
        );
        assert_eq!(
            personal_registry_dir(Some(b"XDG_RUNTIME_DIR=relative\0"), 1000, home.clone()),
            Path::new("/home/alice/.local/state/cinelog/run")
        );
        assert_eq!(
            personal_registry_dir(None, 1000, home),
            Path::new("/run/user/1000/cinelog"),
            "environ unreadable: the systemd default"
        );
        // Our own environ is readable: the rule applied to it.
        let me = holder(HolderClass::OwnRecorder { same_uid: false });
        let expected = Paths::personal(&EnvSnapshot::capture(), &PathsConfig::default(), "").runtime_dir;
        if me.uid != 0 {
            assert_eq!(foreign_registry_dir(&me, &SystemRoot::real()), expected);
        }
    }

    // --- "never fails the shell" (§5.1): `run()` in a re-executed copy of this test binary.

    /// Selects the scenario in the re-executed child (it is a `CINELOG_*` variable, so the
    /// fallback must remove it).
    const CHILD_MODE: &str = "CINELOG_REC_TEST_CHILD";
    // The first echo ends libtest's unterminated "test … " line.
    const PROBE: &str = r#"echo
echo "FAILED=${CINELOG_FAILED-unset}"
echo "FTTY=${CINELOG_FAILED_TTY-unset}"
echo "FPID=${CINELOG_FAILED_PID-unset}"
echo "PID=$$"
echo "SESSION=${CINELOG_SESSION-unset}"
echo "MODE=${CINELOG_REC_TEST_CHILD-unset}"
"#;

    /// Child side: does nothing in a normal test run.
    #[test]
    fn fallback_child() {
        let Some(mode) = std::env::var_os(CHILD_MODE) else {
            return;
        };
        if mode == "nproc" {
            // No more processes for this uid: the fork in `record()` fails.
            let none = libc::rlimit {
                rlim_cur: 0,
                rlim_max: 0,
            };
            // SAFETY: plain setrlimit on a valid struct.
            assert_eq!(unsafe { libc::setrlimit(libc::RLIMIT_NPROC, &none) }, 0);
        }
        crate::run(RecRequest {
            shell: "/bin/sh".into(),
            args: vec!["-c".into(), PROBE.into()],
        });
    }

    struct Child {
        home: tempfile::TempDir,
        vars: std::collections::HashMap<String, String>,
        pid: u32,
    }

    impl Child {
        fn rec_log(&self) -> String {
            std::fs::read_to_string(self.home.path().join("state/cinelog/rec.log")).unwrap_or_default()
        }

        fn casts(&self) -> Vec<PathBuf> {
            fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
                for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
                    let p = e.path();
                    if p.is_dir() {
                        walk(&p, out);
                    } else if p.extension().is_some_and(|x| x == "cast") {
                        out.push(p);
                    }
                }
            }
            let mut out = Vec::new();
            walk(self.home.path(), &mut out);
            out
        }
    }

    /// Runs [`fallback_child`] in `mode` with a temp HOME/XDG tree prepared by `prep`.
    fn run_child(mode: &str, stdin: std::process::Stdio, prep: impl FnOnce(&Path)) -> Child {
        let home = tempfile::tempdir().unwrap();
        let h = home.path();
        for d in ["config/cinelog", "state", "run"] {
            std::fs::create_dir_all(h.join(d)).unwrap();
        }
        prep(h);
        let out = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "setup::tests::fallback_child",
                "--nocapture",
                "--test-threads=1",
            ])
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", h)
            .env("XDG_CONFIG_HOME", h.join("config"))
            .env("XDG_STATE_HOME", h.join("state"))
            .env("XDG_RUNTIME_DIR", h.join("run"))
            .env("CINELOG_SESSION", "stale-session")
            .env(CHILD_MODE, mode)
            .stdin(stdin)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let pid = out.id();
        let out = out.wait_with_output().unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
        assert!(out.status.success(), "{stdout}{}", String::from_utf8_lossy(&out.stderr));
        let vars = stdout
            .lines()
            .filter_map(|l| l.split_once('='))
            .map(|(k, v)| (k.to_owned(), v.to_owned()))
            .collect();
        Child { home, vars, pid }
    }

    fn assert_fell_back(c: &Child, reason_prefix: &str) {
        let v = |k: &str| c.vars.get(k).map(String::as_str);
        assert!(
            v("FAILED").is_some_and(|r| r.starts_with(reason_prefix)),
            "{reason_prefix}: {:?}",
            c.vars
        );
        assert_eq!(v("FPID"), Some(c.pid.to_string().as_str()), "the shell's own $$");
        assert_eq!(v("PID"), Some(c.pid.to_string().as_str()));
        assert_eq!(v("SESSION"), Some("unset"), "stale CINELOG_* removed");
        assert_eq!(v("MODE"), Some("unset"), "stale CINELOG_* removed");
        assert!(c.casts().is_empty(), "no cast left behind: {:?}", c.casts());
        assert!(
            c.rec_log().contains(&format!("not recording: {reason_prefix}")),
            "{}",
            c.rec_log()
        );
    }

    #[test]
    fn invalid_config_runs_the_shell_unrecorded() {
        let pty = nix::pty::openpty(None, None).unwrap();
        crate::pty::set_cloexec(pty.master.as_raw_fd()).unwrap();
        let tty = nix::unistd::ttyname(pty.slave.as_fd()).unwrap();
        let c = run_child("config", pty.slave.into(), |h| {
            std::fs::write(h.join("config/cinelog/config.toml"), "record = [not toml").unwrap();
        });
        assert_fell_back(&c, "config:");
        assert_eq!(c.vars.get("FTTY").map(PathBuf::from), Some(tty));
    }

    #[test]
    fn unwritable_logs_dir_runs_the_shell_unrecorded() {
        if nix::unistd::geteuid().is_root() {
            return; // root writes through 0500
        }
        let c = run_child("logs", std::process::Stdio::null(), |h| {
            use std::os::unix::fs::PermissionsExt;
            let ro = h.join("ro");
            std::fs::create_dir(&ro).unwrap();
            std::fs::set_permissions(&ro, std::fs::Permissions::from_mode(0o500)).unwrap();
            let cfg = format!("[paths]\nlogs = \"{}\"\n", ro.join("logs").display());
            std::fs::write(h.join("config/cinelog/config.toml"), cfg).unwrap();
        });
        assert_fell_back(&c, "logs:");
        assert_eq!(c.vars.get("FTTY").map(String::as_str), Some(""), "stdin is no tty");
    }

    #[test]
    fn fork_failure_deletes_the_cast_and_runs_the_shell_unrecorded() {
        if nix::unistd::geteuid().is_root() {
            return; // RLIMIT_NPROC does not apply to root
        }
        let c = run_child("nproc", std::process::Stdio::null(), |_| {});
        assert!(
            c.home.path().join("logs").is_dir(),
            "prepare() got as far as the cast dir"
        );
        assert_fell_back(&c, "fork:");
    }
}
