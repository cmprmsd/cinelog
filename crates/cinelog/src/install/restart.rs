//! Restarting a running `cinelog serve` that was started outside systemd (`serve --ensure`
//! without a unit, or by hand) so it runs the binary an upgrade installed (§10.2 step 5,
//! §10.3). A unit-managed server is left to `systemctl try-restart`.
//!
//! The server is found through `serve.lock`, whose holder is verified against the kernel's
//! lock table, so a stale pid is never signalled. It is stopped with SIGTERM and started
//! again from the new binary with its original arguments (by the unit's manager when one
//! is installed, else detached, which is also the fallback). Recorders are never
//! touched: they keep running on their `(deleted)` executable until their shell exits.

use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use super::Steps;
use crate::serve::lock;

/// How long a stopping server gets to release `serve.lock` (it commits the index first).
pub const STOP_GRACE: Duration = Duration::from_secs(20);
/// How long the new server gets to take `serve.lock`.
const START_GRACE: Duration = Duration::from_secs(5);
const POLL: Duration = Duration::from_millis(50);

/// What [`restart_stale`] found or did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServerRestart {
    /// No process holds `serve.lock`.
    NotRunning,
    /// The server already runs `exe`.
    Current { pid: u32 },
    /// It belongs to a systemd unit, which restarts it.
    Managed { pid: u32, unit: String },
    /// It was stopped and a new one started from `exe`.
    Restarted { old: u32, new: u32 },
    /// Left alone or only half done; the reason was reported as a warning.
    Failed,
}

/// How the stopped server is started again.
pub enum Start<'a> {
    /// A detached `exe <original arguments>`.
    Detached,
    /// Through this callback (e.g. `systemctl start` of an installed unit, which then owns
    /// the server); it gets the original arguments and returns whether it succeeded. When
    /// it fails, or no server takes `serve.lock` in time, a detached server is started
    /// instead, as `serve --ensure` does when the manager cannot start the unit.
    With(&'a mut dyn FnMut(&[String], &mut Steps) -> bool),
}

/// The systemd unit that may own the server; its server is left to systemd.
#[derive(Debug, Clone, Copy)]
pub struct Unit<'a> {
    pub name: &'a str,
    /// A unit of the user manager (`systemctl --user`).
    pub user: bool,
}

impl Unit<'_> {
    /// `systemctl [--user] <verb> <unit>`.
    pub fn command(&self, verb: &str) -> String {
        let scope = if self.user { "--user " } else { "" };
        format!("systemctl {scope}{verb} {}", self.name)
    }
}

/// The running server of `runtime_dir`, restarted from `exe` unless it already runs that
/// file. A server in `unit`'s cgroup is left to systemd.
pub fn restart_stale(
    runtime_dir: &Path,
    exe: &Path,
    euid: u32,
    unit: Unit<'_>,
    start: Start<'_>,
    steps: &mut Steps,
) -> ServerRestart {
    let Some(pid) = lock::holder_pid(runtime_dir) else {
        return ServerRestart::NotRunning;
    };
    let proc = PathBuf::from(format!("/proc/{pid}"));
    if std::fs::metadata(&proc).ok().map(|m| m.uid()) != Some(euid) {
        steps.warn(format!("the server (pid {pid}) belongs to another user; not restarted"));
        return ServerRestart::Failed;
    }
    if runs_file(&proc, exe) {
        steps.note(format!("the running server (pid {pid}) already runs {}", exe.display()));
        return ServerRestart::Current { pid };
    }
    if let Some(name) = std::fs::read_to_string(proc.join("cgroup"))
        .ok()
        .and_then(|cg| cgroup_unit(&cg, &[unit.name]))
    {
        steps.warn(format!(
            "the server (pid {pid}) of {name} still runs an older binary: `{}`",
            unit.command("restart")
        ));
        return ServerRestart::Managed { pid, unit: name };
    }
    let args = match std::fs::read(proc.join("cmdline")).ok().and_then(|c| serve_args(&c)) {
        Some(a) => a,
        None => {
            steps.warn(format!(
                "pid {pid} holds serve.lock but is not `cinelog serve`; not restarted"
            ));
            return ServerRestart::Failed;
        }
    };
    if let Err(e) = nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid as i32), nix::sys::signal::SIGTERM) {
        steps.warn(format!("cannot stop the server (pid {pid}): {e}"));
        return ServerRestart::Failed;
    }
    if !wait_for(STOP_GRACE, || !lock::is_held(runtime_dir)) {
        steps.warn(format!(
            "the server (pid {pid}) did not stop within {}s; it keeps running the old binary",
            STOP_GRACE.as_secs()
        ));
        return ServerRestart::Failed;
    }
    match start_again(runtime_dir, exe, &args, start, START_GRACE, steps) {
        Some(new) => {
            steps.done(format!(
                "restarted the server (pid {pid} → {new}) with {}",
                exe.display()
            ));
            ServerRestart::Restarted { old: pid, new }
        }
        None => {
            steps.warn(format!("stopped the old server (pid {pid}); see `cinelog status`"));
            ServerRestart::Failed
        }
    }
}

