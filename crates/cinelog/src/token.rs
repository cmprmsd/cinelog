//! `cinelog token new|list|revoke|team-login`, the personal token of `install`, the first
//! member token of `install --team`, and the pinned HTTPS client of `search --team`
//! (work package M3-D; plan §7.2, §7.7; docs/API.md §4 and §8).
//!
//! Token files are [`TokenFile`]s at `ctx.paths.auth_file` (PERSONAL `auth.toml` with one
//! token named after the user, TEAM `/etc/cinelog/tokens.toml` with one token per member,
//! EUID 0 only). Every read-modify-write holds an flock on a sidecar lock file, because the
//! file itself is replaced by rename. Revoking or replacing a token also deletes the web
//! sessions of its principal in meta.db (`cinelog_web::auth::revoke_principal`).
//!
//! `team-login` stores `{url, token, fingerprint}` in `$XDG_CONFIG_HOME/cinelog/team-token`
//! (0600). The token is only ever sent to a pinned certificate: the one given with
//! `--fingerprint`, or the one the member confirmed at the terminal after a token-less probe
//! (`GET /api/login`). [`TeamClient`] talks HTTP/1.1 over rustls to that URL and accepts
//! exactly the pinned certificate (the team certificate is self-signed, so the pin replaces
//! CA and name checks). Its session cookie is cached in the runtime dir, so the fzf preview,
//! which runs one process per selection, does not hit the login rate limit.

use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, IsTerminal, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context as _, Result, anyhow, bail};
use cinelog_core::{Scope, location};
use cinelog_index::model::DbPaths;
use cinelog_web::api::types as wire;
use cinelog_web::tokens::{self, NewToken, TokenFile};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::cli::{TokenAction, TokenArgs};
use crate::ctx::Ctx;
use crate::install::fsutil;

/// Exit status of `revoke` for a name that has no token.
const NOT_FOUND: u8 = 1;

pub fn run(args: &TokenArgs) -> ExitCode {
    let ctx = Ctx::load();
    let mut out = io::stdout().lock();
    let result = match &args.action {
        TokenAction::TeamLogin { url, fingerprint } => {
            team_login(&ctx, url.as_deref(), fingerprint.as_deref(), &mut Terminal, &mut out)
        }
        action => manage(&ctx, action, cinelog_web::now(), &mut out),
    };
    let _ = out.flush();
    match result {
        Ok(code) => ExitCode::from(code),
        Err(e) => {
            eprintln!("cinelog: token: {e:#}");
            ExitCode::from(2)
        }
    }
}

/// Creates the PERSONAL token when `auth.toml` has none; `None` when one exists (or in
/// TEAM scope, whose tokens are per member).
pub fn ensure_personal(ctx: &Ctx) -> Result<Option<NewToken>> {
    if ctx.scope() != Scope::Personal {
        return Ok(None);
    }
    prepare_dir(ctx)?;
    let name = personal_name(&ctx.env.user);
    let now = cinelog_web::now();
    update(&ctx.paths.auth_file, |tf| {
        Ok(if tf.tokens.is_empty() {
            Some(tf.add(&name, now)?)
        } else {
            None
        })
    })
}

/// Adds a token named `name` to `file` unless one exists (`install --team` for
/// `$SUDO_USER`). The directory must exist.
pub fn ensure_named(file: &Path, name: &str, now: f64) -> Result<Option<NewToken>> {
    update(file, |tf| {
        Ok(if tf.get(name).is_some() {
            None
        } else {
            Some(tf.add(name, now)?)
        })
    })
}

/// `new`, `list` and `revoke` on this scope's token file.
fn manage(ctx: &Ctx, action: &TokenAction, now: f64, out: &mut dyn Write) -> Result<u8> {
    let file = &ctx.paths.auth_file;
    let team = ctx.scope() == Scope::Team;
    match action {
        TokenAction::List { json } => {
            check_dir(ctx)?;
            list(&TokenFile::load(file)?, *json, out)?;
            Ok(0)
        }
        TokenAction::New { name } => {
            prepare_dir(ctx)?;
            let (token, replaced) = if team {
                let name = name
                    .as_deref()
                    .context("team tokens are named after the member: cinelog token new NAME")?;
                (update(file, |tf| Ok(tf.add(name, now)?))?, Vec::new())
            } else {
                let name = name.clone().unwrap_or_else(|| personal_name(&ctx.env.user));
                update(file, |tf| {
                    let old: Vec<String> = tf.tokens.drain(..).map(|t| t.name).collect();
                    Ok((tf.add(&name, now)?, old))
                })?
            };
            writeln!(out, "token for {} (shown once): {}", token.name, token.secret)?;
            for principal in replaced {
                let n = end_sessions(ctx, &principal)
                    .with_context(|| format!("the old token is gone, but its web sessions of {principal} remain"))?;
                if n > 0 {
                    writeln!(out, "ended {n} web session(s) of the old token")?;
                }
            }
            if team {
                writeln!(
                    out,
                    "{} logs in with it on the team web UI, or stores it with `cinelog token team-login` for `cinelog search --team`",
                    token.name
                )?;
            }
            Ok(0)
        }
        TokenAction::Revoke { name } => {
            check_dir(ctx)?;
            let removed = update(file, |tf| Ok(tf.remove(name)))?;
            // Also after an earlier revoke whose session cleanup failed.
            let ended = end_sessions_settled(ctx, name).with_context(|| {
                format!("could not end the web sessions of {name}; run `cinelog token revoke {name}` again")
            })?;
            if removed {
                writeln!(out, "revoked {name}; ended {ended} web session(s)")?;
                Ok(0)
            } else {
                writeln!(out, "no token named {name}; ended {ended} web session(s)")?;
                Ok(NOT_FOUND)
            }
        }
        TokenAction::TeamLogin { url, fingerprint } => {
            team_login(ctx, url.as_deref(), fingerprint.as_deref(), &mut Terminal, out)
        }
    }
}

fn list(tf: &TokenFile, json: bool, out: &mut dyn Write) -> io::Result<()> {
    if json {
        #[derive(Serialize)]
        struct Row<'a> {
            name: &'a str,
            created_at: f64,
        }
        let rows: Vec<Row<'_>> = tf
            .tokens
            .iter()
            .map(|t| Row {
                name: &t.name,
                created_at: t.created_at,
            })
            .collect();
        return writeln!(out, "{}", serde_json::to_string(&rows).map_err(io::Error::other)?);
    }
    let width = tf.tokens.iter().map(|t| t.name.len()).max().unwrap_or(0);
    for t in &tf.tokens {
        writeln!(out, "{:<width$}  created {}", t.name, fmt_time(t.created_at))?;
    }
    Ok(())
}

fn fmt_time(ts: f64) -> String {
    jiff::Timestamp::from_microsecond((ts * 1e6) as i64)
        .map(|t| {
            t.to_zoned(jiff::tz::TimeZone::system())
                .strftime("%Y-%m-%d %H:%M")
                .to_string()
        })
        .unwrap_or_else(|_| ts.to_string())
}

/// A valid token name for a user name (`[A-Za-z0-9._-]`, other characters become `_`).
pub fn personal_name(user: &str) -> String {
    let mut name: String = user
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .take(64)
        .collect();
    while name.starts_with(['.', '-']) {
        name.remove(0);
    }
    if name.is_empty() { "user".into() } else { name }
}

/// The token file's directory must exist (TEAM: created by `install --team`) and, for the
/// real TEAM file, be root-owned all the way up.
fn check_dir(ctx: &Ctx) -> Result<()> {
    let dir = ctx.paths.auth_file.parent().unwrap_or(Path::new("/"));
    if ctx.scope() == Scope::Team {
        if !dir.is_dir() {
            bail!(
                "{} does not exist: run `sudo cinelog install --team` first",
                dir.display()
            );
        }
        if ctx.root.is_real()
            && let Some(bad) = location::insecure_component(dir, 0, Path::new("/"))
        {
            bail!(
                "refusing to use {}: {}: {}",
                dir.display(),
                bad.path.display(),
                bad.problem
            );
        }
    }
    Ok(())
}

/// [`check_dir`], creating the PERSONAL config dir (0700) when missing.
fn prepare_dir(ctx: &Ctx) -> Result<()> {
    if ctx.scope() == Scope::Personal
        && let Some(dir) = ctx.paths.auth_file.parent()
    {
        fsutil::ensure_dir(dir, 0o700)?;
    }
    check_dir(ctx)
}

/// Loads `file` under its lock, applies `change` and saves (0600) when the list changed.
/// Nothing is written when `change` fails.
fn update<T>(file: &Path, change: impl FnOnce(&mut TokenFile) -> Result<T>) -> Result<T> {
    let _lock = lock(file)?;
    let before = TokenFile::load(file)?;
    let mut after = before.clone();
    let value = change(&mut after)?;
    if after != before {
        after.save(file)?;
    }
    Ok(value)
}

/// Exclusive flock on `.<name>.lock` next to `file`, held until the guard drops.
fn lock(file: &Path) -> Result<nix::fcntl::Flock<File>> {
    let dir = file.parent().unwrap_or(Path::new("."));
    let name = file.file_name().map(|n| n.to_string_lossy()).unwrap_or_default();
    let path = dir.join(format!(".{name}.lock"));
    let f = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&path)
        .with_context(|| format!("opening {}", path.display()))?;
    nix::fcntl::Flock::lock(f, nix::fcntl::FlockArg::LockExclusive)
        .map_err(|(_, e)| anyhow!("locking {}: {e}", path.display()))
}

/// Deletes the web sessions of `principal`; 0 while there is no meta.db yet.
fn end_sessions(ctx: &Ctx, principal: &str) -> Result<u64> {
    let meta = DbPaths::in_dir(&ctx.paths.db_dir).meta;
    if !meta.exists() {
        return Ok(0);
    }
    cinelog_web::auth::revoke_principal(&meta, principal).map_err(|e| anyhow!("{e}"))
}

