//! Version skew between this binary and the other parts of an installation (plan §10.3):
//! the plugin checkout, the team binary, this scope's running server and, with
//! [`Reach::Network`], the team server a member logged in to (`GET /healthz`).
//!
//! Marker protocol v1, the web API v1 and the DB schemas are frozen across 1.x, so skew
//! within 1.x never breaks recording; these checks only say what to update and how.
//! `cinelog doctor` runs all of them. `cinelog doctor --skew` is the plugin's MOTD line: it
//! runs the local ones, prints one line per warning and always exits 0.

use std::process::ExitCode;

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
    let out = std::process::Command::new(&team_bin).arg("--version").output();
    let theirs = out
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .and_then(|s| s.split_whitespace().nth(1).map(str::to_owned));
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
            format!("{} does not report a version", team_bin.display()),
        ),
    }
}

/// This scope's running server, from `server.json`.
fn server(ctx: &Ctx, me: &str) -> Option<Check> {
    let info = ServerInfo::read_live(&ctx.paths.runtime_dir)?;
    if info.version.is_empty() {
        return None;
    }
    let restart = match ctx.scope() {
        Scope::Personal => "cinelog install --refresh",
        Scope::Team => "systemctl try-restart cinelog-team",
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
