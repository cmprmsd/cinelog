//! Version skew between this binary and the other parts of an installation (plan §10.3):
//! the plugin checkout, the team binary, this scope's running server and, with
//! [`Reach::Network`], the team server a member logged in to (`GET /healthz`).
//!
//! Marker protocol v1, the web API v1 and the DB schemas are frozen across 1.x, so skew
//! within 1.x never breaks recording; these checks only say what to update and how.
//! `cinelog doctor` runs all of them. `cinelog doctor --skew` is the plugin's MOTD line: it
//! runs the local ones, prints one line per warning and always exits 0.

use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, ExitCode, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicI32, Ordering};
use std::time::Duration;

use cinelog_core::Scope;
use cinelog_core::paths::team;
use cinelog_core::server::ServerInfo;
use cinelog_web::api::types::Health;

use super::{Check, Status};
use crate::ctx::Ctx;
use crate::module;
use crate::token::{TeamClient, TeamCredentials};

/// How far the checks may look.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reach {
    /// Files and local processes only; fast enough for every shell start.
    Local,
    /// Also the team server over HTTPS (bounded by the client's connect timeout).
    Network,
}

/// Every skew check that applies to `ctx`, in a fixed order.
pub fn checks(ctx: &Ctx, reach: Reach) -> Vec<Check> {
    let me = cinelog_core::VERSION;
    let mut out = vec![plugin(me), team_binary(ctx, me)];
    out.extend(server(ctx, me));
    if reach == Reach::Network {
        out.extend(team_server(ctx, me));
    }
    out
}

/// `cinelog doctor --skew`: the warnings of the local checks, one line each.
pub fn motd(ctx: &Ctx) -> ExitCode {
    for line in motd_lines(&checks(ctx, Reach::Local)) {
        println!("{line}");
    }
    ExitCode::SUCCESS
}

/// The MOTD lines for `checks`: warnings and failures only.
pub fn motd_lines(checks: &[Check]) -> Vec<String> {
    checks
        .iter()
        .filter(|c| c.status >= Status::Warn)
        .map(|c| format!("cinelog: {}", c.message))
        .collect()
}

/// Whether `theirs` is older than `mine`. An unparsable version counts as older.
pub fn older(theirs: &str, mine: &str) -> bool {
    module::parse_version(theirs) < module::parse_version(mine)
}

fn check(id: &'static str, status: Status, message: impl Into<String>) -> Check {
    Check {
        id,
        status,
        message: message.into(),
    }
}

fn plugin(me: &str) -> Check {
    let Some(m) = module::current_exe().ok().as_deref().and_then(module::find_from_exe) else {
        return check(
            "version",
            Status::Info,
            format!("binary {me}, not inside a module checkout"),
        );
    };
    match module::version(&m) {
        Some(v) if v == me => check(
            "version",
            Status::Ok,
            format!("binary {me}, plugin {v} ({})", m.display()),
        ),
        Some(v) => check(
            "version",
            Status::Warn,
            format!("binary {me} but plugin {v} in {}: run cinelog-fetch", m.display()),
        ),
        None => check("version", Status::Warn, format!("{} has no VERSION file", m.display())),
    }
}

fn team_binary(ctx: &Ctx, me: &str) -> Check {
    let team_bin = ctx.root.join(team::BINARY);
    if !team_bin.exists() {
        return check(
            "team-binary",
            Status::Info,
            format!("no team install ({})", module::sudo_install_team("")),
        );
    }
    let theirs = version_of(&team_bin, VERSION_WAIT).and_then(|s| s.split_whitespace().nth(1).map(str::to_owned));
    match theirs {
        Some(v) if older(&v, me) => check(
            "team-binary",
            Status::Warn,
            format!(
                "{} is {v}, older than {me}: {}",
                team_bin.display(),
                module::sudo_install_team(" --upgrade")
            ),
        ),
        Some(v) => check("team-binary", Status::Ok, format!("{} is {v}", team_bin.display())),
        None => check(
            "team-binary",
            Status::Warn,
            format!(
                "{} does not report a version within {} ms",
                team_bin.display(),
                VERSION_WAIT.as_millis()
            ),
        ),
    }
}

/// How long the team binary may take to answer `--version` (a dead mount must not stall
/// `doctor --skew` at shell start).
const VERSION_WAIT: Duration = Duration::from_millis(500);

