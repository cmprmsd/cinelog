//! Frozen M1 command-line surface (see docs/PROTOCOL.md, "Binary interface").
//! Later milestones add subcommands; existing names and flags do not change.

use std::ffi::OsString;
use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(name = "cinelog", version, about = "Record every interactive shell exactly once")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Record a shell in a new PTY (normally exec'd by the zsh plugin)
    Rec(RecArgs),
    /// Print the session id and cast path of the current recording
    Link(LinkArgs),
    /// Pause recording of the current session (leaves a visible pause marker)
    Pause(PauseArgs),
    /// Resume a paused or degraded recording
    Resume,
    /// Diagnose the installation and the current shell
    Doctor(DoctorArgs),
    /// Install or refresh cinelog for this user, or system-wide with --team
    Install(InstallArgs),
    /// Print a shell completion script
    #[command(hide = true)]
    Completions {
        #[arg(value_enum)]
        shell: clap_complete::Shell,
    },
}

#[derive(Debug, Args)]
pub struct RecArgs {
    /// Print the start decision (`start <scope> <reason>`, `recorded <uuid>`, `skip <reason>`) and exit
    #[arg(long, conflicts_with = "protocol")]
    pub check: bool,
    /// Print the supported marker protocol version and exit
    #[arg(long)]
    pub protocol: bool,
    /// Shell and its arguments (after `--`)
    #[arg(last = true, value_name = "SHELL [ARGS]...")]
    pub command: Vec<OsString>,
}

#[derive(Debug, Args)]
pub struct LinkArgs {
    /// Print JSON (`{"uuid","cast","url"}`) instead of text
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Args)]
pub struct PauseArgs {
    /// Free-text reason stored in the pause marker
    #[arg(long)]
    pub reason: Option<String>,
}

#[derive(Debug, Args)]
pub struct DoctorArgs {
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Args)]
pub struct InstallArgs {
    /// System-wide TEAM install (root): binary, root-owned plugin copy, dirs
    #[arg(long)]
    pub team: bool,
    /// With --team: add the guarded block to the system zshrc
    #[arg(long, requires = "team")]
    pub system_zshrc: bool,
    /// With --team: replace an existing team binary after verifying its checksum
    #[arg(long, requires = "team")]
    pub upgrade: bool,
    /// Write every system path below this directory instead of `/` (tests)
    #[arg(long, alias = "root", value_name = "DIR")]
    pub prefix: Option<PathBuf>,
    /// Only refresh the ~/.local/bin symlink and restart the user unit if active
    #[arg(long, conflicts_with_all = ["team", "uninstall"])]
    pub refresh: bool,
    /// Remove units, symlinks and the system zshrc block (casts are kept)
    #[arg(long)]
    pub uninstall: bool,
    /// Skip reading the old cinelog-settings.conf
    #[arg(long)]
    pub no_migrate: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn cli_is_consistent() {
        Cli::command().debug_assert();
    }

    #[test]
    fn rec_takes_shell_after_double_dash() {
        let cli = Cli::try_parse_from(["cinelog", "rec", "--", "/bin/zsh", "-i"]).unwrap();
        let Command::Rec(r) = cli.command else { panic!() };
        assert_eq!(r.command, ["/bin/zsh", "-i"]);
        assert!(Cli::try_parse_from(["cinelog", "install", "--system-zshrc"]).is_err());
        assert!(Cli::try_parse_from(["cinelog", "install", "--team", "--root", "/tmp/x"]).is_ok());
    }
}
