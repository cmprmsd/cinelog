//! `cinelog doctor [--json]` (§10.7, M1 subset): exit 0 when nothing needs attention, 1
//! when at least one check warns or fails.

mod root_sources;
pub mod skew;

use std::fmt;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use cinelog_core::config::HidepidFallback;
use cinelog_core::location;
use cinelog_core::paths::team;
use cinelog_core::registry::{RecState, SystemProbe};
use cinelog_core::server::ServerInfo;
use cinelog_detect::{Detector, Hidepid, ProcFs, RealProcFs, Reason, StartDecision, TtyIdentity};
use cinelog_proto::NestedMode;
use serde::Serialize;

use crate::check::{self, CheckInputs};
use crate::ctx::Ctx;
use crate::install::zshrc;
use crate::link::state_name;
use crate::module;
use crate::session::{self, NotRecorded};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Ok,
    Info,
    Warn,
    Fail,
}

impl Status {
    const fn label(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Info => "info",
            Self::Warn => "warn",
            Self::Fail => "FAIL",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Check {
    pub id: &'static str,
    pub status: Status,
    pub message: String,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Report {
    pub version: &'static str,
    pub checks: Vec<Check>,
}

impl Report {
    fn add(&mut self, id: &'static str, status: Status, message: impl Into<String>) {
        self.checks.push(Check {
            id,
            status,
            message: message.into(),
        });
    }

    pub fn problems(&self) -> usize {
        self.checks.iter().filter(|c| c.status >= Status::Warn).count()
    }
}

impl fmt::Display for Report {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "cinelog {}", self.version)?;
        for c in &self.checks {
            writeln!(f, "  [{:<4}] {:<14} {}", c.status.label(), c.id, c.message)?;
        }
        match self.problems() {
            0 => writeln!(f, "no problems found"),
            n => writeln!(f, "{n} problem(s) found"),
        }
    }
}

pub fn run(json: bool) -> ExitCode {
    let report = collect(&Ctx::load());
    if json {
        println!("{}", serde_json::to_string(&report).expect("report serializes"));
    } else {
        print!("{report}");
    }
    if report.problems() == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

pub fn collect(ctx: &Ctx) -> Report {
    let mut r = Report {
        version: cinelog_core::VERSION,
        checks: Vec::new(),
    };
    let proc = RealProcFs::new();
    let detector = ctx.detector();
    versions(ctx, &mut r);
    config(ctx, &mut r);
    guards(&GuardEnv::capture(), &mut r);
    detection(ctx, &detector, &mut r);
    current_session(ctx, &mut r);
    registry(ctx, &mut r);
    storage(ctx, &mut r);
    server(ctx, &mut r);
    shell_setup(ctx, &mut r);
    if ctx.env.euid == 0 {
        root_hardening(ctx, &mut r);
    }
    old_server(&proc, ctx.env.euid, &mut r);
    hidepid(&proc, ctx.config.record.hidepid_fallback, &mut r);
    stale_env(
        std::env::vars_os().map(|(k, _)| k.to_string_lossy().into_owned()),
        &mut r,
    );
    r
}

fn versions(ctx: &Ctx, r: &mut Report) {
    r.checks.extend(skew::checks(ctx, skew::Reach::Network));
}

fn config(ctx: &Ctx, r: &mut Report) {
    let file = &ctx.paths.config_file;
    match &ctx.config_error {
        Some(e) => r.add("config", Status::Fail, format!("{e} (new shells start unrecorded)")),
        None if file.exists() => r.add("config", Status::Ok, file.display().to_string()),
        None => r.add(
            "config",
            Status::Info,
            format!("{} missing, defaults apply (cinelog install writes it)", file.display()),
        ),
    }
    if let Some(mode) = loose_mode(file) {
        r.add(
            "config-mode",
            Status::Warn,
            format!(
                "{} has mode {mode:o}; use 600 (cinelog install{} fixes it)",
                file.display(),
                if ctx.env.euid == 0 { " --team" } else { "" }
            ),
        );
    }
    if ctx.config_error.is_none() && ctx.config.record.nested == NestedMode::Record {
        r.add("nested", Status::Info, NESTED_RECORD_WARNING);
    }
    if ctx.is_disabled() {
        r.add(
            "kill-switch",
            Status::Warn,
            format!(
                "recording disabled ({} or $CINELOG_DISABLE)",
                ctx.paths.disable_file.display()
            ),
        );
    }
}

/// What `record.nested = "record"` means for the user (§5.3); `install` and `doctor` say it.
pub const NESTED_RECORD_WARNING: &str = "record.nested = \"record\": sudo/root sessions are also \
     recorded into your personal cast, which you can read without root (use \"pause\" to avoid that)";

/// The mode of `file` when it grants group or other bits.
fn loose_mode(file: &Path) -> Option<u32> {
    let mode = std::fs::metadata(file).ok()?.permissions().mode() & 0o777;
    (mode & 0o077 != 0).then_some(mode)
}

/// The values the plugin's start guards look at (§9.1).
#[derive(Debug, Clone, Default)]
pub struct GuardEnv {
    pub stdin_tty: bool,
    /// The plugin's `-t 1`. Doctor's own stdout is often a pipe (`doctor --json | jq`), so
    /// this is judged on the terminal a shell started here would write to.
    pub stdout_tty: bool,
    pub term: Option<String>,
    pub vscode_resolving: bool,
    pub inside_emacs: bool,
}

impl GuardEnv {
    fn capture() -> Self {
        use std::io::IsTerminal;
        let set = |k: &str| std::env::var_os(k).is_some_and(|v| !v.is_empty());
        Self {
            stdin_tty: std::io::stdin().is_terminal(),
            stdout_tty: std::io::stdout().is_terminal() || controlling_terminal_usable(),
            term: std::env::var("TERM").ok(),
            vscode_resolving: set("VSCODE_RESOLVING_ENVIRONMENT"),
            inside_emacs: set("INSIDE_EMACS"),
        }
    }

    /// Guards that would keep a shell in this environment from recording.
    pub fn blockers(&self) -> Vec<&'static str> {
        let mut out = Vec::new();
        if !self.stdin_tty {
            out.push("stdin is not a tty");
        }
        if !self.stdout_tty {
            out.push("stdout is not a tty");
        }
        match self.term.as_deref() {
            Some("dumb") => out.push("TERM=dumb"),
            None | Some("") => out.push("TERM is unset"),
            Some(_) => {}
        }
        if self.vscode_resolving {
            out.push("VSCODE_RESOLVING_ENVIRONMENT is set");
        }
        if self.inside_emacs {
            out.push("INSIDE_EMACS is set");
        }
        out
    }
}

/// Whether this process has a controlling terminal it can write to.
fn controlling_terminal_usable() -> bool {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .write(true)
        .custom_flags(libc::O_NOCTTY)
        .open("/dev/tty")
        .is_ok()
}

fn guards(env: &GuardEnv, r: &mut Report) {
    let b = env.blockers();
    if b.is_empty() {
        r.add("guards", Status::Ok, "an interactive zsh here would start recording");
    } else {
        r.add(
            "guards",
            Status::Warn,
            format!("a shell here would not record: {}", b.join(", ")),
        );
    }
}

fn detection(ctx: &Ctx, detector: &dyn Detector, r: &mut Report) {
    let inputs = CheckInputs::capture();
    let (decision, detail) = check::decide(ctx, detector, &SystemProbe, &inputs);
    let holder = match &inputs.tty {
        Some(TtyIdentity::Pts(n)) => detector.holder(*n, inputs.shell_pid).map(|h| {
            format!(
                "; holder {:?} pid {} uid {} ({:?}, via {:?})",
                h.comm, h.pid, h.uid, h.class, h.via
            )
        }),
        _ => None,
    }
    .unwrap_or_default();
    let status = match &decision {
        StartDecision::Skip {
            reason: Reason::NotATty,
        } => Status::Info,
        StartDecision::Skip { .. } => Status::Warn,
        _ => Status::Ok,
    };
    let detail = detail.map(|d| format!(" ({d})")).unwrap_or_default();
    r.add("detection", status, format!("rec --check: {decision}{holder}{detail}"));
}

fn current_session(ctx: &Ctx, r: &mut Report) {
    match session::current(ctx) {
        Ok(cur) => {
            let e = &cur.entry;
            let status = if e.state == RecState::Degraded {
                Status::Warn
            } else {
                Status::Ok
            };
            let reason = e.state_reason.as_deref().map(|s| format!(", {s}")).unwrap_or_default();
            r.add(
                "session",
                status,
                format!(
                    "recorded: {} {} ({}{reason}), recorder pid {}",
                    e.uuid,
                    e.cast.display(),
                    state_name(e.state),
                    e.pid
                ),
            );
        }
        Err(e @ (NotRecorded::NoTty | NotRecorded::NoEntry(_))) => {
            r.add("session", Status::Info, format!("this shell is not recorded: {e}"));
        }
        Err(e) => r.add("session", Status::Warn, e.to_string()),
    }
}

fn registry(ctx: &Ctx, r: &mut Report) {
    let reg = ctx.registry();
    let entries = match reg.entries() {
        Ok(e) => e,
        Err(e) => return r.add("registry", Status::Warn, e.to_string()),
    };
    let (mut live, mut stale, mut broken) = (0, Vec::new(), Vec::new());
    for (key, entry) in entries {
        match entry {
            Ok(e) if e.is_alive(&SystemProbe) => live += 1,
            Ok(_) => stale.push(key.file_name()),
            Err(_) => broken.push(key.file_name()),
        }
    }
    let dir = reg.dir().display();
    if stale.is_empty() && broken.is_empty() {
        r.add("registry", Status::Ok, format!("{live} live recorder(s) in {dir}"));
    } else {
        r.add(
            "registry",
            Status::Warn,
            format!(
                "{live} live; stale: [{}]; unreadable: [{}] in {dir} (stale files are harmless and can be deleted)",
                stale.join(", "),
                broken.join(", ")
            ),
        );
    }
}

fn storage(ctx: &Ctx, r: &mut Report) {
    let logs = &ctx.paths.logs_root;
    match std::fs::metadata(logs) {
        Err(_) => r.add(
            "logs",
            Status::Warn,
            format!("{} does not exist (cinelog install creates it)", logs.display()),
        ),
        Ok(_) if nix::unistd::access(logs, nix::unistd::AccessFlags::W_OK).is_err() => {
            r.add("logs", Status::Fail, format!("{} is not writable", logs.display()));
        }
        Ok(m) if m.permissions().mode() & 0o077 != 0 => r.add(
            "logs",
            Status::Warn,
            format!(
                "{} has mode {:o}; casts may contain secrets, use 0700 (cinelog install fixes it)",
                logs.display(),
                m.permissions().mode() & 0o777
            ),
        ),
        Ok(_) => {
            let shared = location::synced_ancestor(logs)
                .map(|s| format!("inside the Syncthing folder {}", s.display()))
                .or_else(|| location::network_fs(logs).map(|fs| format!("on a {fs} filesystem")));
            match shared {
                Some(why) => r.add(
                    "logs",
                    Status::Info,
                    format!("{} is {why}: casts are copied off this machine", logs.display()),
                ),
                None => r.add("logs", Status::Ok, logs.display().to_string()),
            }
        }
    }
    match location::db_location_problem(&ctx.paths.db_dir) {
        Some(p) => r.add("db-location", Status::Fail, p),
        None => r.add("db-location", Status::Ok, ctx.paths.db_dir.display().to_string()),
    }
}

fn server(ctx: &Ctx, r: &mut Report) {
    match ServerInfo::read_live(&ctx.paths.runtime_dir) {
        Some(s) => r.add(
            "server",
            Status::Ok,
            format!(
                "{} (pid {}, version {}, cert {})",
                s.url, s.pid, s.version, s.fingerprint
            ),
        ),
        None => match crate::serve::lock::holder_pid(&ctx.paths.runtime_dir) {
            Some(pid) => r.add(
                "server",
                Status::Ok,
                format!("indexer running (pid {pid}; the web UI arrives in a later version)"),
            ),
            None => r.add(
                "server",
                Status::Info,
                "no cinelog server running: recorded shells start it (server.autostart), or run `cinelog serve`",
            ),
        },
    }
}

/// Where zsh reads its startup files for this user.
fn zdotdir(ctx: &Ctx) -> PathBuf {
    if ctx.env.euid == 0 {
        return ctx.env.home.clone();
    }
    std::env::var_os("ZDOTDIR")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| ctx.env.home.clone())
}