/// `<bin> --version`'s stdout, or `None` when it fails or takes longer than `wait`. Spawning
/// happens on a helper thread because `spawn` itself blocks until the exec succeeds; on a
/// timeout the child's process group is killed and the thread reaps it.
fn version_of(bin: &Path, wait: Duration) -> Option<String> {
    let pgid = Arc::new(AtomicI32::new(0));
    let (tx, rx) = crossbeam_channel::bounded(1);
    let bin = bin.to_owned();
    let child_pgid = Arc::clone(&pgid);
    std::thread::spawn(move || {
        let out = Command::new(&bin)
            .arg("--version")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .process_group(0)
            .spawn()
            .and_then(|child| {
                child_pgid.store(child.id() as i32, Ordering::SeqCst);
                child.wait_with_output()
            });
        let _ = tx.send(out);
    });
    match rx.recv_timeout(wait) {
        Ok(Ok(out)) if out.status.success() => String::from_utf8(out.stdout).ok(),
        Ok(_) => None,
        Err(_) => {
            let pg = pgid.load(Ordering::SeqCst);
            if pg > 0 {
                // SAFETY: kill(2) with a negative pid signals only the child's own group.
                unsafe { libc::kill(-pg, libc::SIGKILL) };
            }
            None
        }
    }
}

/// This scope's running server, from `server.json`.
fn server(ctx: &Ctx, me: &str) -> Option<Check> {
    let info = ServerInfo::read_live(&ctx.paths.runtime_dir)?;
    if info.version.is_empty() {
        return None;
    }
    let restart = match ctx.scope() {
        Scope::Personal => "cinelog install --refresh".to_owned(),
        Scope::Team => team_restart(info.pid),
    };
    Some(if older(&info.version, me) {
        check(
            "server-version",
            Status::Warn,
            format!(
                "the running server (pid {}) is {}, older than {me}: restart it with `{restart}`",
                info.pid, info.version
            ),
        )
    } else {
        check(
            "server-version",
            Status::Ok,
            format!("the running server is {}", info.version),
        )
    })
}

/// How to restart the team server `pid`: through its unit when systemd runs it, else (a
/// detached server of root's shells) the upgrade install, which restarts it.
fn team_restart(pid: u32) -> String {
    let unit = crate::install::team::UNIT_NAME;
    let by_unit = std::fs::read_to_string(format!("/proc/{pid}/cgroup"))
        .ok()
        .and_then(|cg| crate::install::restart::cgroup_unit(&cg, &[unit]))
        .is_some();
    if by_unit {
        format!("systemctl try-restart {unit}")
    } else {
        module::sudo_install_team(" --upgrade")
    }
}

