//! Frozen command-line surface of 1.x (docs/PROTOCOL.md §6, docs/STABILITY.md): later 1.x
//! releases may add subcommands and flags; existing names, flags and outputs do not change.

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
    /// Index (and later serve) this scope's casts; one instance per scope (serve.lock)
    Serve(ServeArgs),
    /// Search commands, and with -a their output; interactive with fzf
    Search(SearchArgs),
    /// Print a command (or session) with its output; the fzf preview of `search`
    Show(ShowArgs),
    /// Remove a command, time range or session from the index (casts are never rewritten)
    Forget(ForgetArgs),
    /// Index the casts once, or rebuild the index from the casts
    Index(IndexArgs),
    /// Indexer lag, backlog, legacy import progress and server state
    Status(StatusArgs),
    /// Compress finished casts older than `retention.compress_after_days` (never legacy casts)
    Compress(CompressArgs),
    /// Legacy import report (aggregate numbers only)
    Import(ImportArgs),
    /// Export or import meta.db (annotations, tags, forget list, settings)
    Meta(MetaArgs),
    /// Manage web login tokens (TEAM: one named token per member)
    Token(TokenArgs),
    /// Show or regenerate the web server's TLS certificate
    Tls(TlsArgs),
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
    /// Only the local version-skew checks: print one line per warning and always exit 0
    /// (the plugin's MOTD line)
    #[arg(long, hide = true, conflicts_with = "json")]
    pub skew: bool,
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
    /// With --team: install a binary that `shell/checksums/` does not list (local builds)
    #[arg(long, requires = "team")]
    pub allow_unverified: bool,
    /// With --team: the cinelog module checkout to install the plugin from (default: the one
    /// the running binary lives in)
    #[arg(long, requires = "team", value_name = "DIR")]
    pub module: Option<PathBuf>,
    /// With --team: the binary to install (default: the module's bin/cinelog, else the
    /// running binary if it is part of the module)
    #[arg(long, requires = "team", value_name = "PATH")]
    pub binary: Option<PathBuf>,
    /// Write every system path below this directory instead of `/` (tests)
    #[arg(long, alias = "root", value_name = "DIR")]
    pub prefix: Option<PathBuf>,
    /// Refresh the ~/.local/bin symlink and restart this user's running server (unit or detached)
    #[arg(long, conflicts_with_all = ["team", "uninstall"])]
    pub refresh: bool,
    /// Remove units, symlinks and the system zshrc block (casts are kept)
    #[arg(long)]
    pub uninstall: bool,
    /// Skip reading the old cinelog-settings.conf
    #[arg(long)]
    pub no_migrate: bool,
}

