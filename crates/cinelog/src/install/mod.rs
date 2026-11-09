//! `cinelog install` (§10.6): PERSONAL by default, TEAM with `--team`.

pub mod fsutil;
pub mod migrate;
pub mod old_server;
pub mod personal;
pub mod restart;
pub mod team;
pub mod zshrc;

use std::fmt;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, Result};
use cinelog_core::paths::{EnvSnapshot, Paths, SystemRoot};
use cinelog_core::server::ServerInfo;
use cinelog_core::{Config, Scope};
use cinelog_index::model::{DbPaths, settings_keys};
use cinelog_web::server::BindSpec;
use fsutil::{Outcome, pretty};

use crate::cli::InstallArgs;
use crate::ctx::Ctx;
use crate::{doctor, module};

/// What an install did, in order, for the summary.
#[derive(Debug, Default)]
pub struct Steps {
    lines: Vec<String>,
    finale: Option<Finale>,
}

/// The last lines of the output, below the summary: where to sign in, and the login token
/// when the install created one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finale {
    pub text: String,
    /// `text` holds a secret: it is printed outside the recording of the terminal.
    pub secret: bool,
}

impl Steps {
    pub fn finale(&self) -> Option<&Finale> {
        self.finale.as_ref()
    }

    pub fn set_finale(&mut self, text: String, secret: bool) {
        self.finale = Some(Finale { text, secret });
    }

    fn push(&mut self, mark: &str, msg: String) {
        self.lines.push(format!("  {mark} {msg}"));
    }

    pub fn done(&mut self, msg: String) {
        self.push("✓", msg);
    }

    pub fn note(&mut self, msg: impl Into<String>) {
        self.push("·", msg.into());
    }

    pub fn notes(&mut self, msgs: Vec<String>) {
        for m in msgs {
            self.note(m);
        }
    }

    pub fn warn(&mut self, msg: String) {
        self.push("!", msg);
    }

    pub fn dir(&mut self, outcome: &Outcome, dir: &Path, what: &str, home: &Path) {
        let p = pretty(dir, home);
        let what = if what.is_empty() {
            String::new()
        } else {
            format!(" ({what})")
        };
        match outcome {
            Outcome::Created => self.done(format!("created {p}{what}")),
            Outcome::Tightened(old) => self.done(format!("tightened {p} from {old:o} to private{what}")),
            Outcome::Unchanged | Outcome::Replaced => self.note(format!("kept {p}{what}")),
        }
    }

    pub fn file(&mut self, outcome: &Outcome, file: &Path, detail: &str, home: &Path) {
        let p = pretty(file, home);
        match outcome {
            Outcome::Created => self.done(format!("installed {p} {detail}")),
            Outcome::Replaced | Outcome::Tightened(_) => self.done(format!("updated {p} {detail}")),
            Outcome::Unchanged => self.note(format!("{p} is up to date {detail}")),
        }
    }
}

impl fmt::Display for Steps {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for l in &self.lines {
            writeln!(f, "{}", l.trim_end())?;
        }
        Ok(())
    }
}

/// The web UI's address: the running server's, else the one it will bind (a port in
/// `server.bind`, the port it persisted, the old viewer's port, else the `auto` port of
/// §7.1) on its first `server.hostnames` entry or this host's name. Members cannot read
/// root's `server.json`, and the personal server may still be starting, so the install
/// prints this.
pub fn web_url(cfg: &Config, paths: &Paths, scope: Scope, uid: u32, legacy_port: Option<u16>) -> String {
    if let Some(live) = ServerInfo::read_live(&paths.runtime_dir) {
        return live.url;
    }
    let bound = cfg.server.bind.iter().find_map(|b| b.parse::<BindSpec>().ok()?.port);
    let port = bound
        .or_else(|| persisted_port(&DbPaths::in_dir(&paths.db_dir).meta))
        .or_else(|| old_server::persisted_port(legacy_port, &paths.migrated_port_file()))
        .unwrap_or_else(|| cinelog_core::server::auto_port(scope, uid));
    let host = cfg
        .server
        .hostnames
        .first()
        .cloned()
        .unwrap_or_else(cinelog_core::paths::hostname);
    let host = if host.contains(':') { format!("[{host}]") } else { host };
    format!("https://{host}:{port}")
}

/// The port the server persisted in meta.db (read-only; `None` without one).
fn persisted_port(meta: &Path) -> Option<u16> {
    use cinelog_index::schema::{self, Access, DbKind};
    if !meta.exists() {
        return None;
    }
    let conn = schema::open(meta, DbKind::Meta, Access::ReadOnly).ok()?;
    conn.query_row(
        "SELECT value FROM settings WHERE key = ?1",
        [settings_keys::PORT],
        |r| r.get::<_, Option<String>>(0),
    )
    .ok()
    .flatten()?
    .parse()
    .ok()
}

pub fn user_unit_file(env: &EnvSnapshot) -> PathBuf {
    env.config_home().join("systemd/user/cinelog.service")
}

/// The calling user's systemd manager, as far as the personal install may drive it.
pub struct UserManager {
    /// The unit directory the manager reads (`~/.config/systemd/user` below the passwd
    /// home); `None` when unknown.
    pub unit_dir: Option<PathBuf>,
    /// Runs `systemctl` (reports into the steps); whether it succeeded.
    pub run: fn(&[&str], &mut Steps) -> bool,
    /// Linger is enabled: the manager runs without a login session.
    pub linger: bool,
}

