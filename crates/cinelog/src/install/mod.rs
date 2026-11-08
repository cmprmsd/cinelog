//! `cinelog install` (§10.6): PERSONAL by default, TEAM with `--team`.

pub mod fsutil;
pub mod migrate;
pub mod personal;
pub mod team;
pub mod zshrc;

use std::fmt;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, Result};
use cinelog_core::paths::{EnvSnapshot, SystemRoot};
use fsutil::{Outcome, pretty};

use crate::cli::InstallArgs;
use crate::{doctor, module};

/// What an install did, in order, for the summary.
#[derive(Debug, Default)]
pub struct Steps {
    lines: Vec<String>,
}

impl Steps {
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

pub fn user_unit_file(env: &EnvSnapshot) -> PathBuf {
    env.config_home().join("systemd/user/cinelog.service")
}

/// Runs `systemctl` for an installed unit; failures are reported, never fatal.
pub fn systemctl(args: &[&str], steps: &mut Steps) {
    let line = format!("systemctl {}", args.join(" "));
    match std::process::Command::new("systemctl").args(args).status() {
        Ok(s) if s.success() => steps.done(line),
        Ok(s) => steps.warn(format!("{line} failed ({s})")),
        Err(e) => steps.warn(format!("{line}: {e}")),
    }
}

fn team_options(args: &InstallArgs) -> Result<team::Options> {
    let root = args
        .prefix
        .as_ref()
        .map_or_else(SystemRoot::real, |p| SystemRoot::prefixed(p.clone()));
    let euid = nix::unistd::geteuid().as_raw();
    team::check_privileges(&root, euid)?;
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
        trust: team::Trust::for_root(&root),
        root,
        module,
        binary,
        system_zshrc: args.system_zshrc,
        upgrade: args.upgrade,
        allow_unverified: args.allow_unverified,
        migrate: !args.no_migrate,
        euid,
    })
}

fn execute(args: &InstallArgs, steps: &mut Steps) -> Result<Option<doctor::Report>> {
    if args.team {
        let opts = team_options(args)?;
        if args.uninstall {
            team::uninstall(&opts, steps)?;
        } else {
            team::install(&opts, steps)?;
        }
        return Ok(None);
    }
    let env = EnvSnapshot::capture();
    if args.prefix.is_some() {
        steps.note("--prefix only applies to --team; the personal install writes no system paths");
    }
    if args.refresh {
        personal::refresh(env, steps)?;
        return Ok(None);
    }
    if args.uninstall {
        personal::uninstall(env, steps)?;
        return Ok(None);
    }
    let opts = personal::Options {
        migrate: !args.no_migrate,
        exe: None,
    };
    let ctx = personal::install(env, &opts, steps)?;
    steps.note("next: put `zmodule cmprmsd/cinelog --on-pull 'zsh shell/fetch.zsh'` in .zimrc and, for the fastest");
    steps.note("      start, `source <module>/shell/early.zsh` as the first line of .zshrc");
    steps.note("the search index and the web UI are not part of this version yet");
    Ok(Some(doctor::collect(&ctx)))
}

pub fn run(args: &InstallArgs) -> ExitCode {
    let mut steps = Steps::default();
    let scope = if args.team { "team" } else { "personal" };
    let result = execute(args, &mut steps);
    println!("cinelog install ({scope})");
    print!("{steps}");
    match result {
        Ok(report) => {
            if let Some(r) = report {
                println!();
                print!("{r}");
            }
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("cinelog install: {e:#}");
            ExitCode::FAILURE
        }
    }
}