#[derive(Debug, Args)]
pub struct ServeArgs {
    /// Scope to serve; TEAM needs EUID 0 (default: the scope of the current EUID)
    #[arg(long, value_enum)]
    pub scope: Option<ScopeArg>,
    /// Indexer only: no HTTPS listener
    #[arg(long)]
    pub no_http: bool,
    /// Return at once if a server holds serve.lock; otherwise start one (systemd user unit if
    /// installed, else a detached process) and return
    #[arg(long)]
    pub ensure: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum ScopeArg {
    Personal,
    Team,
}

#[derive(Debug, Args)]
pub struct SearchArgs {
    /// Also search command output
    #[arg(short = 'a', long = "all", conflicts_with = "out")]
    pub all: bool,
    /// Search command output only
    #[arg(long)]
    pub out: bool,
    /// Case-sensitive match (default: Unicode case-insensitive)
    #[arg(short = 's', long)]
    pub case_sensitive: bool,
    #[arg(long, value_name = "USER")]
    pub user: Option<String>,
    #[arg(long, value_name = "HOST")]
    pub host: Option<String>,
    /// Not before: a date/time (`2026-11-01`, `2026-11-01T14:00`) or an age (`2d`, `3h`)
    #[arg(long, value_name = "WHEN")]
    pub since: Option<String>,
    /// Not after: same forms as --since
    #[arg(long, value_name = "WHEN")]
    pub until: Option<String>,
    /// Exit status: a number, `ok` or `fail`
    #[arg(long, value_name = "STATUS")]
    pub exit: Option<String>,
    /// Commands run in this directory (`.` is the current one)
    #[arg(long, value_name = "DIR")]
    pub cwd: Option<String>,
    /// tmux pane id (e.g. `%3`)
    #[arg(long, value_name = "PANE")]
    pub pane: Option<String>,
    /// Print JSON lines instead of running fzf
    #[arg(long)]
    pub json: bool,
    /// Print the plain list instead of running fzf
    #[arg(long, conflicts_with = "json")]
    pub print: bool,
    /// Maximum number of results
    #[arg(long, default_value_t = 1000)]
    pub limit: usize,
    /// Search the team server's history over HTTPS with your team token (`token team-login`)
    #[arg(long)]
    pub team: bool,
    /// Search term (words are joined with single spaces); empty lists the newest commands
    #[arg(value_name = "TERM")]
    pub term: Vec<String>,
}

#[derive(Debug, Args)]
pub struct ShowArgs {
    /// `<uuid>`, `<uuid>:<seq>` or `<uuid>@<t>` (the hidden first field of `search`)
    #[arg(value_name = "ID")]
    pub id: String,
    #[arg(long)]
    pub json: bool,
    /// Maximum output bytes to print
    #[arg(long, default_value_t = 64 * 1024, value_name = "BYTES")]
    pub max_bytes: usize,
    /// Look the entry up on the team server (`cinelog token team-login`); the preview of `search --team`
    #[arg(long, hide = true)]
    pub team: bool,
}

#[derive(Debug, Args)]
pub struct ForgetArgs {
    /// `<uuid>` (whole session) or `<uuid>:<seq>` (one command)
    #[arg(value_name = "ID")]
    pub id: String,
    /// With a session id: forget only output from this cast offset (seconds)
    #[arg(long, value_name = "SECS", requires = "to")]
    pub from: Option<f64>,
    /// With a session id: forget only output up to this cast offset (seconds)
    #[arg(long, value_name = "SECS", requires = "from")]
    pub to: Option<f64>,
}

#[derive(Debug, Args)]
#[command(group = clap::ArgGroup::new("mode").required(true).args(["once", "rebuild"]))]
pub struct IndexArgs {
    /// Index new and changed casts once (only while no writer holds the lock)
    #[arg(long)]
    pub once: bool,
    /// Drop index.db and rebuild it from the casts (meta.db is kept)
    #[arg(long)]
    pub rebuild: bool,
    /// Only these casts (default: everything under the logs root)
    #[arg(value_name = "CAST")]
    pub paths: Vec<PathBuf>,
}

#[derive(Debug, Args)]
pub struct StatusArgs {
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Args)]
#[command(group = clap::ArgGroup::new("mode").required(true).args(["now", "dry_run"]))]
pub struct CompressArgs {
    /// Compress every candidate now
    #[arg(long)]
    pub now: bool,
    /// List the candidates without compressing
    #[arg(long)]
    pub dry_run: bool,
}

#[derive(Debug, Args)]
pub struct ImportArgs {
    /// Per-file banners, log lines, aligned commands and mean confidence
    #[arg(long, required = true)]
    pub report: bool,
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Args)]
pub struct MetaArgs {
    #[command(subcommand)]
    pub action: MetaAction,
}

#[derive(Debug, Subcommand)]
pub enum MetaAction {
    /// Write meta.db as JSON Lines (`-` for stdout; `.zst` compresses)
    Export {
        #[arg(value_name = "FILE")]
        file: PathBuf,
    },
    /// Merge a JSON Lines export into meta.db (`.zst` is decompressed)
    Import {
        #[arg(value_name = "FILE")]
        file: PathBuf,
    },
}

#[derive(Debug, Args)]
pub struct TokenArgs {
    #[command(subcommand)]
    pub action: TokenAction,
}