/// Starts the stopped server again; the pid of the process that then holds `serve.lock`.
fn start_again(
    runtime_dir: &Path,
    exe: &Path,
    args: &[String],
    start: Start<'_>,
    grace: Duration,
    steps: &mut Steps,
) -> Option<u32> {
    if let Start::With(f) = start {
        if !f(args, steps) {
            steps.note("starting a detached server instead");
        } else if let Some(pid) = wait_holder(runtime_dir, grace) {
            return Some(pid);
        } else {
            steps.warn(format!(
                "no server took serve.lock within {}s; starting a detached one",
                grace.as_secs()
            ));
        }
    }
    let argv: Vec<&str> = args.iter().map(String::as_str).collect();
    if let Err(e) = lock::spawn_detached(exe, &argv) {
        steps.warn(format!(
            "cannot start {}: {e}; the next recorded shell starts a server",
            exe.display()
        ));
        return None;
    }
    let new = wait_holder(runtime_dir, grace);
    if new.is_none() {
        steps.warn(format!(
            "the new server has not taken serve.lock within {}s",
            grace.as_secs()
        ));
    }
    new
}

fn wait_holder(runtime_dir: &Path, grace: Duration) -> Option<u32> {
    let mut pid = None;
    wait_for(grace, || {
        pid = lock::holder_pid(runtime_dir);
        pid.is_some()
    });
    pid
}

/// Whether the process at `proc` (`/proc/<pid>`) executes the file `exe` (the same inode,
/// so a replaced binary, shown as `(deleted)`, never matches its successor).
pub fn runs_file(proc: &Path, exe: &Path) -> bool {
    match (std::fs::metadata(proc.join("exe")), std::fs::metadata(exe)) {
        (Ok(a), Ok(b)) => a.dev() == b.dev() && a.ino() == b.ino(),
        _ => false,
    }
}

/// The arguments after `argv[0]` of a `cinelog serve …` command line (NUL-separated).
pub fn serve_args(cmdline: &[u8]) -> Option<Vec<String>> {
    let mut parts = cmdline
        .split(|b| *b == 0)
        .map(|p| String::from_utf8_lossy(p).into_owned());
    let argv0 = parts.next()?;
    let args: Vec<String> = parts.collect();
    let args = match args.last() {
        Some(last) if last.is_empty() => args[..args.len() - 1].to_vec(),
        _ => args,
    };
    let base = Path::new(&argv0).file_name()?.to_string_lossy().into_owned();
    (base == "cinelog" && args.first().is_some_and(|a| a == "serve") && !args.iter().any(|a| a == "--ensure"))
        .then_some(args)
}

/// The unit of `units` a `/proc/<pid>/cgroup` text places the process in.
pub fn cgroup_unit(cgroup: &str, units: &[&str]) -> Option<String> {
    cgroup.lines().find_map(|line| {
        let path = line.splitn(3, ':').nth(2)?;
        path.split('/').find(|seg| units.contains(seg)).map(str::to_owned)
    })
}

fn wait_for(timeout: Duration, mut done: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if done() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(POLL);
    }
}