impl UserManager {
    pub fn real(euid: u32) -> Self {
        let user = nix::unistd::User::from_uid(nix::unistd::Uid::from_raw(euid))
            .ok()
            .flatten();
        Self {
            unit_dir: user.as_ref().map(|u| u.dir.join(".config/systemd/user")),
            run: systemctl,
            linger: user.is_some_and(|u| Path::new("/var/lib/systemd/linger").join(u.name).exists()),
        }
    }

    /// Whether the manager reads `unit`: only then may `systemctl --user` act on it (an
    /// install into another HOME, such as a test sandbox, never touches the real manager).
    pub fn reads(&self, unit: &Path) -> bool {
        let canon = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
        match (&self.unit_dir, unit.parent()) {
            (Some(dir), Some(parent)) => canon(dir) == canon(parent),
            _ => false,
        }
    }

    pub fn systemctl(&self, args: &[&str], steps: &mut Steps) -> bool {
        (self.run)(args, steps)
    }
}

/// Runs `systemctl` for an installed unit; failures are reported, never fatal. Whether it
/// succeeded.
pub fn systemctl(args: &[&str], steps: &mut Steps) -> bool {
    let line = format!("systemctl {}", args.join(" "));
    match std::process::Command::new("systemctl").args(args).status() {
        Ok(s) if s.success() => {
            steps.done(line);
            true
        }
        Ok(s) => {
            steps.warn(format!("{line} failed ({s})"));
            false
        }
        Err(e) => {
            steps.warn(format!("{line}: {e}"));
            false
        }
    }
}

fn team_system(args: &InstallArgs) -> Result<team::System> {
    let root = args
        .prefix
        .as_ref()
        .map_or_else(SystemRoot::real, |p| SystemRoot::prefixed(p.clone()));
    let sys = team::System::new(root, nix::unistd::geteuid().as_raw());
    team::check_privileges(&sys.root, sys.euid)?;
    Ok(sys)
}

fn team_options(args: &InstallArgs, sys: team::System) -> Result<team::Options> {
    let running = module::current_exe().context("cannot locate the running binary")?;
    let module = match &args.module {
        Some(m) => m.clone(),
        None => module::find_from_exe(&running).with_context(|| {
            format!(
                "{} is not inside a cinelog module checkout: pass --module DIR, or run the \
                 module's own binary (sudo ~/.zim/modules/cinelog/bin/cinelog install --team ...)",
                running.display()
            )
        })?,
    };
    let binary = team::source_binary(args.binary.as_deref(), &module, &running)?;
    Ok(team::Options {
        sys,
        module,
        binary,
        system_zshrc: args.system_zshrc,
        upgrade: args.upgrade,
        allow_unverified: args.allow_unverified,
        migrate: !args.no_migrate,
    })
}

fn execute(args: &InstallArgs, steps: &mut Steps) -> Result<Option<doctor::Report>> {
    if args.team {
        let sys = team_system(args)?;
        if args.uninstall {
            // Resolving a module or binary here would break uninstalling from
            // /usr/local/bin or after a rollback to a release without bin/cinelog.
            team::uninstall(&sys, steps)?;
        } else {
            team::install(&team_options(args, sys)?, steps)?;
        }
        return Ok(None);
    }
    let env = EnvSnapshot::capture();
    let manager = UserManager::real(env.euid);
    if args.prefix.is_some() {
        steps.note("--prefix only applies to --team; the personal install writes no system paths");
    }
    if args.refresh {
        personal::refresh(env, &manager, steps)?;
        return Ok(None);
    }
    if args.uninstall {
        personal::uninstall(env, &manager, steps)?;
        return Ok(None);
    }
    let opts = personal::Options {
        migrate: !args.no_migrate,
        exe: None,
        manager,
        proc_root: old_server::proc_root(),
    };
    let ctx = personal::install(env, &opts, steps)?;
    steps.note("next: put `zmodule cmprmsd/cinelog --on-pull 'zsh shell/fetch.zsh'` in .zimrc and, for the fastest");
    steps.note("      start, `source <module>/shell/early.zsh` as the first line of .zshrc");
    steps.note("search with `cinelog search`; `cinelog status` shows the web UI address");
    Ok(Some(doctor::collect(&ctx)))
}

pub fn run(args: &InstallArgs) -> ExitCode {
    let mut steps = Steps::default();
    let scope = if args.team { "team" } else { "personal" };
    let result = execute(args, &mut steps);
    println!("cinelog install ({scope})");
    print!("{steps}");
    if let Ok(Some(r)) = &result {
        println!();
        print!("{r}");
    }
    // Also after a later failure: a token that was created is never shown again.
    if let Some(f) = steps.finale() {
        println!();
        if f.secret {
            let mut out = std::io::stdout().lock();
            let _ = crate::pause::write_unrecorded(&Ctx::load(), &mut out, &format!("{}\n", f.text));
        } else {
            println!("{}", f.text);
        }
    }
    match result {
        Ok(_) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("cinelog install: {e:#}");
            ExitCode::FAILURE
        }
    }
}