/// How long a login that read the token file before the revoke may still take to store its
/// session (the server reads the file, then inserts).
const REVOKE_SETTLE: Duration = Duration::from_millis(1500);

/// [`end_sessions`], and once more after [`REVOKE_SETTLE`] for a login that was already
/// past its token check when the file was rewritten.
fn end_sessions_settled(ctx: &Ctx, principal: &str) -> Result<u64> {
    let first = end_sessions(ctx, principal)?;
    if !DbPaths::in_dir(&ctx.paths.db_dir).meta.exists() {
        return Ok(first);
    }
    std::thread::sleep(REVOKE_SETTLE);
    Ok(first + end_sessions(ctx, principal)?)
}

// ------------------------------------------------------------------ team-login

/// `$XDG_CONFIG_HOME/cinelog/team-token`: what `search --team` connects with.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TeamCredentials {
    /// Base URL, `https://host:port`.
    pub url: String,
    pub token: String,
    /// Pinned certificate fingerprint ([`cinelog_web::tls::fingerprint`] format).
    pub fingerprint: String,
}

impl TeamCredentials {
    pub fn path(ctx: &Ctx) -> PathBuf {
        ctx.env.config_home().join("cinelog/team-token")
    }

    /// Refuses a file that others can read: it holds a credential.
    pub fn load(path: &Path) -> Result<Self> {
        let text = match fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                bail!("no team token yet: run `cinelog token team-login` first")
            }
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };
        let mode = fs::metadata(path)?.permissions().mode();
        if mode & 0o077 != 0 {
            bail!(
                "{} is accessible by other users (mode {:o}): chmod 600 it",
                path.display(),
                mode & 0o777
            );
        }
        toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(dir) = path.parent() {
            fsutil::ensure_dir(dir, 0o700)?;
        }
        let body = format!(
            "# cinelog team token for `cinelog search --team` (written by `cinelog token team-login`)\n{}",
            toml::to_string(self)?
        );
        write_private(path, body.as_bytes())
    }
}

/// Atomic write with mode 0600.
fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    let dir = path.parent().unwrap_or(Path::new("."));
    let name = path.file_name().map(|n| n.to_string_lossy()).unwrap_or_default();
    let tmp = dir.join(format!(".{name}.{}.tmp", std::process::id()));
    let mut f = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&tmp)
        .with_context(|| format!("writing {}", tmp.display()))?;
    f.write_all(bytes)?;
    f.sync_all()?;
    drop(f);
    fs::rename(&tmp, path).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

/// What `team-login` asks the member at the terminal.
pub trait LoginPrompt {
    /// Whether someone can answer a question (stdin is a terminal).
    fn interactive(&self) -> bool;
    /// Shows `message` and asks for yes or no; anything but yes is no.
    fn confirm(&mut self, message: &str) -> Result<bool>;
    /// The token: from the terminal without echo, or the first line of stdin.
    fn token(&mut self, url: &BaseUrl) -> Result<String>;
}

/// The controlling terminal (`/dev/tty`) and stdin.
pub struct Terminal;

impl LoginPrompt for Terminal {
    fn interactive(&self) -> bool {
        io::stdin().is_terminal()
    }

    fn confirm(&mut self, message: &str) -> Result<bool> {
        let tty = OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/tty")
            .context("opening the terminal")?;
        (&tty).write_all(message.as_bytes())?;
        let mut answer = String::new();
        BufReader::new(&tty).read_line(&mut answer)?;
        Ok(matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes"))
    }

    fn token(&mut self, url: &BaseUrl) -> Result<String> {
        if !io::stdin().is_terminal() {
            let mut line = String::new();
            io::stdin().lock().read_line(&mut line)?;
            return Ok(line);
        }
        let tty = OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/tty")
            .context("opening the terminal")?;
        (&tty).write_all(format!("team token for {url}: ").as_bytes())?;
        let line = {
            let _quiet = NoEcho::new(&tty)?;
            let mut line = String::new();
            BufReader::new(&tty).read_line(&mut line).map(|_| line)
        };
        (&tty).write_all(b"\n")?;
        Ok(line?)
    }
}

/// Echo off on a terminal until dropped. A SIGINT, SIGTERM, SIGHUP or SIGQUIT meanwhile
/// restores the saved mode before the signal's default action ends the process.
struct NoEcho {
    fd: std::os::fd::RawFd,
    saved: nix::sys::termios::Termios,
    handlers: Vec<signal_hook::SigId>,
}

impl NoEcho {
    const SIGNALS: [libc::c_int; 4] = [libc::SIGINT, libc::SIGTERM, libc::SIGHUP, libc::SIGQUIT];

    fn new(tty: &File) -> Result<Self> {
        use nix::sys::termios::{self, LocalFlags, SetArg};
        use std::os::fd::AsRawFd;
        let fd = tty.as_raw_fd();
        let saved = termios::tcgetattr(tty)?;
        let raw: libc::termios = saved.clone().into();
        let mut guard = Self {
            fd,
            saved: saved.clone(),
            handlers: Vec::new(),
        };
        for sig in Self::SIGNALS {
            // SAFETY: the action only calls async-signal-safe functions (tcsetattr, and the
            // signal-hook default emulation, which re-raises with the default disposition).
            let id = unsafe {
                signal_hook::low_level::register(sig, move || {
                    libc::tcsetattr(fd, libc::TCSAFLUSH, &raw);
                    let _ = signal_hook::low_level::emulate_default_handler(sig);
                })
            }?;
            guard.handlers.push(id);
        }
        let mut quiet = saved;
        quiet.local_flags.remove(LocalFlags::ECHO);
        termios::tcsetattr(tty, SetArg::TCSAFLUSH, &quiet)?;
        Ok(guard)
    }
}

impl Drop for NoEcho {
    fn drop(&mut self) {
        // SAFETY: `fd` belongs to the terminal the guard borrows for its whole life.
        let tty = unsafe { std::os::fd::BorrowedFd::borrow_raw(self.fd) };
        let _ = nix::sys::termios::tcsetattr(tty, nix::sys::termios::SetArg::TCSAFLUSH, &self.saved);
        for id in self.handlers.drain(..) {
            signal_hook::low_level::unregister(id);
        }
    }
}

fn team_login(
    ctx: &Ctx,
    url: Option<&str>,
    fingerprint: Option<&str>,
    prompt: &mut dyn LoginPrompt,
    out: &mut dyn Write,
) -> Result<u8> {
    if ctx.scope() == Scope::Team {
        bail!("as root, `cinelog search` reads the team database directly; team-login is for member shells");
    }
    let url = match url {
        Some(u) => BaseUrl::parse(u)?,
        None => BaseUrl::parse(&format!(
            "https://{}:{}",
            cinelog_core::paths::hostname(),
            cinelog_core::server::BASE_PORT
        ))?,
    };
    let given = fingerprint.map(normalize_fingerprint).transpose()?;
    let confirmed = given.is_none();
    let pin = match given {
        Some(pin) => pin,
        None => confirm_certificate(&url, prompt)?,
    };
    let token = prompt.token(&url)?;
    let path = TeamCredentials::path(ctx);
    let (creds, me) = login_check(url, pin, &token)?;
    creds.save(&path)?;
    let _ = fs::remove_file(session_cache(ctx));
    writeln!(
        out,
        "logged in to {} as {} (cinelog {})",
        creds.url, me.principal, me.version
    )?;
    if confirmed {
        writeln!(out, "pinned the certificate you confirmed: {}", creds.fingerprint)?;
    } else {
        writeln!(out, "the certificate matches the given fingerprint")?;
    }
    writeln!(out, "stored in {}; `cinelog search --team` uses it", path.display())?;
    Ok(0)
}

/// Without `--fingerprint`: fetches the certificate with a token-less request and has the
/// member confirm it, before the token goes anywhere. Refused without a terminal.
fn confirm_certificate(url: &BaseUrl, prompt: &mut dyn LoginPrompt) -> Result<String> {
    if !prompt.interactive() {
        bail!(
            "without a terminal to confirm the team server's certificate, pass --fingerprint \
             (`sudo cinelog install --team` prints it, as does `sudo cinelog tls show` on the team server)"
        );
    }
    let (fingerprint, info) = probe(url)?;
    let message = format!(
        "The team server {url} (cinelog {}) presents the certificate\n  {fingerprint}\n\
         Compare it with what `sudo cinelog install --team` printed or `sudo cinelog tls show` prints \
         on the team server.\nIs it the same? [y/N] ",
        info.version
    );
    if !prompt.confirm(&message)? {
        bail!("not logged in: the certificate was not confirmed, and the token was not sent");
    }
    Ok(fingerprint)
}

/// `GET /api/login` without credentials: the certificate the server presents, and what it
/// says it is.
pub fn probe(url: &BaseUrl) -> Result<(String, wire::LoginInfo)> {
    let client = TeamClient::new(url.clone(), None, String::new(), None)?;
    let resp = client.send("GET", "/api/login", None, None)?;
    let fingerprint = client.fingerprint().context("no certificate seen")?;
    if resp.status != 200 {
        return Err(resp.error(url)).context("is this a cinelog server?");
    }
    let info: wire::LoginInfo = serde_json::from_slice(&resp.body)
        .with_context(|| format!("{url} is not a cinelog server (unexpected /api/login answer)"))?;
    if info.scope != wire::Scope::Team {
        bail!("{url} is a personal cinelog server, not the team server");
    }
    Ok((fingerprint, info))
}

/// Logs in once with `token`, sent only to the certificate `pin`, and returns what to store
/// plus who the server says we are.
pub fn login_check(url: BaseUrl, pin: String, token: &str) -> Result<(TeamCredentials, wire::Me)> {
    let token = token.trim().to_owned();
    if tokens::hash(&token).is_none() {
        bail!("that is not a cinelog token (52 base32 characters, as `cinelog token new` prints it)");
    }
    let client = TeamClient::new(url, Some(pin), token.clone(), None)?;
    let me: wire::Me = client
        .get_json("/api/me")?
        .context("the server has no /api/me: is this a cinelog server?")?;
    if me.scope != wire::Scope::Team {
        bail!("{} is a personal cinelog server, not the team server", client.url);
    }
    let fingerprint = client.fingerprint().context("no certificate seen")?;
    Ok((
        TeamCredentials {
            url: client.url.to_string(),
            token,
            fingerprint,
        },
        me,
    ))
}

/// `AA:BB:…` (64 hex digits, any case, with or without `:` and a `sha256` prefix) in the
/// canonical uppercase colon form.
pub fn normalize_fingerprint(s: &str) -> Result<String> {
    let s = s.trim();
    let s = s
        .split_once('=')
        .map_or(s, |(_, v)| v)
        .trim_start_matches("sha256:")
        .trim_start_matches("SHA256:");
    let hex: String = s.chars().filter(|c| *c != ':' && !c.is_whitespace()).collect();
    if hex.len() != 64 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        bail!("fingerprint {s:?}: expected 64 hex digits (as `cinelog tls show` prints them)");
    }
    let upper = hex.to_ascii_uppercase();
    Ok(upper
        .as_bytes()
        .chunks(2)
        .map(|p| std::str::from_utf8(p).expect("ascii"))
        .collect::<Vec<_>>()
        .join(":"))
}