/// Stand-ins for `cinelog serve` processes that hold `serve.lock`, built from `flock(1)`.
#[cfg(test)]
pub(crate) mod fake {
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};

    use super::*;

    /// flock's child: records flock's pid in the lock file (as `serve` does) and drops the
    /// inherited lock descriptor, so the lock goes with flock when it is stopped.
    pub const CHILD: &str = "echo $PPID > serve.lock; exec 3>&- 4>&- 5>&- 6>&- 7>&- 8>&- 9>&-; exec sleep 60";

    /// Kills every process group it was given (and reaps the leaders) when dropped, so a
    /// failing test leaves nothing running.
    #[derive(Default)]
    pub struct Reaper(Vec<i32>);

    impl Reaper {
        pub fn add(&mut self, pid: u32) {
            self.0.push(pid as i32);
        }
    }

    impl Drop for Reaper {
        fn drop(&mut self) {
            use nix::sys::signal::{SIGKILL, killpg};
            use nix::unistd::Pid;
            for &pid in &self.0 {
                let _ = killpg(Pid::from_raw(pid), SIGKILL);
                let _ = nix::sys::wait::waitpid(Pid::from_raw(pid), None);
            }
        }
    }

    /// An old server of `runtime_dir`, `argv[0]` `argv0`: `flock serve sh -c CHILD`, where
    /// `serve` links to `serve.lock`. Its pid once it holds the lock.
    pub fn old_server(runtime_dir: &Path, argv0: &str, reaper: &mut Reaper) -> u32 {
        std::fs::create_dir_all(runtime_dir).unwrap();
        let link = runtime_dir.join("serve");
        if std::fs::symlink_metadata(&link).is_err() {
            std::os::unix::fs::symlink(lock::LOCK_FILE, &link).unwrap();
        }
        // The Reaper kills its process group and reaps it.
        #[allow(clippy::zombie_processes)]
        let child = Command::new("flock")
            .arg0(argv0)
            .args(["serve", "sh", "-c", CHILD])
            .current_dir(runtime_dir)
            .stdin(Stdio::null())
            .process_group(0)
            .spawn()
            .expect("flock(1)");
        let pid = child.id();
        reaper.add(pid);
        assert!(
            wait_for(Duration::from_secs(5), || lock::holder_pid(runtime_dir) == Some(pid)),
            "the fake server never took serve.lock"
        );
        pid
    }

    /// A new binary in `dir` that, whatever its arguments, serves `runtime_dir`.
    pub fn new_binary(dir: &Path, runtime_dir: &Path) -> PathBuf {
        let exe = dir.join("cinelog-new");
        let script = format!(
            "#!/bin/sh\n[ -n \"$FAKE_PROBE\" ] && exit 0\ncd '{}' && exec flock serve.lock sh -c '{CHILD}'\n",
            runtime_dir.display()
        );
        std::fs::write(&exe, script).unwrap();
        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
        let mut probe = Command::new(&exe);
        probe.env("FAKE_PROBE", "1");
        assert!(spawn_executable(&mut probe).wait().unwrap().success());
        exe
    }

    /// Spawns a just-written executable. A process another test thread forks while the file
    /// is still open for writing inherits that descriptor until it execs, and the kernel
    /// refuses to run the file meanwhile (ETXTBSY); that passes within moments.
    pub fn spawn_executable(cmd: &mut Command) -> std::process::Child {
        let mut tries = 0;
        loop {
            match cmd.spawn() {
                Err(e) if e.raw_os_error() == Some(libc::ETXTBSY) && tries < 100 => {
                    tries += 1;
                    std::thread::sleep(Duration::from_millis(10));
                }
                r => return r.expect("spawn"),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::process::{Command, Stdio};

    use super::fake::Reaper;
    use super::*;

    const UNIT: Unit<'static> = Unit {
        name: "cinelog.service",
        user: true,
    };

    fn euid() -> u32 {
        nix::unistd::geteuid().as_raw()
    }

    #[test]
    fn restart_commands_name_the_right_manager() {
        assert_eq!(UNIT.command("restart"), "systemctl --user restart cinelog.service");
        let team = Unit {
            name: "cinelog-team.service",
            user: false,
        };
        assert_eq!(team.command("restart"), "systemctl restart cinelog-team.service");
    }

    /// Regression: a failing `systemctl start` (no user manager in an ssh session, another
    /// init) used to leave no server at all after the old one was stopped.
    #[test]
    fn a_failed_unit_start_falls_back_to_a_detached_server() {
        let d = tempfile::tempdir().unwrap();
        let run = d.path().join("run");
        let mut reaper = Reaper::default();
        let old = fake::old_server(&run, "/opt/m/bin/cinelog", &mut reaper);
        let exe = fake::new_binary(d.path(), &run);
        let mut calls = Vec::new();
        let mut failing = |args: &[String], _: &mut Steps| {
            calls.push(args.to_vec());
            false
        };
        let mut steps = Steps::default();
        let r = restart_stale(&run, &exe, euid(), UNIT, Start::With(&mut failing), &mut steps);
        let new = match r {
            ServerRestart::Restarted { old: o, new } => {
                reaper.add(new);
                assert_eq!(o, old);
                new
            }
            other => panic!("{other:?}\n{steps}"),
        };
        assert_ne!(new, old);
        assert_eq!(lock::holder_pid(&run), Some(new));
        assert_eq!(
            calls,
            [vec!["serve".to_owned(), "sh".into(), "-c".into(), fake::CHILD.into()]]
        );
        let text = steps.to_string();
        assert!(text.contains("starting a detached server instead"), "{text}");
        assert!(
            text.contains(&format!("restarted the server (pid {old} → {new})")),
            "{text}"
        );
    }

    /// A unit start that reports success but starts nothing that takes the lock is also
    /// replaced by a detached server.
    #[test]
    fn a_unit_start_without_a_server_falls_back_too() {
        let d = tempfile::tempdir().unwrap();
        let run = d.path().join("run");
        std::fs::create_dir_all(&run).unwrap();
        let exe = fake::new_binary(d.path(), &run);
        let mut reaper = Reaper::default();
        let mut ok = |_: &[String], _: &mut Steps| true;
        let mut steps = Steps::default();
        let new = start_again(
            &run,
            &exe,
            &[],
            Start::With(&mut ok),
            Duration::from_millis(300),
            &mut steps,
        );
        if let Some(pid) = new {
            reaper.add(pid);
        }
        assert!(new.is_some(), "{steps}");
        assert!(steps.to_string().contains("no server took serve.lock"), "{steps}");
    }

    #[test]
    fn a_lock_holder_that_is_not_a_server_is_left_alone() {
        let d = tempfile::tempdir().unwrap();
        let run = d.path().join("run");
        let mut reaper = Reaper::default();
        let pid = fake::old_server(&run, "flock", &mut reaper);
        let mut steps = Steps::default();
        let exe = fake::new_binary(d.path(), &run);
        let r = restart_stale(&run, &exe, euid(), UNIT, Start::Detached, &mut steps);
        assert_eq!(r, ServerRestart::Failed);
        assert!(steps.to_string().contains("is not `cinelog serve`"), "{steps}");
        assert_eq!(lock::holder_pid(&run), Some(pid), "not signalled");
    }

    #[test]
    fn serve_command_lines() {
        let line = |s: &str| s.replace(' ', "\0").into_bytes();
        assert_eq!(
            serve_args(&line("/m/bin/cinelog serve --scope personal --no-http ")),
            Some(vec![
                "serve".into(),
                "--scope".into(),
                "personal".into(),
                "--no-http".into()
            ])
        );
        assert_eq!(serve_args(&line("cinelog serve")), Some(vec!["serve".into()]));
        assert_eq!(serve_args(&line("/usr/bin/sleep 60")), None);
        assert_eq!(serve_args(&line("/x/cinelog rec -- zsh")), None);
        assert_eq!(
            serve_args(&line("/x/cinelog serve --ensure")),
            None,
            "a one-shot, not a server"
        );
        assert_eq!(serve_args(b""), None);
    }

    #[test]
    fn unit_from_cgroup() {
        let units = ["cinelog.service", "cinelog-team.service"];
        let v2 = "0::/user.slice/user-1000.slice/user@1000.service/app.slice/cinelog.service\n";
        assert_eq!(cgroup_unit(v2, &units).as_deref(), Some("cinelog.service"));
        let team = "0::/system.slice/cinelog-team.service\n";
        assert_eq!(cgroup_unit(team, &units).as_deref(), Some("cinelog-team.service"));
        let shell = "0::/user.slice/user-1000.slice/session-3.scope\n";
        assert_eq!(cgroup_unit(shell, &units), None);
        let v1 = "12:pids:/system.slice/cinelog-team.service\n1:name=systemd:/system.slice/cinelog-team.service\n";
        assert_eq!(cgroup_unit(v1, &units).as_deref(), Some("cinelog-team.service"));
    }

    /// The upgrade case: the running file is replaced by rename; the process keeps the old
    /// inode, so it no longer runs the file at that path.
    #[test]
    fn a_replaced_binary_is_detected() {
        let d = tempfile::tempdir().unwrap();
        let exe = d.path().join("sleeper");
        std::fs::copy("/bin/sleep", &exe).unwrap();
        let mut child = fake::spawn_executable(Command::new(&exe).arg("30").stdin(Stdio::null()));
        let proc = PathBuf::from(format!("/proc/{}", child.id()));
        assert!(wait_for(Duration::from_secs(5), || runs_file(&proc, &exe)));

        let next = d.path().join(".sleeper.new");
        std::fs::copy("/bin/sleep", &next).unwrap();
        std::fs::rename(&next, &exe).unwrap();
        let replaced = runs_file(&proc, &exe);
        let link = std::fs::read_link(proc.join("exe")).unwrap();
        child.kill().unwrap();
        child.wait().unwrap();
        assert!(!replaced);
        assert!(link.to_string_lossy().ends_with(" (deleted)"), "{}", link.display());
        assert!(!runs_file(&proc, &exe), "a gone process runs nothing");
    }

    #[test]
    fn nothing_to_restart_without_a_lock_holder() {
        let d = tempfile::tempdir().unwrap();
        let mut steps = Steps::default();
        let r = restart_stale(d.path(), Path::new("/bin/sleep"), 0, UNIT, Start::Detached, &mut steps);
        assert_eq!(r, ServerRestart::NotRunning);
        assert_eq!(steps.to_string(), "");
    }
}