/// The team server of `cinelog token team-login`, if this user has logged in to one.
fn team_server(ctx: &Ctx, me: &str) -> Option<Check> {
    if ctx.scope() == Scope::Team || !TeamCredentials::path(ctx).exists() {
        return None;
    }
    let client = match TeamClient::from_saved(ctx) {
        Ok(c) => c,
        Err(e) => return Some(check("team-server", Status::Warn, format!("{e:#}"))),
    };
    let url = client.url.to_string();
    let health = client.send("GET", "/healthz", None, None).and_then(|r| match r.status {
        200 => Ok(serde_json::from_slice::<Health>(&r.body)?),
        s => anyhow::bail!("/healthz answered {s}"),
    });
    Some(match health {
        Ok(h) if older(&h.version, me) => check(
            "team-server",
            Status::Warn,
            format!(
                "team server {url} runs {}, older than {me}: on that host run \
                 `sudo <module>/bin/cinelog install --team --upgrade`",
                h.version
            ),
        ),
        Ok(h) => check(
            "team-server",
            Status::Ok,
            format!("team server {url} runs {}", h.version),
        ),
        Err(e) => check(
            "team-server",
            Status::Info,
            format!("team server {url} not reachable: {e:#}"),
        ),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn older_compares_numerically() {
        assert!(older("0.12.0", "1.0.0"));
        assert!(older("0.9.12", "0.10.0"));
        assert!(!older("1.0.0", "1.0.0"));
        assert!(!older("1.1.0", "1.0.0"));
        assert!(older("garbage", "1.0.0"));
    }

    fn fake_team_binary(root: &std::path::Path, version_line: &str) {
        use std::os::unix::fs::PermissionsExt;
        let bin = root.join("usr/local/bin/cinelog");
        std::fs::create_dir_all(bin.parent().unwrap()).unwrap();
        let tmp = bin.with_extension("tmp");
        std::fs::write(&tmp, format!("#!/bin/sh\necho '{version_line}'\n")).unwrap();
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::rename(&tmp, &bin).unwrap();
    }

    fn by_id<'a>(checks: &'a [Check], id: &str) -> &'a Check {
        checks
            .iter()
            .find(|c| c.id == id)
            .unwrap_or_else(|| panic!("no {id} in {checks:?}"))
    }

    #[test]
    fn an_older_team_binary_names_the_upgrade_command() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path().join("root");
        let ctx = Ctx::with(
            crate::check::tests::test_env(d.path()),
            cinelog_core::SystemRoot::prefixed(&root),
        );
        let none = checks(&ctx, Reach::Local);
        assert_eq!(by_id(&none, "team-binary").status, Status::Info);

        fake_team_binary(&root, "cinelog 0.12.0");
        let old = checks(&ctx, Reach::Local);
        let c = by_id(&old, "team-binary");
        assert_eq!(c.status, Status::Warn);
        assert!(c.message.contains("0.12.0, older than"), "{}", c.message);
        assert!(
            c.message.contains(" install --team --upgrade") && c.message.contains("sudo /"),
            "the full path of this binary, never a bare `sudo cinelog`: {}",
            c.message
        );
        let motd = motd_lines(&old);
        assert!(
            motd.iter()
                .any(|l| l.starts_with("cinelog: ") && l.contains("--upgrade")),
            "{motd:?}"
        );
        assert!(motd.iter().all(|l| !l.contains('\x1b')), "no ANSI: {motd:?}");

        fake_team_binary(&root, &format!("cinelog {}", cinelog_core::VERSION));
        assert_eq!(by_id(&checks(&ctx, Reach::Local), "team-binary").status, Status::Ok);
        fake_team_binary(&root, "garbage");
        assert_eq!(by_id(&checks(&ctx, Reach::Local), "team-binary").status, Status::Warn);
    }

    #[test]
    fn a_hanging_team_binary_is_given_up_on_and_killed() {
        use std::os::unix::fs::PermissionsExt;
        let d = tempfile::tempdir().unwrap();
        let bin = d.path().join("cinelog");
        let pidfile = d.path().join("pid");
        std::fs::write(
            &bin,
            format!("#!/bin/sh\necho $$ > '{}'\nexec sleep 30\n", pidfile.display()),
        )
        .unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();

        let t0 = std::time::Instant::now();
        assert_eq!(version_of(&bin, Duration::from_millis(300)), None);
        assert!(t0.elapsed() < Duration::from_secs(2), "{:?}", t0.elapsed());

        let pid: i32 = std::fs::read_to_string(&pidfile).unwrap().trim().parse().unwrap();
        let gone = (0..100).any(|_| {
            std::thread::sleep(Duration::from_millis(20));
            // SAFETY: signal 0 only checks that the pid exists.
            (unsafe { libc::kill(pid, 0) }) != 0
        });
        assert!(gone, "the hanging child {pid} was not killed and reaped");
    }

    #[test]
    fn an_older_running_server_is_reported_with_the_restart_command() {
        let d = tempfile::tempdir().unwrap();
        let ctx = Ctx::with(
            crate::check::tests::test_env(d.path()),
            cinelog_core::SystemRoot::prefixed(d.path().join("root")),
        );
        assert!(checks(&ctx, Reach::Local).iter().all(|c| c.id != "server-version"));
        std::fs::create_dir_all(&ctx.paths.runtime_dir).unwrap();
        let mut info = ServerInfo {
            pid: std::process::id(),
            url: "https://box:11000".into(),
            port: 11000,
            fingerprint: String::new(),
            version: "0.12.0".into(),
        };
        info.write(&ctx.paths.runtime_dir).unwrap();
        let c = by_id(&checks(&ctx, Reach::Local), "server-version").clone();
        assert_eq!(c.status, Status::Warn);
        assert!(c.message.contains("cinelog install --refresh"), "{}", c.message);

        info.version = cinelog_core::VERSION.into();
        info.write(&ctx.paths.runtime_dir).unwrap();
        assert_eq!(by_id(&checks(&ctx, Reach::Local), "server-version").status, Status::Ok);
        // A newer server (a downgraded binary) is not skew this binary can fix.
        info.version = "99.0.0".into();
        info.write(&ctx.paths.runtime_dir).unwrap();
        assert_eq!(by_id(&checks(&ctx, Reach::Local), "server-version").status, Status::Ok);
    }

    /// A detached team server (no unit runs it) is restarted by the upgrade install, not by
    /// `systemctl try-restart`, which would do nothing.
    #[test]
    fn an_older_detached_team_server_names_the_upgrade_install() {
        let pid = std::process::id();
        let hint = team_restart(pid);
        assert!(hint.contains("install --team --upgrade"), "{hint}");
        assert!(!hint.contains("systemctl"), "{hint}");
    }

    #[test]
    fn motd_shows_only_problems() {
        let checks = [
            check("version", Status::Ok, "binary 1.0.0, plugin 1.0.0"),
            check(
                "team-binary",
                Status::Warn,
                "/usr/local/bin/cinelog is 0.12.0, older than 1.0.0",
            ),
            check("team-server", Status::Info, "team server not reachable"),
        ];
        assert_eq!(
            motd_lines(&checks),
            ["cinelog: /usr/local/bin/cinelog is 0.12.0, older than 1.0.0"]
        );
    }
}