// ------------------------------------------------------------------ HTTPS client

/// `https://host[:port]` without a path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BaseUrl {
    /// Host name or IP literal, without brackets.
    pub host: String,
    pub port: u16,
}

impl BaseUrl {
    pub fn parse(s: &str) -> Result<Self> {
        let bad = |why: &str| anyhow!("URL {s:?}: {why}");
        let rest = s
            .trim()
            .strip_prefix("https://")
            .or_else(|| s.trim().strip_prefix("HTTPS://"))
            .ok_or_else(|| bad("must start with https://"))?;
        let authority = rest.trim_end_matches('/');
        if authority.contains(['/', '?', '#', '@']) || authority.is_empty() {
            bail!(bad("give only https://host:port"));
        }
        let (host, port) = if let Some(v6) = authority.strip_prefix('[') {
            let (h, after) = v6.split_once(']').ok_or_else(|| bad("unclosed ["))?;
            if h.parse::<std::net::Ipv6Addr>().is_err() {
                bail!(bad("bad IPv6 address"));
            }
            match after {
                "" => (h, None),
                p => (
                    h,
                    Some(p.strip_prefix(':').ok_or_else(|| bad("expected :port after ]"))?),
                ),
            }
        } else {
            let (h, p) = match authority.split_once(':') {
                Some((h, p)) => (h, Some(p)),
                None => (authority, None),
            };
            if p.is_some_and(|p| p.contains(':')) {
                bail!(bad("write IPv6 addresses in brackets: https://[addr]:port"));
            }
            (h, p)
        };
        let port = match port {
            None => 443,
            Some(p) => p
                .parse::<u16>()
                .ok()
                .filter(|p| *p > 0)
                .ok_or_else(|| bad("bad port"))?,
        };
        if host.is_empty() {
            bail!(bad("no host"));
        }
        Ok(Self {
            host: host.to_ascii_lowercase(),
            port,
        })
    }

    /// The `Host` header value (and the authority of the `Origin`).
    pub fn authority(&self) -> String {
        if self.host.contains(':') {
            format!("[{}]:{}", self.host, self.port)
        } else {
            format!("{}:{}", self.host, self.port)
        }
    }
}

impl std::fmt::Display for BaseUrl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "https://{}", self.authority())
    }
}

/// Where `search --team` caches its session cookie (runtime dir, 0600).
fn session_cache(ctx: &Ctx) -> PathBuf {
    ctx.paths.runtime_dir.join("team-session.json")
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct CachedSession {
    url: String,
    fingerprint: String,
    /// `name=value` of the session cookie.
    cookie: String,
}

/// Bodies above this are refused (search pages and `show` output stay far below).
const MAX_BODY: usize = 64 << 20;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const IO_TIMEOUT: Duration = Duration::from_secs(30);

/// A JSON client for one team server, pinned to one certificate.
pub struct TeamClient {
    pub url: BaseUrl,
    tls: Arc<rustls::ClientConfig>,
    pin: Arc<PinnedCert>,
    token: String,
    cookie: Mutex<Option<String>>,
    cache: Option<PathBuf>,
    user_agent: String,
}

impl std::fmt::Debug for TeamClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TeamClient")
            .field("url", &self.url)
            .finish_non_exhaustive()
    }
}

/// An HTTP response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Response {
    pub status: u16,
    /// Lowercase names.
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Response {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(n, _)| n == name).map(|(_, v)| v.as_str())
    }

    fn error(&self, url: &BaseUrl) -> anyhow::Error {
        let message = serde_json::from_slice::<wire::ApiError>(&self.body)
            .map(|e| e.message)
            .unwrap_or_else(|_| String::from_utf8_lossy(&self.body).chars().take(200).collect());
        let mut m = format!("team server {url}: {} ({})", message.trim(), self.status);
        if self.status == 429
            && let Some(s) = self.header("retry-after")
        {
            m.push_str(&format!("; retry in {s} s"));
        }
        anyhow!(m)
    }
}

impl TeamClient {
    /// `expect`: the pinned fingerprint; `None` trusts the first certificate (TOFU, for
    /// `team-login`). `cache`: the session cookie file, if any.
    pub fn new(url: BaseUrl, expect: Option<String>, token: String, cache: Option<PathBuf>) -> Result<Self> {
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let pin = Arc::new(PinnedCert {
            expect,
            seen: Mutex::new(None),
            algs: provider.signature_verification_algorithms,
        });
        let mut tls = rustls::ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()?
            .dangerous()
            .with_custom_certificate_verifier(pin.clone())
            .with_no_client_auth();
        tls.alpn_protocols = vec![b"http/1.1".to_vec()];
        Ok(Self {
            url,
            tls: Arc::new(tls),
            pin,
            token,
            cookie: Mutex::new(None),
            cache,
            user_agent: format!("cinelog/{}", cinelog_core::VERSION),
        })
    }

    /// The client of `cinelog token team-login`'s credentials.
    pub fn from_saved(ctx: &Ctx) -> Result<Self> {
        let creds = TeamCredentials::load(&TeamCredentials::path(ctx))?;
        let url = BaseUrl::parse(&creds.url)?;
        let pin = normalize_fingerprint(&creds.fingerprint)?;
        let client = Self::new(url, Some(pin), creds.token, Some(session_cache(ctx)))?;
        *client.cookie.lock().expect("cookie lock") = client.cached_cookie();
        Ok(client)
    }

    /// Fingerprint of the certificate the server presented last.
    pub fn fingerprint(&self) -> Option<String> {
        self.pin.seen.lock().expect("pin lock").clone()
    }

    /// `GET` with the session cookie, logging in first or again after a 401. `None` on 404.
    pub fn get_json<T: DeserializeOwned>(&self, target: &str) -> Result<Option<T>> {
        let have = self.cookie.lock().expect("cookie lock").clone();
        let cookie = match have {
            Some(c) => c,
            None => self.login()?,
        };
        let mut resp = self.send("GET", target, None, Some(&cookie))?;
        if resp.status == 401 {
            let cookie = self.login()?;
            resp = self.send("GET", target, None, Some(&cookie))?;
        }
        match resp.status {
            200 => Ok(Some(serde_json::from_slice(&resp.body).with_context(|| {
                format!("team server {}: unexpected response to {target}", self.url)
            })?)),
            404 => Ok(None),
            _ => Err(resp.error(&self.url)),
        }
    }

    /// `POST /api/login`; keeps (and caches) the session cookie and returns it.
    fn login(&self) -> Result<String> {
        let body = serde_json::to_vec(&wire::LoginRequest {
            token: self.token.clone(),
        })?;
        let resp = self.send("POST", "/api/login", Some(&body), None)?;
        match resp.status {
            200 | 204 => {}
            401 => bail!(
                "team server {}: the token was rejected (revoked?); get a new one from the team admin and run `cinelog token team-login`",
                self.url
            ),
            _ => return Err(resp.error(&self.url)),
        }
        let cookie = resp
            .headers
            .iter()
            .filter(|(n, _)| n == "set-cookie")
            .filter_map(|(_, v)| v.split(';').next())
            .map(str::trim)
            .find(|c| c.starts_with("__Host-cl-") && c.contains('='))
            .with_context(|| format!("team server {}: login set no session cookie", self.url))?
            .to_owned();
        *self.cookie.lock().expect("cookie lock") = Some(cookie.clone());
        self.store_cookie(&cookie);
        Ok(cookie)
    }

    fn cached_cookie(&self) -> Option<String> {
        let path = self.cache.as_ref()?;
        let meta = fs::symlink_metadata(path).ok()?;
        if !meta.is_file() || meta.permissions().mode() & 0o077 != 0 {
            return None;
        }
        let c: CachedSession = serde_json::from_slice(&fs::read(path).ok()?).ok()?;
        let pin = self.pin.expect.as_deref()?;
        (c.url == self.url.to_string() && c.fingerprint == pin).then_some(c.cookie)
    }

    /// Best effort: a failed write only costs a login next time.
    fn store_cookie(&self, cookie: &str) {
        let (Some(path), Some(fingerprint)) = (&self.cache, self.pin.expect.clone()) else {
            return;
        };
        let c = CachedSession {
            url: self.url.to_string(),
            fingerprint,
            cookie: cookie.to_owned(),
        };
        if let Some(dir) = path.parent()
            && fsutil::ensure_dir(dir, 0o700).is_ok()
            && let Ok(bytes) = serde_json::to_vec(&c)
        {
            let _ = write_private(path, &bytes);
        }
    }