/// `Some(true)`: the cinelog `zmodule` line has an `--on-pull` fetch hook.
pub fn zimrc_on_pull(text: &str) -> Option<bool> {
    let line = text
        .lines()
        .map(str::trim)
        .filter(|l| !l.starts_with('#'))
        .find(|l| l.starts_with("zmodule") && l.contains("cinelog"))?;
    Some(line.contains("--on-pull") && line.contains("fetch.zsh"))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EarlyPlacement {
    /// `early.zsh` is sourced before the instant prompt and zim init.
    First,
    /// Sourced at this 1-based line, after `before` (also a line number).
    Late {
        line: usize,
        before: usize,
    },
    Absent,
}

/// Where `.zshrc` sources `shell/early.zsh`, relative to p10k's instant prompt and zim init.
pub fn early_placement(zshrc: &str) -> EarlyPlacement {
    let mut first_heavy = None;
    for (i, raw) in zshrc.lines().enumerate() {
        let l = raw.trim();
        if l.starts_with('#') {
            continue;
        }
        if l.contains("early.zsh") && (l.starts_with("source ") || l.starts_with(". ")) {
            return match first_heavy {
                None => EarlyPlacement::First,
                Some(before) => EarlyPlacement::Late { line: i + 1, before },
            };
        }
        let heavy = l.contains("p10k-instant-prompt") || l.contains("init.zsh") || l.contains("zimfw");
        if heavy && first_heavy.is_none() {
            first_heavy = Some(i + 1);
        }
    }
    EarlyPlacement::Absent
}

fn shell_setup(ctx: &Ctx, r: &mut Report) {
    let zdot = zdotdir(ctx);
    let zimrc = std::env::var_os("ZIM_CONFIG_FILE")
        .filter(|_| ctx.env.euid != 0)
        .map(PathBuf::from)
        .unwrap_or_else(|| zdot.join(".zimrc"));
    match std::fs::read_to_string(&zimrc).ok().as_deref().and_then(zimrc_on_pull) {
        Some(true) => r.add(
            "zim",
            Status::Ok,
            format!("{} fetches the binary on pull", zimrc.display()),
        ),
        Some(false) => r.add(
            "zim",
            Status::Warn,
            format!(
                "{}: add --on-pull 'zsh shell/fetch.zsh' to the cinelog zmodule line, then zimfw update",
                zimrc.display()
            ),
        ),
        None => r.add(
            "zim",
            Status::Info,
            format!("no cinelog zmodule line in {}", zimrc.display()),
        ),
    }
    let zshrc = zdot.join(".zshrc");
    match early_placement(&std::fs::read_to_string(&zshrc).unwrap_or_default()) {
        EarlyPlacement::First => r.add("early-zsh", Status::Ok, "shell/early.zsh is sourced first"),
        EarlyPlacement::Late { line, before } => r.add(
            "early-zsh",
            Status::Warn,
            format!(
                "{} sources early.zsh at line {line}, after line {before} (instant prompt/zim): move it to the top",
                zshrc.display()
            ),
        ),
        EarlyPlacement::Absent => r.add(
            "early-zsh",
            Status::Info,
            "early.zsh not used: works, but sourcing <module>/shell/early.zsh at the top of .zshrc starts faster",
        ),
    }
}

fn root_hardening(ctx: &Ctx, r: &mut Report) {
    let rc = zshrc::system_zshrc(&ctx.root);
    let has_block = std::fs::read_to_string(&rc).is_ok_and(|t| zshrc::has_block(&t));
    if has_block {
        r.add(
            "root-plugin",
            Status::Ok,
            format!("{} loads the root-owned plugin copy", rc.display()),
        );
    } else {
        r.add(
            "root-plugin",
            Status::Warn,
            format!(
                "{} has no cinelog block: root shells depend on user dotfiles (cinelog install --team --system-zshrc)",
                rc.display()
            ),
        );
    }
    // zsh reads startup files from $HOME even where cinelog ignores it.
    let home = std::env::var_os("HOME").map(PathBuf::from);
    if let Some(home) = home.filter(|h| *h != ctx.env.home) {
        let owner = std::fs::metadata(home.join(".zshrc")).map(|m| m.uid()).ok();
        let who = owner.map(|u| format!(" (.zshrc owned by uid {u})")).unwrap_or_default();
        r.add(
            "root-dotfiles",
            Status::Warn,
            format!(
                "HOME={} is not root's home{who}: root runs another user's dotfiles; use sudo -i or Defaults always_set_home",
                home.display()
            ),
        );
    }
    let homes = root_sources::Homes::of(&ctx.env.home);
    let found = root_sources::untrusted(&homes, &rc, 0, Path::new("/"));
    match found.first() {
        None => r.add(
            "root-sources",
            Status::Ok,
            format!(
                "everything {} and the system zshrc source is root-owned",
                ctx.env.home.display()
            ),
        ),
        Some(first) => {
            let via = first
                .via
                .as_ref()
                .map(|v| format!(" (sourced by {})", v.display()))
                .unwrap_or_default();
            let more = match found.len() {
                1 => String::new(),
                n => format!(", and {} more", n - 1),
            };
            r.add(
                "root-sources",
                Status::Warn,
                format!(
                    "root's shell runs code from {}{via}: {}{more}; whoever can write there gets root",
                    first.bad.path.display(),
                    first.bad.problem
                ),
            );
        }
    }
    if let Ok(exe) = module::current_exe()
        && let Some(bad) = location::insecure_component(&exe, 0, Path::new("/"))
    {
        r.add(
            "root-binary",
            Status::Warn,
            format!(
                "root runs {} but {} is {}; root shells only use {}",
                exe.display(),
                bad.path.display(),
                bad.problem,
                team::BINARY
            ),
        );
    }
}

/// Pids of the pre-rewrite Python viewer (`python3 …/cinelog/http_server.py`).
pub fn old_server_pids(proc: &dyn ProcFs, euid: u32) -> Vec<u32> {
    let Ok(pids) = proc.pids() else { return Vec::new() };
    pids.into_iter()
        .filter(|&pid| euid == 0 || proc.uids(pid).is_ok_and(|u| u.real == euid))
        .filter(|&pid| {
            proc.cmdline(pid).is_ok_and(|argv| {
                argv.first().is_some_and(|a| a.contains("python"))
                    && argv.iter().any(|a| a.ends_with("cinelog/http_server.py"))
            })
        })
        .collect()
}

fn old_server(proc: &dyn ProcFs, euid: u32, r: &mut Report) {
    let pids = old_server_pids(proc, euid);
    if !pids.is_empty() {
        let list: Vec<String> = pids.iter().map(u32::to_string).collect();
        r.add(
            "old-server",
            Status::Warn,
            format!(
                "the old Python viewer still runs (pid {}); it serves casts over plain HTTP: kill it",
                list.join(", ")
            ),
        );
    }
}

fn hidepid(proc: &dyn ProcFs, fallback: HidepidFallback, r: &mut Report) {
    let what = match fallback {
        HidepidFallback::Start => "shells with an invisible holder start recording",
        HidepidFallback::Skip => "shells with an invisible holder are not recorded",
    };
    match proc.hidepid() {
        Ok(Hidepid::Off) => r.add("hidepid", Status::Ok, "/proc is fully visible"),
        Ok(h) => r.add("hidepid", Status::Info, format!("/proc is mounted with {h:?}: {what}")),
        Err(e) => r.add(
            "hidepid",
            Status::Info,
            format!("cannot read the /proc mount options: {e}"),
        ),
    }
}

/// Variables that must never be inherited (R1): the old plugin's exports and the
/// recorder's child env, which the plugin unsets immediately.
pub fn stale_vars(names: impl Iterator<Item = String>) -> Vec<String> {
    let mut out: Vec<String> = names
        .filter(|k| {
            (k.starts_with("CINELOG_") && k != cinelog_proto::env::DISABLE)
                || matches!(k.as_str(), "LOGFILE" | "COMMANDS_LOGFILE" | "ASCIINEMA_REC")
        })
        .collect();
    out.sort();
    out
}

fn stale_env(names: impl Iterator<Item = String>, r: &mut Report) {
    let vars = stale_vars(names);
    if !vars.is_empty() {
        r.add(
            "environment",
            Status::Warn,
            format!(
                "inherited {}: an old plugin or tmux server is exporting them (restart tmux after upgrading)",
                vars.join(", ")
            ),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guard_blockers() {
        let ok = GuardEnv {
            stdin_tty: true,
            stdout_tty: true,
            term: Some("xterm-256color".into()),
            ..GuardEnv::default()
        };
        assert!(ok.blockers().is_empty());
        let bad = GuardEnv {
            term: Some("dumb".into()),
            vscode_resolving: true,
            ..ok.clone()
        };
        assert_eq!(bad.blockers(), ["TERM=dumb", "VSCODE_RESOLVING_ENVIRONMENT is set"]);
        assert_eq!(GuardEnv::default().blockers().len(), 3);
    }

    #[test]
    fn zimrc_detection() {
        assert_eq!(zimrc_on_pull("zmodule git\n"), None);
        assert_eq!(zimrc_on_pull("# zmodule cmprmsd/cinelog\n"), None);
        assert_eq!(zimrc_on_pull("zmodule cmprmsd/cinelog\n"), Some(false));
        assert_eq!(
            zimrc_on_pull("zmodule cmprmsd/cinelog --on-pull 'zsh shell/fetch.zsh'\n"),
            Some(true)
        );
    }

    #[test]
    fn early_zsh_placement() {
        assert_eq!(early_placement(""), EarlyPlacement::Absent);
        assert_eq!(
            early_placement("# hi\nsource ~/.zim/modules/cinelog/shell/early.zsh\nsource ${ZIM_HOME}/init.zsh\n"),
            EarlyPlacement::First
        );
        let late = "if [[ -r \"${XDG_CACHE_HOME}/p10k-instant-prompt-${(%):-%n}.zsh\" ]]; then\nfi\n\
                    source ${ZIM_HOME}/init.zsh\n. ~/cinelog/shell/early.zsh\n";
        assert_eq!(early_placement(late), EarlyPlacement::Late { line: 4, before: 1 });
    }

    #[test]
    fn loose_config_modes() {
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("config.toml");
        std::fs::write(&f, "").unwrap();
        std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(loose_mode(&f), Some(0o644));
        std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(loose_mode(&f), None);
        assert_eq!(loose_mode(&d.path().join("missing")), None);
    }

    #[test]
    fn stale_variables() {
        let names = ["PATH", "CINELOG_TTY", "LOGFILE", "CINELOG_DISABLE", "ASCIINEMA_REC"].map(String::from);
        assert_eq!(
            stale_vars(names.into_iter()),
            ["ASCIINEMA_REC", "CINELOG_TTY", "LOGFILE"]
        );
    }

    #[test]
    fn report_counts_problems() {
        let mut r = Report::default();
        r.add("a", Status::Ok, "x");
        r.add("b", Status::Info, "y");
        assert_eq!(r.problems(), 0);
        r.add("c", Status::Warn, "z");
        r.add("d", Status::Fail, "w");
        assert_eq!(r.problems(), 2);
        let text = r.to_string();
        assert!(text.contains("[FAIL] d"), "{text}");
        assert!(text.ends_with("2 problem(s) found\n"));
        let v = serde_json::to_value(&r).unwrap();
        assert_eq!(v["checks"][2]["status"], "warn");
    }

    #[test]
    fn old_server_on_this_machine_is_only_read() {
        // Smoke test against the real /proc: must not fail, whatever runs here.
        let _ = old_server_pids(&RealProcFs::new(), nix::unistd::geteuid().as_raw());
    }
}