#[derive(Debug, Subcommand)]
pub enum TokenAction {
    /// Create a token and print it once. TEAM: NAME is the member (required); PERSONAL:
    /// replaces your token (NAME defaults to your user name) and ends its web sessions
    New {
        #[arg(value_name = "NAME")]
        name: Option<String>,
    },
    /// List token names and creation times (never the tokens)
    List {
        #[arg(long)]
        json: bool,
    },
    /// Delete a token and end all web sessions it opened
    Revoke {
        #[arg(value_name = "NAME")]
        name: String,
    },
    /// Store your team token for `search --team` (read from the terminal, or stdin when it
    /// is not a terminal) and pin the team server's certificate fingerprint
    TeamLogin {
        /// Team server URL (default: https://<this host>:10000)
        #[arg(long, value_name = "URL")]
        url: Option<String>,
        /// Expected certificate fingerprint (as `cinelog tls show` prints it on the server);
        /// without it the server's certificate is shown and must be confirmed at the terminal
        /// before the token is sent
        #[arg(long, value_name = "SHA256")]
        fingerprint: Option<String>,
    },
}

#[derive(Debug, Args)]
pub struct TlsArgs {
    #[command(subcommand)]
    pub action: TlsAction,
}

#[derive(Debug, Subcommand)]
pub enum TlsAction {
    /// Print the certificate's fingerprint, names and expiry
    Show {
        #[arg(long)]
        json: bool,
    },
    /// Replace the self-signed certificate (restart the server afterwards)
    Regen,
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
        assert!(Cli::try_parse_from(["cinelog", "install", "--allow-unverified"]).is_err());
        assert!(Cli::try_parse_from(["cinelog", "install", "--binary", "/x"]).is_err());
        assert!(Cli::try_parse_from(["cinelog", "install", "--team", "--binary", "/x"]).is_ok());
        assert!(Cli::try_parse_from(["cinelog", "install", "--refresh", "--team"]).is_err());
    }

    #[test]
    fn m2_subcommands() {
        let ok = |args: &[&str]| Cli::try_parse_from(args).map(|_| ()).map_err(|e| e.to_string());
        let parse = |args: &[&str]| Cli::try_parse_from(args).unwrap().command;
        let Command::Search(s) = parse(&["cinelog", "search", "-a", "--exit", "fail", "foo", "bar"]) else {
            panic!()
        };
        assert!(s.all && s.term == ["foo", "bar"] && s.exit.as_deref() == Some("fail"));
        assert!(ok(&["cinelog", "search", "-a", "--out", "x"]).is_err());
        assert!(ok(&["cinelog", "index"]).is_err());
        assert!(ok(&["cinelog", "index", "--once", "--rebuild"]).is_err());
        assert!(ok(&["cinelog", "index", "--rebuild"]).is_ok());
        assert!(ok(&["cinelog", "compress"]).is_err());
        assert!(ok(&["cinelog", "compress", "--now"]).is_ok());
        assert!(ok(&["cinelog", "import"]).is_err());
        assert!(ok(&["cinelog", "forget", "u", "--from", "1"]).is_err());
        assert!(ok(&["cinelog", "forget", "u", "--from", "1", "--to", "2"]).is_ok());
        assert!(ok(&["cinelog", "serve", "--scope", "team", "--no-http"]).is_ok());
        assert!(ok(&["cinelog", "meta", "export", "-"]).is_ok());
        let Command::Show(s) = parse(&["cinelog", "show", "x:1"]) else {
            panic!()
        };
        assert_eq!(s.max_bytes, 65536);
    }

    #[test]
    fn m3_subcommands() {
        let ok = |args: &[&str]| Cli::try_parse_from(args).map(|_| ()).map_err(|e| e.to_string());
        assert!(ok(&["cinelog", "token", "new"]).is_ok());
        assert!(ok(&["cinelog", "token", "new", "alice"]).is_ok());
        assert!(ok(&["cinelog", "token", "list", "--json"]).is_ok());
        assert!(ok(&["cinelog", "token", "revoke"]).is_err());
        assert!(ok(&["cinelog", "token", "team-login", "--url", "https://t:10000"]).is_ok());
        assert!(ok(&["cinelog", "tls", "show", "--json"]).is_ok());
        assert!(ok(&["cinelog", "tls", "regen"]).is_ok());
        assert!(ok(&["cinelog", "search", "--team", "-a", "x"]).is_ok());
    }

    #[test]
    fn m5_doctor_skew() {
        let Command::Doctor(d) = Cli::try_parse_from(["cinelog", "doctor", "--skew"]).unwrap().command else {
            panic!()
        };
        assert!(d.skew && !d.json);
        assert!(Cli::try_parse_from(["cinelog", "doctor", "--skew", "--json"]).is_err());
    }
}