    /// One request on a new connection (`Connection: close`).
    pub fn send(&self, method: &str, target: &str, body: Option<&[u8]>, cookie: Option<&str>) -> Result<Response> {
        let tcp = self.connect()?;
        let name = rustls::pki_types::ServerName::try_from(self.url.host.clone())
            .map_err(|_| anyhow!("bad host name {:?}", self.url.host))?;
        let conn = rustls::ClientConnection::new(self.tls.clone(), name)?;
        let mut stream = rustls::StreamOwned::new(conn, tcp);
        let head = request_head(
            &self.url,
            method,
            target,
            body.map(<[u8]>::len),
            cookie,
            &self.user_agent,
        );
        let result = (|| -> io::Result<Response> {
            stream.write_all(head.as_bytes())?;
            if let Some(b) = body {
                stream.write_all(b)?;
            }
            stream.flush()?;
            read_response(&mut BufReader::new(&mut stream), method == "HEAD")
        })();
        result.map_err(|e| self.explain(e, target))
    }

    fn connect(&self) -> Result<TcpStream> {
        let addrs = (self.url.host.as_str(), self.url.port)
            .to_socket_addrs()
            .with_context(|| format!("resolving {}", self.url.host))?;
        let mut last = None;
        for addr in addrs {
            match TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT) {
                Ok(s) => {
                    s.set_read_timeout(Some(IO_TIMEOUT))?;
                    s.set_write_timeout(Some(IO_TIMEOUT))?;
                    s.set_nodelay(true)?;
                    return Ok(s);
                }
                Err(e) => last = Some(e),
            }
        }
        Err(match last {
            Some(e) => anyhow!("cannot reach the team server {}: {e}", self.url),
            None => anyhow!("{} has no address", self.url.host),
        })
    }

    fn explain(&self, e: io::Error, target: &str) -> anyhow::Error {
        if let (Some(want), Some(seen)) = (&self.pin.expect, self.fingerprint())
            && *want != seen
        {
            return anyhow!(
                "the team server {} presents another certificate ({seen}) than the pinned one ({want}). \
                 If it was regenerated, compare with `sudo cinelog tls show` on the server and run \
                 `cinelog token team-login --url {} --fingerprint {seen}`",
                self.url,
                self.url
            );
        }
        anyhow!("team server {}: {target}: {e}", self.url)
    }
}

fn request_head(
    url: &BaseUrl,
    method: &str,
    target: &str,
    body_len: Option<usize>,
    cookie: Option<&str>,
    user_agent: &str,
) -> String {
    let authority = url.authority();
    let mut h = format!(
        "{method} {target} HTTP/1.1\r\nHost: {authority}\r\nUser-Agent: {user_agent}\r\n\
         Accept: application/json\r\nConnection: close\r\n"
    );
    if let Some(c) = cookie {
        h.push_str(&format!("Cookie: {c}\r\n"));
    }
    if let Some(n) = body_len {
        h.push_str(&format!(
            "Origin: https://{authority}\r\nContent-Type: application/json\r\nContent-Length: {n}\r\n"
        ));
    }
    h.push_str("\r\n");
    h
}

/// Parses one HTTP/1.1 response: `Content-Length`, chunked, or until EOF.
pub fn read_response(r: &mut impl BufRead, head_only: bool) -> io::Result<Response> {
    let bad = |m: &str| io::Error::new(io::ErrorKind::InvalidData, m.to_owned());
    loop {
        let status_line = read_line(r)?;
        let status = status_line
            .strip_prefix("HTTP/1.")
            .and_then(|s| s.get(2..5))
            .and_then(|s| s.parse::<u16>().ok())
            .ok_or_else(|| bad("not an HTTP/1.x response"))?;
        let mut headers = Vec::new();
        let mut size = 0;
        loop {
            let line = read_line(r)?;
            if line.is_empty() {
                break;
            }
            size += line.len();
            if size > 64 << 10 {
                return Err(bad("response headers too large"));
            }
            let (n, v) = line.split_once(':').ok_or_else(|| bad("malformed header"))?;
            headers.push((n.trim().to_ascii_lowercase(), v.trim().to_owned()));
        }
        if (100..200).contains(&status) {
            continue;
        }
        let get = |n: &str| headers.iter().find(|(k, _)| k == n).map(|(_, v)| v.as_str());
        let body = if head_only || status == 204 || status == 304 {
            Vec::new()
        } else if get("transfer-encoding").is_some_and(|v| v.to_ascii_lowercase().contains("chunked")) {
            read_chunked(r)?
        } else if let Some(n) = get("content-length") {
            let n: usize = n.parse().map_err(|_| bad("bad content-length"))?;
            if n > MAX_BODY {
                return Err(bad("response too large"));
            }
            let mut b = vec![0; n];
            r.read_exact(&mut b)?;
            b
        } else {
            read_to_eof(r)?
        };
        return Ok(Response { status, headers, body });
    }
}

fn read_line(r: &mut impl BufRead) -> io::Result<String> {
    let mut buf = Vec::new();
    r.by_ref().take(16 << 10).read_until(b'\n', &mut buf)?;
    if buf.last() != Some(&b'\n') {
        return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "truncated response"));
    }
    while matches!(buf.last(), Some(b'\n' | b'\r')) {
        buf.pop();
    }
    String::from_utf8(buf).map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "non-UTF-8 header"))
}

fn read_chunked(r: &mut impl BufRead) -> io::Result<Vec<u8>> {
    let mut body = Vec::new();
    loop {
        let line = read_line(r)?;
        let size = line.split(';').next().unwrap_or_default().trim();
        let n = usize::from_str_radix(size, 16)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "bad chunk size"))?;
        if n == 0 {
            // Trailers up to the blank line.
            while !read_line(r)?.is_empty() {}
            return Ok(body);
        }
        if body.len() + n > MAX_BODY {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "response too large"));
        }
        let start = body.len();
        body.resize(start + n, 0);
        r.read_exact(&mut body[start..])?;
        read_line(r)?;
    }
}

/// Without a length the body ends with the connection; a missing TLS close_notify at that
/// point is not an error.
fn read_to_eof(r: &mut impl BufRead) -> io::Result<Vec<u8>> {
    let mut body = Vec::new();
    match r.take(MAX_BODY as u64 + 1).read_to_end(&mut body) {
        Ok(_) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => {}
        Err(e) => return Err(e),
    }
    if body.len() > MAX_BODY {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "response too large"));
    }
    Ok(body)
}

/// Accepts exactly the pinned leaf certificate (or, without a pin, the first one seen),
/// and verifies the handshake signatures with it.
#[derive(Debug)]
struct PinnedCert {
    expect: Option<String>,
    seen: Mutex<Option<String>>,
    algs: rustls::crypto::WebPkiSupportedAlgorithms,
}

impl rustls::client::danger::ServerCertVerifier for PinnedCert {
    fn verify_server_cert(
        &self,
        end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        let fp = cinelog_web::tls::fingerprint(end_entity.as_ref());
        let ok = self.expect.as_ref().is_none_or(|want| *want == fp);
        *self.seen.lock().expect("pin lock") = Some(fp);
        if ok {
            Ok(rustls::client::danger::ServerCertVerified::assertion())
        } else {
            Err(rustls::Error::General(
                "certificate fingerprint does not match the pin".into(),
            ))
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(message, cert, dss, &self.algs)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.algs)
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.algs.supported_schemes()
    }
}

/// Percent-encodes a query value (RFC 3986 unreserved characters stay).
pub fn query_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// A one-thread HTTPS/1.1 server with a fixed self-signed certificate, and a fake team API
/// on top of it (login, cookie, Host/Origin checks), for the client tests here and in
/// `search::team`.
#[cfg(test)]
pub(crate) mod testserver {
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::thread::JoinHandle;

    use cinelog_web::api::types as wire;
    use data_encoding::BASE64;

    /// P-256, CN=localhost, SAN localhost and 127.0.0.1, valid until 2126.
    const CERT: &str = "MIIBmzCCAUGgAwIBAgIUReiotmEgP1iMgAqEY3pTDmRG6m4wCgYIKoZIzj0EAwIwFDESMBAGA1UEAwwJbG9jYWxob3N0MCAXDTI2MTEwODA5MjUwMVoYDzIxMjYxMDE1MDkyNTAxWjAUMRIwEAYDVQQDDAlsb2NhbGhvc3QwWTATBgcqhkjOPQIBBggqhkjOPQMBBwNCAAS8pxS9TRI/D4AGjQRlupCRBxpat0PDrPPq0vrTDSUmVZo4EJ7Zlu8jhzPhSlw0j4UjdlIZH8Cw0T/tSsyuVNUCo28wbTAdBgNVHQ4EFgQUnlEaiNNI0QNo+yrVVYW9CnC2SeAwHwYDVR0jBBgwFoAUnlEaiNNI0QNo+yrVVYW9CnC2SeAwDwYDVR0TAQH/BAUwAwEB/zAaBgNVHREEEzARgglsb2NhbGhvc3SHBH8AAAEwCgYIKoZIzj0EAwIDSAAwRQIhAM/mINu3HgHKcr3rMGNk9YrOwPQRZYqjT0dBRupX2EY/AiA5U97mTRb9cNGZN9Ft98SCCCCrtZY+pNIFRNeNOMGkbQ==";
    const KEY: &str = "MIGHAgEAMBMGByqGSM49AgEGCCqGSM49AwEHBG0wawIBAQQgsQcjCQSiiVXiyWsXYUmiRDFTSlTBayjDetLAieLSjwuhRANCAAS8pxS9TRI/D4AGjQRlupCRBxpat0PDrPPq0vrTDSUmVZo4EJ7Zlu8jhzPhSlw0j4UjdlIZH8Cw0T/tSsyuVNUC";

    pub fn fingerprint() -> String {
        cinelog_web::tls::fingerprint(&BASE64.decode(CERT.as_bytes()).unwrap())
    }

    #[derive(Debug, Clone)]
    pub struct Req {
        pub method: String,
        pub target: String,
        pub headers: Vec<(String, String)>,
        pub body: Vec<u8>,
    }

    impl Req {
        pub fn header(&self, name: &str) -> Option<&str> {
            self.headers.iter().find(|(n, _)| n == name).map(|(_, v)| v.as_str())
        }

        pub fn path(&self) -> &str {
            self.target.split('?').next().unwrap_or_default()
        }

        /// Decoded query parameters.
        pub fn query(&self) -> Vec<(String, String)> {
            let Some((_, q)) = self.target.split_once('?') else {
                return Vec::new();
            };
            q.split('&')
                .filter_map(|kv| kv.split_once('='))
                .map(|(k, v)| (k.to_owned(), decode(v)))
                .collect()
        }
    }

    fn decode(s: &str) -> String {
        let b = s.as_bytes();
        let mut out = Vec::new();
        let mut i = 0;
        while i < b.len() {
            if b[i] == b'%' && i + 2 < b.len() {
                out.push(u8::from_str_radix(&s[i + 1..i + 3], 16).unwrap());
                i += 3;
            } else {
                out.push(b[i]);
                i += 1;
            }
        }
        String::from_utf8(out).unwrap()
    }

    pub struct Reply {
        pub status: u16,
        pub headers: Vec<(String, String)>,
        pub body: Vec<u8>,
        /// Send the body chunked instead of with a length.
        pub chunked: bool,
    }

    impl Reply {
        pub fn json(status: u16, v: &impl serde::Serialize) -> Self {
            Self {
                status,
                headers: vec![("Content-Type".into(), "application/json".into())],
                body: serde_json::to_vec(v).unwrap(),
                chunked: false,
            }
        }

        pub fn error(status: u16, code: wire::ErrorCode, message: &str) -> Self {
            Self::json(
                status,
                &wire::ApiError {
                    error: code,
                    message: message.into(),
                },
            )
        }
    }

    type Handler = dyn Fn(&Req) -> Reply + Send + Sync;

    pub struct TestServer {
        pub port: u16,
        stop: Arc<AtomicBool>,
        thread: Option<JoinHandle<()>>,
    }

    impl TestServer {
        pub fn start(handler: impl Fn(&Req) -> Reply + Send + Sync + 'static) -> Self {
            let provider = Arc::new(rustls::crypto::ring::default_provider());
            let cert = rustls::pki_types::CertificateDer::from(BASE64.decode(CERT.as_bytes()).unwrap());
            let key = rustls::pki_types::PrivateKeyDer::Pkcs8(BASE64.decode(KEY.as_bytes()).unwrap().into());
            let config = Arc::new(
                rustls::ServerConfig::builder_with_provider(provider)
                    .with_safe_default_protocol_versions()
                    .unwrap()
                    .with_no_client_auth()
                    .with_single_cert(vec![cert], key)
                    .unwrap(),
            );
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let port = listener.local_addr().unwrap().port();
            let stop = Arc::new(AtomicBool::new(false));
            let handler: Arc<Handler> = Arc::new(handler);
            let stopping = stop.clone();
            let thread = std::thread::spawn(move || {
                for tcp in listener.incoming() {
                    if stopping.load(Ordering::SeqCst) {
                        break;
                    }
                    if let Ok(tcp) = tcp {
                        let _ = serve(tcp, config.clone(), &*handler);
                    }
                }
            });
            Self {
                port,
                stop,
                thread: Some(thread),
            }
        }
    }

    impl Drop for TestServer {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::SeqCst);
            let _ = TcpStream::connect(("127.0.0.1", self.port));
            if let Some(t) = self.thread.take() {
                let _ = t.join();
            }
        }
    }

    fn serve(tcp: TcpStream, config: Arc<rustls::ServerConfig>, handler: &Handler) -> std::io::Result<()> {
        tcp.set_read_timeout(Some(std::time::Duration::from_secs(5)))?;
        let conn = rustls::ServerConnection::new(config).map_err(std::io::Error::other)?;
        let mut tls = rustls::StreamOwned::new(conn, tcp);
        let req = {
            let mut r = BufReader::new(&mut tls);
            let mut line = String::new();
            r.read_line(&mut line)?;
            let mut parts = line.split_whitespace();
            let method = parts.next().unwrap_or_default().to_owned();
            let target = parts.next().unwrap_or_default().to_owned();
            let mut headers = Vec::new();
            loop {
                let mut h = String::new();
                r.read_line(&mut h)?;
                let h = h.trim_end();
                if h.is_empty() {
                    break;
                }
                if let Some((n, v)) = h.split_once(':') {
                    headers.push((n.trim().to_ascii_lowercase(), v.trim().to_owned()));
                }
            }
            let len = headers
                .iter()
                .find(|(n, _)| n == "content-length")
                .map_or(0, |(_, v)| v.parse().unwrap_or(0));
            let mut body = vec![0; len];
            r.read_exact(&mut body)?;
            Req {
                method,
                target,
                headers,
                body,
            }
        };
        let reply = handler(&req);
        let mut out = format!("HTTP/1.1 {} X\r\nConnection: close\r\n", reply.status);
        for (n, v) in &reply.headers {
            out.push_str(&format!("{n}: {v}\r\n"));
        }
        if reply.chunked {
            out.push_str("Transfer-Encoding: chunked\r\n\r\n");
            for c in reply.body.chunks(7) {
                out.push_str(&format!("{:x}\r\n", c.len()));
                out.push_str(std::str::from_utf8(c).unwrap());
                out.push_str("\r\n");
            }
            out.push_str("0\r\n\r\n");
            tls.write_all(out.as_bytes())?;
        } else {
            out.push_str(&format!("Content-Length: {}\r\n\r\n", reply.body.len()));
            tls.write_all(out.as_bytes())?;
            tls.write_all(&reply.body)?;
        }
        tls.conn.send_close_notify();
        tls.flush()
    }

    /// The auth part of the team API: `POST /api/login` with `token`, the cookie, Host and
    /// Origin checks; everything else goes to `routes` (`None` → 404).
    pub struct FakeTeam {
        pub token: String,
        pub scope: wire::Scope,
        pub logins: AtomicUsize,
        pub sessions: Mutex<Vec<String>>,
        pub requests: Mutex<Vec<Req>>,
    }

    impl FakeTeam {
        pub fn new(token: &str) -> Arc<Self> {
            Arc::new(Self {
                token: token.into(),
                scope: wire::Scope::Team,
                logins: AtomicUsize::new(0),
                sessions: Mutex::new(Vec::new()),
                requests: Mutex::new(Vec::new()),
            })
        }

        pub fn logins(&self) -> usize {
            self.logins.load(Ordering::SeqCst)
        }

        pub fn serve(self: &Arc<Self>, routes: impl Fn(&Req) -> Option<Reply> + Send + Sync + 'static) -> TestServer {
            let me = self.clone();
            TestServer::start(move |req| me.handle(req, &routes))
        }

        fn handle(&self, req: &Req, routes: &dyn Fn(&Req) -> Option<Reply>) -> Reply {
            use wire::ErrorCode as E;
            self.requests.lock().unwrap().push(req.clone());
            let host = req.header("host").unwrap_or_default();
            if !host.starts_with("127.0.0.1:") {
                return Reply::error(400, E::BadRequest, "host");
            }
            if req.method != "GET" {
                if req.header("origin") != Some(&format!("https://{host}")) {
                    return Reply::error(403, E::Forbidden, "origin");
                }
                if req.header("content-type") != Some("application/json") {
                    return Reply::error(415, E::UnsupportedMediaType, "json");
                }
            }
            if req.method == "GET" && req.path() == "/api/login" {
                return Reply::json(
                    200,
                    &wire::LoginInfo {
                        version: "0.12.0".into(),
                        scope: self.scope,
                        fingerprint: fingerprint(),
                    },
                );
            }
            if req.method == "POST" && req.path() == "/api/login" {
                let login: wire::LoginRequest = serde_json::from_slice(&req.body).unwrap();
                if login.token != self.token {
                    return Reply::error(401, E::Unauthorized, "bad token");
                }
                let n = self.logins.fetch_add(1, Ordering::SeqCst);
                let sid = format!("sid{n}");
                self.sessions.lock().unwrap().push(sid.clone());
                let port = host.rsplit(':').next().unwrap();
                return Reply {
                    status: 204,
                    headers: vec![(
                        "Set-Cookie".into(),
                        format!("__Host-cl-{port}={sid}; HttpOnly; Secure; SameSite=Strict; Path=/; Max-Age=604800"),
                    )],
                    body: Vec::new(),
                    chunked: false,
                };
            }
            let cookie = req
                .header("cookie")
                .and_then(|c| c.split_once('='))
                .map(|(_, v)| v.to_owned());
            let known = cookie.is_some_and(|c| self.sessions.lock().unwrap().contains(&c));
            if !known || !req.header("user-agent").is_some_and(|u| u.starts_with("cinelog/")) {
                return Reply::error(401, E::Unauthorized, "no session");
            }
            if req.path() == "/api/me" {
                return Reply::json(
                    200,
                    &wire::Me {
                        principal: "alice".into(),
                        scope: self.scope,
                        host: "teamhost".into(),
                        version: "0.12.0".into(),
                        protocol: 1,
                        fingerprint: fingerprint(),
                        expires_at: 0.0,
                    },
                );
            }
            routes(req).unwrap_or_else(|| Reply::error(404, E::NotFound, "not found"))
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use cinelog_core::paths::{EnvSnapshot, SystemRoot};

    use super::testserver::{FakeTeam, Reply, TestServer};
    use super::*;

    fn personal_ctx(dir: &Path) -> Ctx {
        Ctx::with(
            EnvSnapshot {
                uid: 1000,
                euid: 1000,
                user: "alice".into(),
                home: dir.join("home"),
                xdg_config_home: Some(dir.join("config")),
                xdg_state_home: Some(dir.join("state")),
                xdg_runtime_dir: Some(dir.join("run")),
            },
            SystemRoot::real(),
        )
    }

    fn team_ctx(dir: &Path) -> Ctx {
        Ctx::with(
            EnvSnapshot {
                uid: 0,
                euid: 0,
                user: "root".into(),
                home: "/root".into(),
                xdg_config_home: None,
                xdg_state_home: None,
                xdg_runtime_dir: None,
            },
            SystemRoot::prefixed(dir.join("sys")),
        )
    }

    fn run_action(ctx: &Ctx, action: TokenAction) -> (Result<u8>, String) {
        let mut out = Vec::new();
        let r = manage(ctx, &action, 1_794_139_200.5, &mut out);
        (r, String::from_utf8(out).unwrap())
    }

    fn secret_of(out: &str) -> String {
        out.lines()
            .find_map(|l| l.split("(shown once): ").nth(1))
            .unwrap_or_else(|| panic!("{out}"))
            .to_owned()
    }

    fn mode(p: &Path) -> u32 {
        fs::metadata(p).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn personal_new_replaces_the_one_token() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = personal_ctx(dir.path());
        let file = ctx.paths.auth_file.clone();
        assert!(file.starts_with(dir.path().join("config/cinelog")));

        let (r, out) = run_action(&ctx, TokenAction::New { name: None });
        assert_eq!(r.unwrap(), 0);
        let first = secret_of(&out);
        assert!(out.starts_with("token for alice"), "{out}");
        assert_eq!(mode(&file), 0o600);
        assert_eq!(mode(file.parent().unwrap()), 0o700);
        assert!(!fs::read_to_string(&file).unwrap().contains(&first));

        let (r, out) = run_action(&ctx, TokenAction::New { name: None });
        assert_eq!(r.unwrap(), 0);
        let second = secret_of(&out);
        let tf = TokenFile::load(&file).unwrap();
        assert_eq!(tf.tokens.len(), 1);
        assert!(tf.verify(&first).is_none(), "the old token no longer logs in");
        assert_eq!(tf.verify(&second).map(|t| t.name.as_str()), Some("alice"));

        let (_, out) = run_action(&ctx, TokenAction::List { json: true });
        let rows: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(rows[0]["name"], "alice");
        assert_eq!(rows[0]["created_at"], 1_794_139_200.5);
        assert!(!out.contains(&second));

        assert_eq!(ensure_personal(&ctx).unwrap(), None, "a token exists");
        let (r, out) = run_action(&ctx, TokenAction::Revoke { name: "alice".into() });
        assert_eq!(
            (r.unwrap(), out.as_str()),
            (0, "revoked alice; ended 0 web session(s)\n")
        );
        let (r, _) = run_action(&ctx, TokenAction::Revoke { name: "alice".into() });
        assert_eq!(r.unwrap(), NOT_FOUND);

        let t = ensure_personal(&ctx).unwrap().expect("created again");
        assert_eq!(t.name, "alice");
        assert_eq!(ensure_personal(&ctx).unwrap(), None);
    }

    #[test]
    fn team_tokens_are_named_per_member() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = team_ctx(dir.path());
        let file = ctx.paths.auth_file.clone();
        assert_eq!(file, dir.path().join("sys/etc/cinelog/tokens.toml"));

        let (r, _) = run_action(
            &ctx,
            TokenAction::New {
                name: Some("alice".into()),
            },
        );
        assert!(format!("{:#}", r.unwrap_err()).contains("install --team"));
        fs::create_dir_all(file.parent().unwrap()).unwrap();

        let (r, _) = run_action(&ctx, TokenAction::New { name: None });
        assert!(format!("{:#}", r.unwrap_err()).contains("token new NAME"));
        let (r, out) = run_action(
            &ctx,
            TokenAction::New {
                name: Some("alice".into()),
            },
        );
        assert_eq!(r.unwrap(), 0);
        let alice = secret_of(&out);
        assert!(out.contains("team-login"), "{out}");
        let (_, out) = run_action(
            &ctx,
            TokenAction::New {
                name: Some("bob".into()),
            },
        );
        let bob = secret_of(&out);
        let (r, _) = run_action(
            &ctx,
            TokenAction::New {
                name: Some("alice".into()),
            },
        );
        assert!(format!("{:#}", r.unwrap_err()).contains("already exists"));
        let (r, _) = run_action(
            &ctx,
            TokenAction::New {
                name: Some("a/b".into()),
            },
        );
        assert!(r.is_err());

        let (_, out) = run_action(&ctx, TokenAction::List { json: false });
        let names: Vec<&str> = out.lines().map(|l| l.split_whitespace().next().unwrap()).collect();
        assert_eq!(names, ["alice", "bob"]);
        assert!(out.lines().all(|l| l.contains("created ")), "{out}");

        let (r, _) = run_action(&ctx, TokenAction::Revoke { name: "alice".into() });
        assert_eq!(r.unwrap(), 0);
        let tf = TokenFile::load(&file).unwrap();
        assert!(tf.verify(&alice).is_none());
        assert_eq!(tf.verify(&bob).map(|t| t.name.as_str()), Some("bob"));
        assert_eq!(mode(&file), 0o600);
        assert_eq!(ensure_personal(&ctx).unwrap(), None, "TEAM has no personal token");
    }

    #[test]
    fn concurrent_updates_lose_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("tokens.toml");
        let threads: Vec<_> = (0..8)
            .map(|i| {
                let file = file.clone();
                std::thread::spawn(move || ensure_named(&file, &format!("m{i}"), 1.0).unwrap())
            })
            .collect();
        for t in threads {
            assert!(t.join().unwrap().is_some());
        }
        assert_eq!(TokenFile::load(&file).unwrap().tokens.len(), 8);
        assert_eq!(ensure_named(&file, "m3", 2.0).unwrap(), None);
    }

    #[test]
    fn personal_names() {
        assert_eq!(personal_name("alice"), "alice");
        assert_eq!(personal_name("Jöhn Doe"), "J_hn_Doe");
        assert_eq!(personal_name("-.x"), "x");
        assert_eq!(personal_name(""), "user");
        assert_eq!(personal_name(&"a".repeat(80)).len(), 64);
        assert!(tokens::valid_name(&personal_name("ad\\user@corp")));
    }

    #[test]
    fn fingerprints_normalize() {
        let canon = cinelog_web::tls::fingerprint(b"x");
        let bare = canon.replace(':', "").to_lowercase();
        for s in [
            canon.clone(),
            bare.clone(),
            format!("sha256:{bare}"),
            format!("SHA256 Fingerprint={canon}"),
        ] {
            assert_eq!(normalize_fingerprint(&s).unwrap(), canon, "{s}");
        }
        assert!(normalize_fingerprint("AA:BB").is_err());
        assert!(normalize_fingerprint(&"g".repeat(64)).is_err());
    }

    #[test]
    fn base_urls() {
        let u = BaseUrl::parse("https://Team.LAN:10000/").unwrap();
        assert_eq!((u.host.as_str(), u.port), ("team.lan", 10000));
        assert_eq!(u.to_string(), "https://team.lan:10000");
        let u = BaseUrl::parse("https://[fe80::1]:10443").unwrap();
        assert_eq!(
            (u.host.as_str(), u.authority()),
            ("fe80::1", "[fe80::1]:10443".to_owned())
        );
        assert_eq!(BaseUrl::parse("https://box").unwrap().port, 443);
        for bad in [
            "http://box:1",
            "https://",
            "https://box:0",
            "https://box:x",
            "https://u@box:1",
            "https://box:1/s/x",
            "https://fe80::1:1",
            "https://[::1",
        ] {
            assert!(BaseUrl::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn responses_parse() {
        let parse = |s: &str| read_response(&mut Cursor::new(s.as_bytes().to_vec()), false);
        let r = parse("HTTP/1.1 200 OK\r\nContent-Length: 5\r\nX-A: b\r\n\r\nhelloEXTRA").unwrap();
        assert_eq!(
            (r.status, r.body.as_slice(), r.header("x-a")),
            (200, &b"hello"[..], Some("b"))
        );
        let r =
            parse("HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabc\r\n2;x=y\r\nde\r\n0\r\nT: 1\r\n\r\n")
                .unwrap();
        assert_eq!(r.body, b"abcde");
        let r = parse("HTTP/1.1 100 Continue\r\n\r\nHTTP/1.1 204 No Content\r\nSet-Cookie: a=b\r\n\r\n").unwrap();
        assert_eq!((r.status, r.body.len(), r.header("set-cookie")), (204, 0, Some("a=b")));
        assert_eq!(parse("HTTP/1.0 404 X\r\n\r\nall of it").unwrap().body, b"all of it");
        assert!(parse("SSH-2.0-OpenSSH\r\n\r\n").is_err());
        assert!(parse("HTTP/1.1 200 OK\r\nContent-Length: 9\r\n\r\nshort").is_err());
        assert!(parse(&format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n", MAX_BODY + 1)).is_err());
        let big = format!("HTTP/1.1 200 OK\r\n{}\r\n", "X: y\r\n".repeat(20_000));
        assert!(parse(&big).is_err());
    }

    #[test]
    fn requests_carry_origin_only_with_a_body() {
        let u = BaseUrl::parse("https://box:10000").unwrap();
        let get = request_head(&u, "GET", "/api/me", None, Some("__Host-cl-10000=s"), "cinelog/1");
        assert!(get.starts_with("GET /api/me HTTP/1.1\r\nHost: box:10000\r\n"));
        assert!(get.contains("Cookie: __Host-cl-10000=s\r\n") && !get.contains("Origin"));
        assert!(get.ends_with("\r\n\r\n"));
        let post = request_head(&u, "POST", "/api/login", Some(9), None, "cinelog/1");
        assert!(post.contains("Origin: https://box:10000\r\nContent-Type: application/json\r\nContent-Length: 9\r\n"));
        assert_eq!(query_escape("a b&c=ü/%"), "a%20b%26c%3D%C3%BC%2F%25");
    }

    #[test]
    fn credentials_file_is_private() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cfg/cinelog/team-token");
        let err = TeamCredentials::load(&path).unwrap_err();
        assert!(err.to_string().contains("team-login"), "{err}");
        let c = TeamCredentials {
            url: "https://t:10000".into(),
            token: "T".into(),
            fingerprint: "AA".into(),
        };
        c.save(&path).unwrap();
        assert_eq!(mode(&path), 0o600);
        assert_eq!(mode(path.parent().unwrap()), 0o700);
        assert_eq!(TeamCredentials::load(&path).unwrap(), c);
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(
            TeamCredentials::load(&path)
                .unwrap_err()
                .to_string()
                .contains("chmod 600")
        );
    }

    fn local(server: &TestServer) -> BaseUrl {
        BaseUrl {
            host: "127.0.0.1".into(),
            port: server.port,
        }
    }

    #[test]
    fn login_check_sends_the_token_only_to_the_pinned_certificate() {
        let (token, _) = tokens::generate();
        let team = FakeTeam::new(&token);
        let server = team.serve(|_| None);
        let fp = testserver::fingerprint();
        let (creds, me) = login_check(local(&server), fp.clone(), &format!(" {token}\n")).unwrap();
        assert_eq!(creds.fingerprint, fp);
        assert_eq!(creds.url, format!("https://127.0.0.1:{}", server.port));
        assert_eq!(creds.token, token);
        assert_eq!(me.principal, "alice");
        assert_eq!(team.logins(), 1);

        let other = cinelog_web::tls::fingerprint(b"another cert");
        let err = format!("{:#}", login_check(local(&server), other.clone(), &token).unwrap_err());
        assert!(err.contains("another certificate") && err.contains(&other), "{err}");
        assert_eq!(posts(&team), 1, "the handshake failed before the token was sent");

        let (wrong, _) = tokens::generate();
        let err = format!("{:#}", login_check(local(&server), fp.clone(), &wrong).unwrap_err());
        assert!(err.contains("rejected"), "{err}");
        let err = format!("{:#}", login_check(local(&server), fp, "not-a-token").unwrap_err());
        assert!(err.contains("not a cinelog token"), "{err}");
    }

    #[test]
    fn a_personal_server_is_not_the_team_server() {
        let (token, _) = tokens::generate();
        let team = Arc::new(FakeTeam {
            scope: wire::Scope::Personal,
            ..Arc::try_unwrap(FakeTeam::new(&token)).ok().unwrap()
        });
        let server = team.serve(|_| None);
        let err = format!(
            "{:#}",
            login_check(local(&server), testserver::fingerprint(), &token).unwrap_err()
        );
        assert!(err.contains("personal cinelog server"), "{err}");
        let err = format!("{:#}", probe(&local(&server)).unwrap_err());
        assert!(err.contains("personal cinelog server"), "{err}");
        assert_eq!(posts(&team), 1, "the probe sends no token");
    }

    /// Scripted answers for `team-login`.
    #[derive(Default)]
    struct FakePrompt {
        interactive: bool,
        yes: bool,
        token: String,
        asked: Vec<String>,
        token_read: bool,
    }

    impl LoginPrompt for FakePrompt {
        fn interactive(&self) -> bool {
            self.interactive
        }
        fn confirm(&mut self, message: &str) -> Result<bool> {
            self.asked.push(message.to_owned());
            Ok(self.yes)
        }
        fn token(&mut self, _url: &BaseUrl) -> Result<String> {
            self.token_read = true;
            Ok(self.token.clone())
        }
    }

    /// `POST /api/login` requests the fake server received.
    fn posts(team: &FakeTeam) -> usize {
        team.requests
            .lock()
            .unwrap()
            .iter()
            .filter(|r| r.method == "POST" && r.path() == "/api/login")
            .count()
    }

    fn login_as(ctx: &Ctx, server: &TestServer, fingerprint: Option<&str>, p: &mut FakePrompt) -> (Result<u8>, String) {
        let url = local(server).to_string();
        let mut out = Vec::new();
        let r = team_login(ctx, Some(&url), fingerprint, p, &mut out);
        (r, String::from_utf8(out).unwrap())
    }

    /// The member sees the certificate and says yes before the token leaves the machine;
    /// without a terminal only `--fingerprint` pins it.
    #[test]
    fn team_login_confirms_the_certificate_before_sending_the_token() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = personal_ctx(dir.path());
        let saved = TeamCredentials::path(&ctx);
        let (token, _) = tokens::generate();
        let team = FakeTeam::new(&token);
        let server = team.serve(|_| None);
        let fp = testserver::fingerprint();

        // No terminal and no fingerprint: refused before connecting.
        let mut p = FakePrompt {
            token: token.clone(),
            ..FakePrompt::default()
        };
        let (r, _) = login_as(&ctx, &server, None, &mut p);
        let err = format!("{:#}", r.unwrap_err());
        assert!(err.contains("--fingerprint"), "{err}");
        assert!(!p.token_read && p.asked.is_empty());
        assert!(team.requests.lock().unwrap().is_empty(), "nothing was sent");

        // Shown, not confirmed: only the token-less probe reached the server.
        let mut p = FakePrompt {
            interactive: true,
            token: token.clone(),
            ..FakePrompt::default()
        };
        let (r, _) = login_as(&ctx, &server, None, &mut p);
        let err = format!("{:#}", r.unwrap_err());
        assert!(err.contains("not confirmed"), "{err}");
        assert!(
            p.asked[0].contains(&fp) && p.asked[0].contains("tls show"),
            "{}",
            p.asked[0]
        );
        assert!(!p.token_read);
        assert_eq!(posts(&team), 0);
        assert!(!saved.exists());

        // Confirmed: logged in with the confirmed pin.
        p.yes = true;
        let (r, out) = login_as(&ctx, &server, None, &mut p);
        assert_eq!(r.unwrap(), 0, "{out}");
        assert!(
            out.contains(&format!("pinned the certificate you confirmed: {fp}")),
            "{out}"
        );
        let creds = TeamCredentials::load(&saved).unwrap();
        assert_eq!(
            (creds.fingerprint.as_str(), creds.token.as_str()),
            (fp.as_str(), token.as_str())
        );
        assert_eq!(posts(&team), 1);

        // --fingerprint needs no terminal; a wrong one never gets the token.
        let mut p = FakePrompt {
            token: token.clone(),
            ..FakePrompt::default()
        };
        let bare = fp.replace(':', "").to_lowercase();
        let (r, out) = login_as(&ctx, &server, Some(&bare), &mut p);
        assert_eq!(r.unwrap(), 0);
        assert!(out.contains("matches the given fingerprint"), "{out}");
        assert!(p.asked.is_empty());
        let other = cinelog_web::tls::fingerprint(b"squatter");
        let (r, _) = login_as(&ctx, &server, Some(&other), &mut p);
        assert!(format!("{:#}", r.unwrap_err()).contains("another certificate"));
        assert_eq!(posts(&team), 2);
        assert_eq!(TeamCredentials::load(&saved).unwrap().fingerprint, fp, "kept");

        let (r, _) = login_as(&team_ctx(dir.path()), &server, Some(&fp), &mut p);
        assert!(format!("{:#}", r.unwrap_err()).contains("member shells"));
    }

    #[test]
    fn the_session_cookie_is_cached_and_renewed() {
        let dir = tempfile::tempdir().unwrap();
        let (token, _) = tokens::generate();
        let team = FakeTeam::new(&token);
        let server = team.serve(|req| (req.path() == "/api/ping").then(|| Reply::json(200, &"pong")));
        let cache = dir.path().join("run/team-session.json");
        let client = |expect: &str| {
            let c = TeamClient::new(
                local(&server),
                Some(expect.to_owned()),
                token.clone(),
                Some(cache.clone()),
            )
            .unwrap();
            *c.cookie.lock().unwrap() = c.cached_cookie();
            c
        };
        let fp = testserver::fingerprint();
        let pong: Option<String> = client(&fp).get_json("/api/ping").unwrap();
        assert_eq!(pong.as_deref(), Some("pong"));
        assert_eq!(mode(&cache), 0o600);
        // A new process reuses the cookie: no second login.
        let again: Option<String> = client(&fp).get_json("/api/ping").unwrap();
        assert_eq!(again.as_deref(), Some("pong"));
        assert_eq!(team.logins(), 1);
        let missing: Option<String> = client(&fp).get_json("/api/nothing").unwrap();
        assert_eq!(missing, None);

        // The server forgot the session (revoked, expired): log in again, once.
        team.sessions.lock().unwrap().clear();
        let pong: Option<String> = client(&fp).get_json("/api/ping").unwrap();
        assert_eq!(pong.as_deref(), Some("pong"));
        assert_eq!(team.logins(), 2);

        // A cache written for another pin is ignored.
        let other = cinelog_web::tls::fingerprint(b"x");
        assert_eq!(client(&other).cached_cookie(), None);
        assert!(client(&fp).cached_cookie().is_some());

        let reqs = team.requests.lock().unwrap();
        let ua: Vec<_> = reqs.iter().filter_map(|r| r.header("user-agent")).collect();
        assert!(
            ua.iter().all(|u| *u == ua[0]),
            "one User-Agent: the session is bound to it"
        );
        assert!(reqs.iter().all(|r| r.header("cookie").is_none() || r.method == "GET"));
    }

    #[test]
    fn errors_carry_the_api_message() {
        let (token, _) = tokens::generate();
        let team = FakeTeam::new(&token);
        let server = team.serve(|req| match req.path() {
            "/api/busy" => Some(Reply {
                headers: vec![("Retry-After".into(), "42".into())],
                ..Reply::error(429, wire::ErrorCode::TooManyRequests, "slow down")
            }),
            "/api/chunked" => Some(Reply {
                chunked: true,
                ..Reply::json(200, &serde_json::json!({"a": "long enough to span chunks"}))
            }),
            _ => None,
        });
        let c = TeamClient::new(local(&server), Some(testserver::fingerprint()), token, None).unwrap();
        let err = c.get_json::<serde_json::Value>("/api/busy").unwrap_err().to_string();
        assert!(
            err.contains("slow down (429)") && err.contains("retry in 42 s"),
            "{err}"
        );
        let v: serde_json::Value = c.get_json("/api/chunked").unwrap().unwrap();
        assert_eq!(v["a"], "long enough to span chunks");

        let (_reserved, port) = refusing_port();
        let gone = TeamClient::new(
            BaseUrl {
                host: "127.0.0.1".into(),
                port,
            },
            Some(testserver::fingerprint()),
            c.token.clone(),
            None,
        )
        .unwrap();
        let err = gone.get_json::<serde_json::Value>("/api/me").unwrap_err().to_string();
        assert!(err.contains("cannot reach"), "{err}");
    }

    /// M3 accept 3 (non-root half): two named member tokens log in to the real team server
    /// as two principals and see the same indexed team history; revoking one ends only its
    /// web sessions, including one stored by a login that raced the revoke.
    #[test]
    fn two_members_share_the_real_team_server() {
        use crate::serve::testkit::{UUID_A, cmd_json, done_json, write_cast};
        use cinelog_index::schema::{self, Access, DbKind};

        let dir = tempfile::tempdir().unwrap();
        let ctx = team_ctx(dir.path());
        for d in [
            ctx.paths.auth_file.parent().unwrap(),
            &ctx.paths.db_dir,
            &ctx.paths.runtime_dir,
            &ctx.paths.tls_dir,
        ] {
            fs::create_dir_all(d).unwrap();
        }
        let (cmd, done) = (cmd_json(1, 7, 1, "grep needle /etc/hosts"), done_json(1, 7, 1, 0));
        write_cast(
            &ctx.paths.logs_root.join("alice/2026-11"),
            &format!("teamhost_{UUID_A}.cast"),
            UUID_A,
            1,
            &[
                (0.5, "m", &cmd),
                (0.6, "o", "a needle in the output\r\n"),
                (0.7, "m", &done),
            ],
            Some(0),
        );
        let indexed = crate::serve::open_engine(&ctx)
            .unwrap()
            .index_once(&cinelog_index::model::OnceOptions::default())
            .unwrap();
        assert_eq!((indexed.indexed, indexed.commands), (1, 1), "{indexed:?}");

        let new = |name: &str| {
            let (r, out) = run_action(
                &ctx,
                TokenAction::New {
                    name: Some(name.into()),
                },
            );
            assert_eq!(r.unwrap(), 0);
            secret_of(&out)
        };
        let (alice, bob) = (new("alice"), new("bob"));

        let mut web = ctx.web_context();
        web.config.server.bind = vec!["127.0.0.1:0".into()];
        let server = cinelog_web::spawn(web).unwrap();
        let url = BaseUrl {
            host: "127.0.0.1".into(),
            port: server.info().port,
        };
        let fp = server.info().fingerprint.clone();

        let (probed, info) = probe(&url).unwrap();
        assert_eq!((probed.as_str(), info.scope), (fp.as_str(), wire::Scope::Team));
        let (_, a_me) = login_check(url.clone(), fp.clone(), &alice).unwrap();
        let (_, b_me) = login_check(url.clone(), fp.clone(), &bob).unwrap();
        assert_eq!((a_me.principal.as_str(), b_me.principal.as_str()), ("alice", "bob"));
        assert_eq!(a_me.scope, wire::Scope::Team);

        let member = |token: &str| {
            let c = TeamClient::new(url.clone(), Some(fp.clone()), token.to_owned(), None).unwrap();
            let cookie = c.login().unwrap();
            (c, cookie)
        };
        let ((ca, a_cookie), (cb, b_cookie)) = (member(&alice), member(&bob));
        let both = |target: &str| {
            let a = ca.send("GET", target, None, Some(&a_cookie)).unwrap();
            let b = cb.send("GET", target, None, Some(&b_cookie)).unwrap();
            assert_eq!(a.status, 200, "{target}: {}", String::from_utf8_lossy(&a.body));
            assert_eq!((b.status, &b.body), (a.status, &a.body), "{target}");
            a.body
        };
        let sessions: wire::SessionPage = serde_json::from_slice(&both("/api/sessions")).unwrap();
        let uuids: Vec<&str> = sessions.sessions.iter().map(|s| s.uuid.as_str()).collect();
        assert_eq!(uuids, [UUID_A]);
        let hits: wire::SearchResponse = serde_json::from_slice(&both("/api/search?q=needle&in=both")).unwrap();
        assert!(!hits.hits.is_empty() && hits.hits.iter().all(|h| h.session == UUID_A));
        let status: wire::Status = serde_json::from_slice(&both("/api/status")).unwrap();
        assert_eq!((status.scope, status.commands), (wire::Scope::Team, 1));

        // A login of bob that passed the token check before the rewrite and stores its
        // session while the revoke runs: modelled as a session row inserted 300 ms in.
        let meta = DbPaths::in_dir(&ctx.paths.db_dir).meta;
        let conn = schema::open(&meta, DbKind::Meta, Access::ReadWrite).unwrap();
        conn.execute_batch(
            "CREATE TEMP TABLE late AS SELECT * FROM auth_sessions WHERE principal = 'bob' LIMIT 1;
             UPDATE late SET id_hash = randomblob(32);",
        )
        .unwrap();
        let racer = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            conn.execute_batch("INSERT INTO auth_sessions SELECT * FROM late")
                .unwrap();
            conn
        });
        let (r, out) = run_action(&ctx, TokenAction::Revoke { name: "bob".into() });
        let conn = racer.join().unwrap();
        assert_eq!(r.unwrap(), 0);
        assert!(out.contains("revoked bob; ended 3 web session(s)"), "{out}");
        let left: i64 = conn
            .query_row("SELECT count(*) FROM auth_sessions WHERE principal = 'bob'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(left, 0, "the late session was ended too");

        let me_status = |c: &TeamClient, cookie: &str| c.send("GET", "/api/me", None, Some(cookie)).unwrap().status;
        assert_eq!(me_status(&cb, &b_cookie), 401);
        assert_eq!(me_status(&ca, &a_cookie), 200);
        assert!(
            login_check(url, fp, &bob).is_err(),
            "the revoked token no longer logs in"
        );
        server.shutdown();
    }

    const NOECHO_TTY_ENV: &str = "CINELOG_TEST_NOECHO_TTY";

    fn echo_on(tty: &File) -> bool {
        nix::sys::termios::tcgetattr(tty)
            .unwrap()
            .local_flags
            .contains(nix::sys::termios::LocalFlags::ECHO)
    }

    /// The hidden token prompt never leaves the terminal without echo: not when it returns,
    /// and not when Ctrl-C (SIGINT) ends the process at the prompt.
    #[test]
    fn echo_comes_back_after_the_prompt_and_after_a_signal() {
        use std::os::unix::process::ExitStatusExt;
        let pty = nix::pty::openpty(None, None).unwrap();
        let _master = pty.master;
        let tty = File::from(pty.slave);
        assert!(echo_on(&tty));
        {
            let _quiet = NoEcho::new(&tty).unwrap();
            assert!(!echo_on(&tty));
        }
        assert!(echo_on(&tty));

        let path = nix::unistd::ttyname(&tty).unwrap();
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "token::tests::no_echo_child",
                "--ignored",
                "--test-threads=1",
                "-q",
            ])
            .env(NOECHO_TTY_ENV, &path)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap();
        assert_eq!(status.signal(), Some(libc::SIGINT), "{status:?}");
        assert!(echo_on(&tty), "the signal handler restored echo");
    }

    /// The child of the test above: turns echo off on the given terminal, then is
    /// interrupted.
    #[test]
    #[ignore = "run by echo_comes_back_after_the_prompt_and_after_a_signal"]
    fn no_echo_child() {
        let Some(path) = std::env::var_os(NOECHO_TTY_ENV) else {
            return;
        };
        let tty = OpenOptions::new().read(true).write(true).open(path).unwrap();
        let _quiet = NoEcho::new(&tty).unwrap();
        assert!(!echo_on(&tty));
        nix::sys::signal::raise(nix::sys::signal::Signal::SIGINT).unwrap();
        std::thread::sleep(Duration::from_secs(5));
        std::process::exit(3);
    }

    /// A loopback port that refuses connections for as long as the returned socket lives:
    /// bound but not listening, so no parallel test can take it over.
    fn refusing_port() -> (std::os::fd::OwnedFd, u16) {
        use std::os::fd::FromRawFd;
        // SAFETY: plain socket calls on a fresh fd that the OwnedFd then owns.
        unsafe {
            let fd = libc::socket(libc::AF_INET, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0);
            assert!(fd >= 0);
            let owned = std::os::fd::OwnedFd::from_raw_fd(fd);
            let mut addr: libc::sockaddr_in = std::mem::zeroed();
            addr.sin_family = libc::AF_INET as libc::sa_family_t;
            addr.sin_addr.s_addr = u32::from(std::net::Ipv4Addr::LOCALHOST).to_be();
            let len = std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t;
            assert_eq!(libc::bind(fd, (&raw const addr).cast(), len), 0);
            let mut got = len;
            assert_eq!(libc::getsockname(fd, (&raw mut addr).cast(), &mut got), 0);
            (owned, u16::from_be(addr.sin_port))
        }
    }
}
