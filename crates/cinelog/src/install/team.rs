//! `cinelog install --team` (§5.6, §9.7, §10.3, §10.4, §10.6 step 6): the verified binary
//! in `/usr/local/bin`, the root-owned plugin copy, the system zshrc block, `/etc/cinelog`
//! (with `tls/`), `/var/lib/cinelog`, the log root and the runtime dir; then the team server:
//! the hardened system unit `packaging/systemd/cinelog-team.service`, tmpfiles.d, stopping
//! root's old Python viewer, and the first named web token for `$SUDO_USER`.
//!
//! With `--prefix DIR` every system path lives below DIR and no root privileges are needed;
//! that is the test mode: it writes the same files but never runs `systemctl` and never
//! looks at running processes. Without it the command must run as root and refuses to write
//! into any directory that is not root-owned and free of group/world write along its whole
//! path. Modes are only tightened on directories cinelog owns; system directories such as
//! `/usr/local/bin` are checked, never changed.

use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use cinelog_core::config::legacy;
use cinelog_core::location;
use cinelog_core::paths::{Paths, SystemRoot, team};
use cinelog_core::server::ServerInfo;
use cinelog_core::{Config, Scope};
use cinelog_index::model::{DbPaths, settings_keys};
use cinelog_web::hosts::HostAllowlist;
use cinelog_web::server::BindSpec;
use cinelog_web::tls::{self, TlsOptions};
use sha2::{Digest, Sha256};

use super::fsutil::{self, Outcome};
use super::{Steps, migrate, old_server, restart, zshrc};
use crate::module;

/// `packaging/tmpfiles.d/cinelog.conf` → `/etc/tmpfiles.d/cinelog.conf`.
pub const TMPFILES_CONF: &str = include_str!("../../../../packaging/tmpfiles.d/cinelog.conf");
/// `packaging/systemd/cinelog-team.service` → `/etc/systemd/system/` (see [`render_unit`]).
pub const UNIT: &str = include_str!("../../../../packaging/systemd/cinelog-team.service");
pub const UNIT_NAME: &str = "cinelog-team.service";
const UNIT_LOGS_LINE: &str = "BindPaths=/root/logs\n";
const OLD_SERVER_GRACE: Duration = Duration::from_secs(3);

/// The binary `install --team` copies to `/usr/local/bin/cinelog`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceBinary {
    /// What is read (`/proc/self/exe` for the running image).
    pub read: PathBuf,
    /// The file it is, for messages and the same-file check.
    pub path: PathBuf,
    /// Version whose `shell/checksums/v<version>.sha256` must list it.
    pub version: String,
}

/// Picks the binary to install: `--binary`, else the module's `bin/cinelog`, else the
/// running image when it lives inside the module (a build in a repository checkout).
///
/// The running binary is not the default: under `sudo` a plain `cinelog` is usually the
/// installed team binary itself, and "upgrading" it with its own bytes would leave the old
/// binary next to a new plugin copy.
pub fn source_binary(explicit: Option<&Path>, module: &Path, running: &Path) -> Result<SourceBinary> {
    let module_version = || module::version(module).unwrap_or_else(|| cinelog_core::VERSION.to_owned());
    if let Some(p) = explicit {
        return Ok(SourceBinary {
            read: p.to_path_buf(),
            path: p.to_path_buf(),
            version: module_version(),
        });
    }
    let bundled = module.join(module::BINARY);
    if bundled.is_file() {
        return Ok(SourceBinary {
            read: bundled.clone(),
            path: bundled,
            version: module_version(),
        });
    }
    let canonical_module = std::fs::canonicalize(module).unwrap_or_else(|_| module.to_path_buf());
    if running.starts_with(&canonical_module) || running.starts_with(module) {
        return Ok(SourceBinary {
            // The running image itself, even if its file was replaced or deleted meanwhile.
            read: PathBuf::from("/proc/self/exe"),
            path: running.to_path_buf(),
            version: cinelog_core::VERSION.to_owned(),
        });
    }
    bail!(
        "{} has no {} and the running binary {} is not part of it: pass --binary PATH",
        module.display(),
        module::BINARY,
        running.display()
    )
}

/// Who must own every existing component of the paths a real install writes to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Trust {
    pub uid: u32,
    /// The checks walk up to and including this directory.
    pub stop: PathBuf,
}

impl Trust {
    /// Root up to `/` for a real install; nothing for a `--prefix` test install.
    pub fn for_root(root: &SystemRoot) -> Option<Self> {
        root.is_real().then(|| Self {
            uid: 0,
            stop: PathBuf::from("/"),
        })
    }
}

/// Where a team install or uninstall writes, and the ownership rules there.
#[derive(Debug, Clone)]
pub struct System {
    pub root: SystemRoot,
    /// Ownership rules for existing paths (see [`Trust::for_root`]).
    pub trust: Option<Trust>,
    /// Effective uid of the installer.
    pub euid: u32,
}

/// What `install --team` installs, from where.
#[derive(Debug, Clone)]
pub struct Options {
    pub sys: System,
    /// Module checkout to install the plugin from (and to verify the binary against).
    pub module: PathBuf,
    pub binary: SourceBinary,
    pub system_zshrc: bool,
    pub upgrade: bool,
    pub allow_unverified: bool,
    pub migrate: bool,
}

/// The system manager, as far as the team install drives it.
pub trait Systemd {
    fn is_active(&self, unit: &str) -> bool;
    /// Runs `systemctl ARGS`; failures are reported into `steps`, never fatal. Whether it
    /// succeeded.
    fn run(&self, args: &[&str], steps: &mut Steps) -> bool;
}

/// `systemctl` of the running system.
pub struct SystemSystemd;

impl Systemd for SystemSystemd {
    fn is_active(&self, unit: &str) -> bool {
        std::process::Command::new("systemctl")
            .args(["is-active", "--quiet", unit])
            .status()
            .is_ok_and(|s| s.success())
    }

    fn run(&self, args: &[&str], steps: &mut Steps) -> bool {
        super::systemctl(args, steps)
    }
}

/// Whether systemd is the running init (what `sd_booted()` checks): a unit written on a host
/// with another init (OpenRC, a container, WSL without systemd) is never started.
pub fn systemd_booted() -> bool {
    Path::new("/run/systemd/system").is_dir()
}

/// What the install learns from and does to the running system besides files. A
/// `--prefix` install has none of it: no systemd, no process scan.
pub struct Host<'a> {
    /// The system manager; `None` for a `--prefix` install or a host not booted with systemd.
    pub systemd: Option<&'a dyn Systemd>,
    /// `/proc` to look for root's old Python viewer in.
    pub proc_root: Option<PathBuf>,
    /// `$SUDO_USER`: the member who gets the first web token.
    pub sudo_user: Option<String>,
    pub now: f64,
}

impl Host<'static> {
    /// The running system for a real install; only `$SUDO_USER` for a `--prefix` one.
    pub fn current(root: &SystemRoot) -> Self {
        Self::detect(root, systemd_booted())
    }

    fn detect(root: &SystemRoot, systemd_booted: bool) -> Self {
        let real = root.is_real();
        Self {
            systemd: (real && systemd_booted).then_some(&SystemSystemd as &dyn Systemd),
            proc_root: real.then(old_server::proc_root),
            sudo_user: std::env::var("SUDO_USER").ok().filter(|u| !u.is_empty()),
            now: cinelog_web::now(),
        }
    }
}

/// A real team install needs root; a `--prefix` one does not.
pub fn check_privileges(root: &SystemRoot, euid: u32) -> Result<()> {
    if root.is_real() && euid != 0 {
        bail!("the team install writes system paths: run it with sudo (or use --prefix DIR to test)");
    }
    Ok(())
}

impl System {
    pub fn new(root: SystemRoot, euid: u32) -> Self {
        Self {
            trust: Trust::for_root(&root),
            root,
            euid,
        }
    }

    fn check_privileges(&self) -> Result<()> {
        check_privileges(&self.root, self.euid)
    }

    /// Real installs: `path` and all its parents must be root-owned and not g/o-writable.
    fn require_secure(&self, path: &Path) -> Result<()> {
        let Some(trust) = &self.trust else {
            return Ok(());
        };
        match location::insecure_component(path, trust.uid, &trust.stop) {
            Some(bad) => bail!(
                "refusing to use {}: {}: {} (root must only use root-owned paths)",
                path.display(),
                bad.path.display(),
                bad.problem
            ),
            None => Ok(()),
        }
    }

    /// Prepares a directory before anything is written into it. In real mode everything
    /// that already exists on its path is checked first; an existing [`DirKind::Owned`]
    /// directory only needs the right owner (its mode is tightened afterwards). A
    /// [`DirKind::System`] directory is never chmod'ed.
    fn prepare_dir(&self, path: &Path, mode: u32, kind: DirKind) -> Result<Outcome> {
        let existing = nearest_existing(path);
        if let Some(trust) = &self.trust {
            match (kind, existing == path) {
                (DirKind::Owned, true) => {
                    let meta = std::fs::symlink_metadata(path)?;
                    if meta.file_type().is_symlink() || meta.uid() != trust.uid {
                        bail!(
                            "refusing to use {}: it must be a directory owned by uid {}, not a symlink",
                            path.display(),
                            trust.uid
                        );
                    }
                    self.require_secure(path.parent().unwrap_or(Path::new("/")))?;
                }
                _ => self.require_secure(existing).with_context(|| {
                    format!(
                        "fix the owner or mode of {} first; cinelog does not change system directories",
                        existing.display()
                    )
                })?,
            }
        }
        let outcome = match kind {
            DirKind::System if existing == path => {
                if !std::fs::symlink_metadata(path)?.is_dir() {
                    bail!("{} exists and is not a directory", path.display());
                }
                Outcome::Unchanged
            }
            _ => fsutil::ensure_dir(path, mode)?,
        };
        self.require_secure(path)?;
        Ok(outcome)
    }
}

/// Whether cinelog may set a directory's mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DirKind {
    /// Created and owned by cinelog (`/etc/cinelog`, `/var/lib/cinelog`, the log root, ...).
    Owned,
    /// Belongs to the system (`/usr/local/bin`, `/etc/zsh`): created if missing, else
    /// only checked.
    System,
}

/// `path` or its nearest ancestor that exists (as anything, symlinks included).
fn nearest_existing(path: &Path) -> &Path {
    path.ancestors()
        .find(|p| std::fs::symlink_metadata(p).is_ok())
        .unwrap_or(Path::new("/"))
}

/// TEAM paths per the installed `team.toml` (defaults while it is unreadable).
fn team_paths(root: &SystemRoot) -> Paths {
    let cfg = Config::load(&root.join(team::CONFIG)).unwrap_or_default();
    Paths::team(root, &cfg.paths, Path::new("/root"))
}

/// Messages show system paths in full.
fn no_home() -> &'static Path {
    Path::new("/")
}

/// How the binary relates to `shell/checksums/v<VERSION>.sha256` of the module.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verification {
    /// Listed under this asset name.
    Listed(String),
    NotListed(PathBuf),
    NoChecksumFile(PathBuf),
}

/// Looks the sha256 of `bytes` up in the module's checksum file (sha256sum format).
pub fn verify(module: &Path, version: &str, bytes: &[u8]) -> Verification {
    let file = module.join(format!("shell/checksums/v{version}.sha256"));
    let Ok(text) = std::fs::read_to_string(&file) else {
        return Verification::NoChecksumFile(file);
    };
    let digest = data_encoding::HEXLOWER.encode(&Sha256::digest(bytes));
    text.lines()
        .filter_map(|l| {
            let mut it = l.split_whitespace();
            Some((it.next()?, it.next()?.trim_start_matches('*')))
        })
        .find(|(hash, _)| hash.eq_ignore_ascii_case(&digest))
        .map_or(Verification::NotListed(file), |(_, name)| {
            Verification::Listed(name.to_owned())
        })
}

pub fn install(opts: &Options, steps: &mut Steps) -> Result<()> {
    install_on(opts, &Host::current(&opts.sys.root), steps)
}

pub fn install_on(opts: &Options, host: &Host<'_>, steps: &mut Steps) -> Result<()> {
    let sys = &opts.sys;
    sys.check_privileges()?;
    let root = &sys.root;
    let home = no_home();
    let binary = check_binary(opts, steps)?;

    for (dir, mode, kind, what) in [
        (team::CONFIG_DIR, 0o700, DirKind::Owned, "team config"),
        (team::TLS_DIR, 0o700, DirKind::Owned, "team server certificate"),
        (team::STATE_DIR, 0o700, DirKind::Owned, "team databases"),
        (team::RUNTIME_DIR, 0o700, DirKind::Owned, "team registry"),
        ("/etc/tmpfiles.d", 0o755, DirKind::System, ""),
        ("/etc/systemd/system", 0o755, DirKind::System, ""),
        ("/usr/local/bin", 0o755, DirKind::System, ""),
        (team::SHARE_DIR, 0o755, DirKind::Owned, "plugin copy"),
    ] {
        let path = root.join(dir);
        let outcome = sys.prepare_dir(&path, mode, kind)?;
        if !what.is_empty() {
            steps.dir(&outcome, &path, what, home);
        }
    }

    let config_file = root.join(team::CONFIG);
    let legacy_file = opts.migrate.then(|| legacy::path_in_home(&root.join("/root")));
    let cfg = migrate::ensure_config(
        Scope::Team,
        &config_file,
        legacy_file.as_deref(),
        fsutil::Symlinks::Refuse,
    )?;
    steps.notes(cfg.notes);
    sys.require_secure(&config_file)?;

    let team_cfg = Config::load(&config_file).unwrap_or_default();
    let paths = team_paths(root);
    if let Some(problem) = location::db_location_problem(&paths.db_dir) {
        bail!("{problem}");
    }
    if let Some(port) = cfg.legacy_port {
        let file = paths.migrated_port_file();
        if fsutil::write_new(&file, format!("{port}\n").as_bytes(), 0o600)? == Outcome::Created {
            steps.note(format!("kept root's old viewer port {port} for the future team server"));
        }
    }
    let outcome = sys.prepare_dir(&paths.logs_root, 0o700, DirKind::Owned)?;
    steps.dir(&outcome, &paths.logs_root, "team casts", home);

    let tmpfiles = root.join(team::TMPFILES);
    let outcome = fsutil::write_atomic(&tmpfiles, TMPFILES_CONF.as_bytes(), 0o644)?;
    sys.require_secure(&tmpfiles)?;
    steps.file(&outcome, &tmpfiles, "(recreates /run/cinelog at boot)", home);

    let binary_changed = write_binary(opts, &binary, steps)?;
    install_plugin(opts, steps)?;

    let rc = zshrc::system_zshrc(root);
    if opts.system_zshrc {
        add_zshrc_block(opts, &rc, steps)?;
    } else if !std::fs::read_to_string(&rc).is_ok_and(|t| zshrc::has_block(&t)) {
        steps.warn(format!(
            "{} has no cinelog block: add --system-zshrc so `sudo -i`/`sudo -s` record without root's own zim",
            rc.display()
        ));
    }

    stop_old_server(host, cfg.legacy_port, &paths, steps);
    let cert = certificate(&team_cfg, &paths, steps);
    let unit_text = render_unit(&unit_logs_root(&team_cfg))?;
    let restart = Restart {
        binary: binary_changed,
        certificate: cert.as_ref().is_some_and(|c| c.regenerated),
    };
    install_unit(opts, host, &unit_text, restart, steps)?;
    restart_detached(opts, host, &paths, steps);
    member_token(&paths, host, steps)?;

    steps.note("recommended: `Defaults always_set_home` in sudoers or `sudo -i`, so root never runs user dotfiles");
    let url = team_url(&team_cfg, &paths, cfg.legacy_port);
    steps.note(format!(
        "the team web UI is {url} (browsers send cookies to every port of a host: give the team server a \
         hostname of its own with server.hostnames in team.toml)"
    ));
    let fingerprint = cert.map_or_else(|| "<`sudo cinelog tls show`>".to_owned(), |c| c.fingerprint);
    steps.note(format!(
        "more members: `sudo cinelog token new NAME`; for `search --team` each member runs \
         `cinelog token team-login --url {url} --fingerprint {fingerprint}`"
    ));
    Ok(())
}

/// The URL members reach the team server at: the running server's, else what it will bind
/// (a port in `server.bind`, the port it persisted, root's old viewer port, else 10000).
/// Members cannot read `/run/cinelog/server.json`, so the install prints it.
fn team_url(cfg: &Config, paths: &Paths, legacy_port: Option<u16>) -> String {
    if let Some(live) = ServerInfo::read_live(&paths.runtime_dir) {
        return live.url;
    }
    let bound = cfg.server.bind.iter().find_map(|b| b.parse::<BindSpec>().ok()?.port);
    let port = bound
        .or_else(|| persisted_port(&DbPaths::in_dir(&paths.db_dir).meta))
        .or_else(|| old_server::persisted_port(legacy_port, &paths.migrated_port_file()))
        .unwrap_or(cinelog_core::server::BASE_PORT);
    let host = cfg
        .server
        .hostnames
        .first()
        .cloned()
        .unwrap_or_else(cinelog_core::paths::hostname);
    let host = if host.contains(':') { format!("[{host}]") } else { host };
    format!("https://{host}:{port}")
}

/// The port the team server persisted in meta.db (read-only; `None` without one).
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

/// The log root as the server sees it (unprefixed: the unit is for the real system).
fn unit_logs_root(cfg: &Config) -> PathBuf {
    Paths::team(&SystemRoot::real(), &cfg.paths, Path::new("/root")).logs_root
}

/// The packaged unit, with `BindPaths=` set to the configured log root. Paths systemd
/// would split or expand are refused.
pub fn render_unit(logs_root: &Path) -> Result<String> {
    let logs = logs_root.to_str().context("the team log root is not valid UTF-8")?;
    if !logs.starts_with('/')
        || logs
            .chars()
            .any(|c| c.is_whitespace() || c.is_control() || matches!(c, '%' | ':' | '"' | '\'' | '\\' | '$'))
    {
        bail!(
            "paths.logs = {logs:?} cannot be put into the systemd unit (BindPaths=): use an absolute path \
             without spaces, ':', '%', quotes, '$' or backslashes"
        );
    }
    debug_assert!(UNIT.contains(UNIT_LOGS_LINE));
    Ok(UNIT.replacen(UNIT_LOGS_LINE, &format!("BindPaths={logs}\n"), 1))
}

/// What changed under a running server, besides its unit.
#[derive(Debug, Clone, Copy, Default)]
struct Restart {
    binary: bool,
    /// The server loads its certificate once, at start.
    certificate: bool,
}

/// Writes the unit, then (real installs) reloads systemd, enables and starts the server,
/// and restarts it when it was running with an older binary, unit or certificate.
fn install_unit(opts: &Options, host: &Host<'_>, text: &str, changed: Restart, steps: &mut Steps) -> Result<()> {
    let unit = opts.sys.root.join(team::UNIT);
    let outcome = fsutil::write_atomic(&unit, text.as_bytes(), 0o644)?;
    opts.sys.require_secure(&unit)?;
    steps.file(&outcome, &unit, "(hardened team server unit)", no_home());
    let Some(systemd) = host.systemd else {
        if opts.sys.root.is_real() {
            steps.note(format!(
                "systemd is not running: {UNIT_NAME} was not enabled or started (root's recorded shells start \
                 `cinelog serve` themselves)"
            ));
        } else {
            steps.note(format!("test install: {UNIT_NAME} was not enabled or started"));
        }
        return Ok(());
    };
    let was_active = systemd.is_active(UNIT_NAME);
    let unit_changed = outcome != Outcome::Unchanged;
    if unit_changed {
        systemd.run(&["daemon-reload"], steps);
    }
    systemd.run(&["enable", "--now", UNIT_NAME], steps);
    if was_active && (unit_changed || changed.binary || changed.certificate) {
        // Only the server restarts; running recorders keep their (deleted) binary.
        systemd.run(&["try-restart", UNIT_NAME], steps);
    }
    Ok(())
}

/// A team server that root's shells started without the unit (`serve --ensure` on a host
/// without systemd, or before the unit existed) still runs the old binary after an upgrade:
/// it is stopped and the unit takes over; without systemd, or when the unit does not start,
/// a detached server.
fn restart_detached(opts: &Options, host: &Host<'_>, paths: &Paths, steps: &mut Steps) {
    if host.proc_root.is_none() {
        return;
    }
    let exe = opts.sys.root.join(team::BINARY);
    let mut start_unit = |_: &[String], steps: &mut Steps| match host.systemd {
        Some(systemd) => systemd.run(&["start", UNIT_NAME], steps),
        None => false,
    };
    let start = if host.systemd.is_some() {
        restart::Start::With(&mut start_unit)
    } else {
        restart::Start::Detached
    };
    let unit = restart::Unit {
        name: UNIT_NAME,
        user: false,
    };
    restart::restart_stale(&paths.runtime_dir, &exe, opts.sys.euid, unit, start, steps);
}

/// Stops root's old Python viewer (`/root/.zim/modules/cinelog/http_server.py`) and checks
/// that nothing else holds the port it used.
fn stop_old_server(host: &Host<'_>, legacy_port: Option<u16>, paths: &Paths, steps: &mut Steps) {
    let Some(proc_root) = &host.proc_root else {
        return;
    };
    let pids = old_server::find(proc_root, 0, Path::new("/root"));
    if !pids.is_empty() {
        let list = |p: &[i32]| p.iter().map(i32::to_string).collect::<Vec<_>>().join(", ");
        let left = old_server::stop(&pids, OLD_SERVER_GRACE);
        if left.is_empty() {
            steps.done(format!(
                "stopped root's old viewer (http_server.py, pid {})",
                list(&pids)
            ));
        } else {
            steps.warn(format!(
                "could not stop root's old viewer (http_server.py, pid {})",
                list(&left)
            ));
        }
    }
    if let Some(port) = old_server::persisted_port(legacy_port, &paths.migrated_port_file())
        && old_server::port_listened(proc_root, port)
    {
        steps.warn(format!(
            "something still listens on port {port}, root's viewer port from the old install"
        ));
    }
}

/// The certificate the team server serves after the install.
#[derive(Debug, Clone)]
struct Certificate {
    fingerprint: String,
    /// Created or replaced just now (missing, expiring, or no longer covering the names).
    regenerated: bool,
}

/// Creates the server certificate now (or checks the existing one, as the server would on
/// start, regenerating it under the same rules), so the install can print the fingerprint
/// that members pin with `cinelog token team-login`. A failure only warns: the server
/// creates the certificate itself on its first start.
fn certificate(cfg: &Config, paths: &Paths, steps: &mut Steps) -> Option<Certificate> {
    let web = cinelog_web::WebContext {
        scope: Scope::Team,
        host: cinelog_core::paths::hostname(),
        user: "root".into(),
        uid: 0,
        config: cfg.clone(),
        paths: paths.clone(),
        db: DbPaths::in_dir(&paths.db_dir),
    };
    let opts = TlsOptions::from_context(&web, HostAllowlist::new(&web).cert_names());
    tls::install_provider();
    match tls::load_or_create(&opts) {
        Ok(m) => {
            let origin = match (m.info.self_signed, m.regenerated) {
                (false, _) => "configured",
                (true, Some(_)) => "new self-signed",
                (true, None) => "self-signed",
            };
            steps.done(format!(
                "team server certificate ({origin}, {}): fingerprint {}",
                m.info.cert_path.display(),
                m.info.fingerprint
            ));
            Some(Certificate {
                fingerprint: m.info.fingerprint,
                regenerated: m.regenerated.is_some(),
            })
        }
        Err(e) => {
            steps.warn(format!(
                "could not prepare the team server certificate ({e}); the server creates one on start, \
                 see `sudo cinelog tls show`"
            ));
            None
        }
    }
}

/// The first named web token, for the member who runs the install (shown once).
fn member_token(paths: &Paths, host: &Host<'_>, steps: &mut Steps) -> Result<()> {
    let Some(user) = host.sudo_user.as_deref().filter(|u| *u != "root") else {
        steps.note("no $SUDO_USER: create member tokens with `sudo cinelog token new NAME`");
        return Ok(());
    };
    if !cinelog_web::tokens::valid_name(user) {
        steps.warn(format!(
            "$SUDO_USER {user:?} is not a valid token name: create one with `sudo cinelog token new NAME`"
        ));
        return Ok(());
    }
    match crate::token::ensure_named(&paths.auth_file, user, host.now)? {
        Some(t) => steps.done(format!("team web token for {} (shown once): {}", t.name, t.secret)),
        None => steps.note(format!("kept the team web token of {user}")),
    }
    Ok(())
}

/// Adds or updates the managed block. An existing rc keeps its mode and owner, and a
/// symlinked one (config management) is edited through the link, which stays.
fn add_zshrc_block(opts: &Options, rc: &Path, steps: &mut Steps) -> Result<()> {
    let block = system_block(opts);
    let detail = "(root shells source the plugin copy first)";
    let Some(target) = fsutil::rewrite_target(rc)? else {
        if let Some(dir) = rc.parent() {
            opts.sys.prepare_dir(dir, 0o755, DirKind::System)?;
        }
        let outcome = fsutil::write_atomic(rc, zshrc::upsert("", &block).as_bytes(), 0o644)?;
        steps.file(&outcome, rc, detail, no_home());
        return Ok(());
    };
    opts.sys.require_secure(&target)?;
    let old = std::fs::read_to_string(&target).with_context(|| format!("reading {}", target.display()))?;
    let outcome = fsutil::rewrite_preserving(&target, zshrc::upsert(&old, &block).as_bytes())?;
    steps.file(&outcome, rc, detail, no_home());
    if target != rc {
        steps.note(format!(
            "{} links to {}, which was edited",
            rc.display(),
            target.display()
        ));
    }
    Ok(())
}

/// The plugin's own `shell/system-zshrc.zsh` block when it has one, else the built-in one.
fn system_block(opts: &Options) -> String {
    let plugin = opts.sys.root.join(team::PLUGIN_COPY);
    std::fs::read_to_string(opts.module.join("shell/system-zshrc.zsh"))
        .ok()
        .and_then(|s| zshrc::block_from_snippet(&s, &plugin))
        .unwrap_or_else(|| zshrc::block(&plugin))
}

/// Reads and verifies the binary to install, before anything is written.
fn check_binary(opts: &Options, steps: &mut Steps) -> Result<Vec<u8>> {
    let src = &opts.binary;
    let target = opts.sys.root.join(team::BINARY);
    if opts.upgrade && same_file(&src.path, &target) {
        bail!(
            "{} is the installed team binary itself, so --upgrade would not change it; run the \
             new binary instead, e.g. `sudo {} install --team --upgrade`",
            target.display(),
            opts.module.join(module::BINARY).display()
        );
    }
    let bytes = std::fs::read(&src.read).with_context(|| format!("reading {}", src.path.display()))?;
    let version = src.version.as_str();
    let why = match verify(&opts.module, version, &bytes) {
        Verification::Listed(name) => {
            steps.done(format!("verified {} ({name}, v{version})", src.path.display()));
            None
        }
        Verification::NotListed(f) => Some(format!("its sha256 is not listed in {}", f.display())),
        Verification::NoChecksumFile(f) => Some(format!("{} does not exist", f.display())),
    };
    if let Some(why) = why {
        if !opts.allow_unverified {
            bail!(
                "cannot verify {}: {why}. Release binaries from `cinelog-fetch` verify; \
                 for a local build pass --allow-unverified",
                src.path.display()
            );
        }
        steps.warn(format!("installing an unverified binary: {why}"));
    }
    if let Ok(old) = std::fs::read(&target)
        && old != bytes
        && !opts.upgrade
    {
        bail!("{} exists and differs: pass --upgrade to replace it", target.display());
    }
    Ok(bytes)
}

fn same_file(a: &Path, b: &Path) -> bool {
    match (std::fs::metadata(a), std::fs::metadata(b)) {
        (Ok(x), Ok(y)) => x.dev() == y.dev() && x.ino() == y.ino(),
        _ => false,
    }
}

/// Whether the installed binary changed.
fn write_binary(opts: &Options, bytes: &[u8], steps: &mut Steps) -> Result<bool> {
    let target = opts.sys.root.join(team::BINARY);
    let outcome = fsutil::write_atomic(&target, bytes, 0o755)?;
    opts.sys.require_secure(&target)?;
    steps.file(
        &outcome,
        &target,
        &format!("(cinelog {} from {})", opts.binary.version, opts.binary.path.display()),
        no_home(),
    );
    Ok(outcome != Outcome::Unchanged)
}

/// The files of the module that make up the plugin copy, relative to the module.
pub fn plugin_files(module: &Path) -> Result<Vec<PathBuf>> {
    if !module.join(module::PLUGIN_FILE).is_file() {
        bail!("{} has no {}", module.display(), module::PLUGIN_FILE);
    }
    let mut out = vec![PathBuf::from(module::PLUGIN_FILE)];
    if module.join("VERSION").is_file() {
        out.push("VERSION".into());
    }
    let mut stack = vec![PathBuf::from("shell")];
    while let Some(rel) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(module.join(&rel)) else {
            continue;
        };
        for de in rd.filter_map(Result::ok) {
            let name = de.file_name();
            let name = name.to_string_lossy();
            let child = rel.join(&*name);
            let Ok(ft) = de.file_type() else { continue };
            // Download/verification tooling is for the user's own module only.
            if name.starts_with('.') || name == "checksums" || name.starts_with("fetch") {
                continue;
            }
            if ft.is_dir() {
                stack.push(child);
            } else if ft.is_file() {
                out.push(child);
            }
        }
    }
    out.sort();
    Ok(out)
}

fn install_plugin(opts: &Options, steps: &mut Steps) -> Result<()> {
    let share = opts.sys.root.join(team::SHARE_DIR);
    let files = plugin_files(&opts.module)?;
    let mut changed = 0;
    for rel in &files {
        let bytes = std::fs::read(opts.module.join(rel))?;
        let dest = share.join(rel);
        if let Some(dir) = dest.parent() {
            opts.sys.prepare_dir(dir, 0o755, DirKind::Owned)?;
        }
        if fsutil::write_atomic(&dest, &bytes, 0o644)? != Outcome::Unchanged {
            changed += 1;
        }
        opts.sys.require_secure(&dest)?;
    }
    let what = if changed == 0 {
        "up to date".to_owned()
    } else {
        format!("{changed} of {} file(s) updated", files.len())
    };
    steps.done(format!("plugin copy in {}: {what}", share.display()));
    Ok(())
}

/// `install --team --uninstall`: needs neither a module checkout nor a binary, so it also
/// works from the installed team binary and after rolling the module back.
pub fn uninstall(sys: &System, steps: &mut Steps) -> Result<()> {
    uninstall_on(sys, &Host::current(&sys.root), steps)
}

pub fn uninstall_on(sys: &System, host: &Host<'_>, steps: &mut Steps) -> Result<()> {
    sys.check_privileges()?;
    let root = &sys.root;
    let rc = zshrc::system_zshrc(root);
    if let Some(target) = fsutil::rewrite_target(&rc)?
        && let Ok(text) = std::fs::read_to_string(&target)
        && zshrc::has_block(&text)
    {
        sys.require_secure(&target)?;
        fsutil::rewrite_preserving(&target, zshrc::remove(&text).as_bytes())?;
        steps.done(format!("removed the cinelog block from {}", rc.display()));
    }
    let share = root.join(team::SHARE_DIR);
    if share.is_dir() {
        std::fs::remove_dir_all(&share).with_context(|| format!("removing {}", share.display()))?;
        steps.done(format!("removed {}", share.display()));
    }
    let unit = root.join(team::UNIT);
    if unit.exists() {
        if let Some(systemd) = host.systemd {
            systemd.run(&["disable", "--now", UNIT_NAME], steps);
        }
        fsutil::remove_file(&unit)?;
        steps.done(format!("removed {}", unit.display()));
        if let Some(systemd) = host.systemd {
            systemd.run(&["daemon-reload"], steps);
        }
    }
    let tmpfiles = root.join(team::TMPFILES);
    if fsutil::remove_file(&tmpfiles)? {
        steps.done(format!("removed {}", tmpfiles.display()));
    }
    let paths = team_paths(root);
    steps.note(format!(
        "kept {} (running recorders use it), {} (config, tokens, certificate), {} and the casts in {}",
        root.join(team::BINARY).display(),
        root.join(team::CONFIG_DIR).display(),
        paths.db_dir.display(),
        paths.logs_root.display()
    ));
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    struct Fixture {
        _dir: tempfile::TempDir,
        opts: Options,
    }

    fn fixture() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let module = dir.path().join("module");
        std::fs::create_dir_all(module.join("shell/checksums")).unwrap();
        std::fs::create_dir_all(module.join("shell/completions")).unwrap();
        std::fs::write(module.join(module::PLUGIN_FILE), "# plugin\n").unwrap();
        std::fs::write(module.join("VERSION"), format!("{}\n", cinelog_core::VERSION)).unwrap();
        std::fs::write(module.join("shell/early.zsh"), "# early\n").unwrap();
        std::fs::write(module.join("shell/fetch.zsh"), "# fetch\n").unwrap();
        std::fs::write(module.join("shell/completions/_cinelog"), "#compdef cinelog\n").unwrap();
        let exe = dir.path().join("cinelog-bin");
        std::fs::write(&exe, b"\x7fELF fake binary").unwrap();
        let opts = Options {
            sys: System {
                root: SystemRoot::prefixed(dir.path().join("sys")),
                trust: None,
                euid: nix::unistd::geteuid().as_raw(),
            },
            module,
            binary: SourceBinary {
                read: exe.clone(),
                path: exe,
                version: cinelog_core::VERSION.into(),
            },
            system_zshrc: true,
            upgrade: false,
            allow_unverified: false,
            migrate: true,
        };
        Fixture { _dir: dir, opts }
    }

    fn sums(opts: &Options, bytes: &[u8]) {
        sums_for(opts, cinelog_core::VERSION, bytes);
    }

    fn sums_for(opts: &Options, version: &str, bytes: &[u8]) {
        let digest = data_encoding::HEXLOWER.encode(&Sha256::digest(bytes));
        std::fs::write(
            opts.module.join(format!("shell/checksums/v{version}.sha256")),
            format!(
                "{}  cinelog-aarch64-unknown-linux-musl\n{digest}  cinelog-x86_64-unknown-linux-musl\n",
                "0".repeat(64)
            ),
        )
        .unwrap();
    }

    fn mode(p: &Path) -> u32 {
        std::fs::metadata(p).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn verification() {
        let f = fixture();
        let o = &f.opts;
        assert!(matches!(
            verify(&o.module, "0.10.0", b"x"),
            Verification::NoChecksumFile(_)
        ));
        sums(o, b"good");
        let v = cinelog_core::VERSION;
        assert_eq!(
            verify(&o.module, v, b"good"),
            Verification::Listed("cinelog-x86_64-unknown-linux-musl".into())
        );
        assert!(matches!(verify(&o.module, v, b"evil"), Verification::NotListed(_)));
    }

    #[test]
    fn unverified_binary_is_refused_unless_allowed() {
        let mut f = fixture();
        let err = install(&f.opts, &mut Steps::default()).unwrap_err();
        assert!(err.to_string().contains("--allow-unverified"), "{err}");
        assert!(!f.opts.sys.root.join(team::BINARY).exists());
        f.opts.allow_unverified = true;
        let mut steps = Steps::default();
        install(&f.opts, &mut steps).unwrap();
        assert!(steps.to_string().contains("unverified"));
    }

    #[test]
    fn full_install_upgrade_and_uninstall() {
        let mut f = fixture();
        let bytes = std::fs::read(&f.opts.binary.read).unwrap();
        sums(&f.opts, &bytes);
        let old = f.opts.sys.root.join("/root/.config/cinelog/cinelog-settings.conf");
        std::fs::create_dir_all(old.parent().unwrap()).unwrap();
        std::fs::write(&old, "SHOW_START_END_TIME=0\nexport CINELOG_VIEWER_PORT=10500\n").unwrap();
        let mut steps = Steps::default();
        install(&f.opts, &mut steps).unwrap();
        let r = &f.opts.sys.root;

        assert_eq!(std::fs::read(r.join(team::BINARY)).unwrap(), bytes);
        assert_eq!(mode(&r.join(team::BINARY)), 0o755);
        assert_eq!(mode(&r.join(team::CONFIG)), 0o600);
        assert_eq!(mode(&r.join(team::CONFIG_DIR)), 0o700);
        assert!(!Config::load(&r.join(team::CONFIG)).unwrap().shell.banners, "migrated");
        assert_eq!(mode(&r.join(team::STATE_DIR)), 0o700);
        let port = team_paths(r).migrated_port_file();
        assert_eq!(std::fs::read_to_string(&port).unwrap(), "10500\n");
        assert_eq!(mode(&port), 0o600);
        assert!(steps.to_string().contains("old viewer port 10500"), "{steps}");
        assert_eq!(mode(&r.join(team::RUNTIME_DIR)), 0o700);
        assert_eq!(mode(&r.join(team::LOGS)), 0o700);
        assert_eq!(std::fs::read_to_string(r.join(team::TMPFILES)).unwrap(), TMPFILES_CONF);
        assert_eq!(mode(&r.join(team::TLS_DIR)), 0o700);
        assert_eq!(std::fs::read_to_string(r.join(team::UNIT)).unwrap(), UNIT);
        assert_eq!(mode(&r.join(team::UNIT)), 0o644);
        assert!(steps.to_string().contains("was not enabled or started"), "{steps}");
        assert_eq!(
            std::fs::read_to_string(r.join(team::PLUGIN_COPY)).unwrap(),
            "# plugin\n"
        );
        let share = r.join(team::SHARE_DIR);
        assert!(share.join("shell/early.zsh").is_file());
        assert!(share.join("shell/completions/_cinelog").is_file());
        assert!(!share.join("shell/fetch.zsh").exists());
        assert!(!share.join("shell/checksums").exists());
        let rc = std::fs::read_to_string(r.join(team::SYSTEM_ZSHRC)).unwrap();
        assert!(rc.contains(&r.join(team::PLUGIN_COPY).display().to_string()));
        assert!(steps.to_string().contains("verified"));

        // A second run changes nothing; a different binary needs --upgrade.
        let mut again = Steps::default();
        install(&f.opts, &mut again).unwrap();
        assert!(again.to_string().contains("up to date"), "{again}");
        assert_eq!(std::fs::read_to_string(r.join(team::SYSTEM_ZSHRC)).unwrap(), rc);

        std::fs::write(&f.opts.binary.read, b"\x7fELF newer").unwrap();
        let newer = std::fs::read(&f.opts.binary.read).unwrap();
        sums(&f.opts, &newer);
        let err = install(&f.opts, &mut Steps::default()).unwrap_err();
        assert!(err.to_string().contains("--upgrade"));
        f.opts.upgrade = true;
        install(&f.opts, &mut Steps::default()).unwrap();
        assert_eq!(std::fs::read(r.join(team::BINARY)).unwrap(), newer);

        let mut un = Steps::default();
        uninstall(&f.opts.sys, &mut un).unwrap();
        assert!(!share.exists());
        assert!(!r.join(team::TMPFILES).exists());
        assert!(!zshrc::has_block(
            &std::fs::read_to_string(r.join(team::SYSTEM_ZSHRC)).unwrap()
        ));
        assert!(r.join(team::BINARY).exists() && r.join(team::CONFIG).exists());
    }

    /// Uninstall works from the installed binary alone: the module may be gone or rolled
    /// back to a release without `bin/cinelog`.
    #[test]
    fn uninstall_needs_no_module() {
        let mut f = fixture();
        f.opts.allow_unverified = true;
        install(&f.opts, &mut Steps::default()).unwrap();
        std::fs::remove_dir_all(&f.opts.module).unwrap();
        let sys = System {
            trust: None,
            ..System::new(f.opts.sys.root.clone(), f.opts.sys.euid)
        };
        let mut steps = Steps::default();
        uninstall(&sys, &mut steps).unwrap();
        let r = &sys.root;
        assert!(!r.join(team::SHARE_DIR).exists(), "{steps}");
        assert!(!zshrc::has_block(
            &std::fs::read_to_string(r.join(team::SYSTEM_ZSHRC)).unwrap()
        ));
    }

    #[test]
    fn existing_system_zshrc_content_is_preserved() {
        let mut f = fixture();
        f.opts.allow_unverified = true;
        let rc = f.opts.sys.root.join(team::SYSTEM_ZSHRC);
        std::fs::create_dir_all(rc.parent().unwrap()).unwrap();
        std::fs::write(&rc, "bindkey -e\n").unwrap();
        install(&f.opts, &mut Steps::default()).unwrap();
        let text = std::fs::read_to_string(&rc).unwrap();
        assert!(text.starts_with("bindkey -e\n\n# >>> cinelog >>>"));
        uninstall(&f.opts.sys, &mut Steps::default()).unwrap();
        assert_eq!(std::fs::read_to_string(&rc).unwrap(), "bindkey -e\n");
    }

    #[test]
    fn the_plugins_snippet_becomes_the_block() {
        let mut f = fixture();
        f.opts.allow_unverified = true;
        std::fs::write(
            f.opts.module.join("shell/system-zshrc.zsh"),
            "# >>> cinelog >>> snippet\n[[ -O /usr/local/share/cinelog/cinelog.plugin.zsh ]] && \
             source /usr/local/share/cinelog/cinelog.plugin.zsh\n# <<< cinelog <<<\n",
        )
        .unwrap();
        install(&f.opts, &mut Steps::default()).unwrap();
        let rc = std::fs::read_to_string(f.opts.sys.root.join(team::SYSTEM_ZSHRC)).unwrap();
        let copy = f.opts.sys.root.join(team::PLUGIN_COPY);
        assert!(rc.starts_with("# >>> cinelog >>> snippet\n"), "{rc}");
        assert_eq!(rc.matches(&format!("'{}'", copy.display())).count(), 2, "{rc}");
        assert!(
            f.opts
                .sys
                .root
                .join(team::SHARE_DIR)
                .join("shell/system-zshrc.zsh")
                .is_file()
        );
    }

    #[test]
    fn source_binary_prefers_the_module_over_the_running_binary() {
        let f = fixture();
        let m = &f.opts.module;
        let installed = f.opts.sys.root.join(team::BINARY);
        let err = source_binary(None, m, &installed).unwrap_err();
        assert!(err.to_string().contains("--binary"), "{err}");

        let inside = m.join("target/release/cinelog");
        let running = source_binary(None, m, &inside).unwrap();
        assert_eq!(running.read, Path::new("/proc/self/exe"));
        assert_eq!(running.path, inside);

        std::fs::create_dir_all(m.join("bin")).unwrap();
        std::fs::write(m.join(module::BINARY), b"new").unwrap();
        std::fs::write(m.join("VERSION"), "9.9.9\n").unwrap();
        for exe in [&installed, &inside] {
            let src = source_binary(None, m, exe).unwrap();
            assert_eq!((src.read, src.version.as_str()), (m.join(module::BINARY), "9.9.9"));
        }
        let explicit = source_binary(Some(Path::new("/opt/c")), m, &inside).unwrap();
        assert_eq!(explicit.read, Path::new("/opt/c"));
    }

    /// `sudo cinelog install --team --upgrade` runs the installed team binary: the new
    /// binary must come from the module, verified against the module's version.
    #[test]
    fn upgrade_run_by_the_installed_team_binary_installs_the_modules_binary() {
        let mut f = fixture();
        let old = std::fs::read(&f.opts.binary.read).unwrap();
        sums(&f.opts, &old);
        install(&f.opts, &mut Steps::default()).unwrap();
        let installed = f.opts.sys.root.join(team::BINARY);

        let m = f.opts.module.clone();
        std::fs::create_dir_all(m.join("bin")).unwrap();
        std::fs::write(m.join(module::BINARY), b"\x7fELF release 9.9.9").unwrap();
        std::fs::write(m.join("VERSION"), "9.9.9\n").unwrap();
        std::fs::write(m.join(module::PLUGIN_FILE), "# plugin 9.9.9\n").unwrap();
        sums_for(&f.opts, "9.9.9", b"\x7fELF release 9.9.9");
        f.opts.binary = source_binary(None, &m, &installed).unwrap();
        f.opts.upgrade = true;
        let mut steps = Steps::default();
        install(&f.opts, &mut steps).unwrap();
        assert_eq!(std::fs::read(&installed).unwrap(), b"\x7fELF release 9.9.9");
        assert_eq!(
            std::fs::read_to_string(f.opts.sys.root.join(team::PLUGIN_COPY)).unwrap(),
            "# plugin 9.9.9\n"
        );
        assert!(steps.to_string().contains("v9.9.9"), "{steps}");

        // The installed binary as its own source cannot upgrade anything.
        f.opts.binary = source_binary(Some(&installed), &m, &installed).unwrap();
        let err = install(&f.opts, &mut Steps::default()).unwrap_err();
        assert!(err.to_string().contains("installed team binary itself"), "{err}");
    }

    #[test]
    fn system_directories_are_checked_never_chmodded() {
        let mut f = fixture();
        f.opts.allow_unverified = true;
        let bin_dir = f.opts.sys.root.join("/usr/local/bin");
        std::fs::create_dir_all(&bin_dir).unwrap();
        std::fs::set_permissions(&bin_dir, std::fs::Permissions::from_mode(0o2775)).unwrap();
        let etc = f.opts.sys.root.join(team::CONFIG_DIR);
        std::fs::create_dir_all(&etc).unwrap();
        std::fs::set_permissions(&etc, std::fs::Permissions::from_mode(0o755)).unwrap();

        // Test mode: installs, keeps the system directory as it is, tightens its own.
        install(&f.opts, &mut Steps::default()).unwrap();
        assert_eq!(
            std::fs::metadata(&bin_dir).unwrap().permissions().mode() & 0o7777,
            0o2775
        );
        assert_eq!(mode(&etc), 0o700);

        // With ownership checks (as for a real install, here for this uid below the
        // temp dir), the group-writable system directory is refused before any write.
        let base = f._dir.path().to_path_buf();
        std::fs::remove_dir_all(f.opts.sys.root.join("/")).unwrap();
        std::fs::create_dir_all(&bin_dir).unwrap();
        for d in bin_dir.ancestors().skip(1).take_while(|d| *d != base) {
            std::fs::set_permissions(d, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        std::fs::set_permissions(&bin_dir, std::fs::Permissions::from_mode(0o2775)).unwrap();
        f.opts.sys.trust = Some(Trust {
            uid: nix::unistd::geteuid().as_raw(),
            stop: base,
        });
        let err = install(&f.opts, &mut Steps::default()).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("does not change system directories"), "{msg}");
        assert_eq!(
            std::fs::metadata(&bin_dir).unwrap().permissions().mode() & 0o7777,
            0o2775
        );
        assert!(!f.opts.sys.root.join(team::BINARY).exists());

        std::fs::set_permissions(&bin_dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        install(&f.opts, &mut Steps::default()).unwrap();
        assert!(f.opts.sys.root.join(team::BINARY).is_file());
    }

    #[test]
    fn a_symlinked_system_zshrc_is_edited_through_the_link() {
        let mut f = fixture();
        f.opts.allow_unverified = true;
        let managed = f._dir.path().join("managed/zshrc");
        std::fs::create_dir_all(managed.parent().unwrap()).unwrap();
        std::fs::write(&managed, "bindkey -e\n").unwrap();
        std::fs::set_permissions(&managed, std::fs::Permissions::from_mode(0o640)).unwrap();
        let rc = f.opts.sys.root.join(team::SYSTEM_ZSHRC);
        std::fs::create_dir_all(rc.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&managed, &rc).unwrap();

        install(&f.opts, &mut Steps::default()).unwrap();
        assert!(std::fs::symlink_metadata(&rc).unwrap().file_type().is_symlink());
        assert!(zshrc::has_block(&std::fs::read_to_string(&managed).unwrap()));
        assert_eq!(mode(&managed), 0o640);

        uninstall(&f.opts.sys, &mut Steps::default()).unwrap();
        assert!(std::fs::symlink_metadata(&rc).unwrap().file_type().is_symlink());
        assert_eq!(std::fs::read_to_string(&managed).unwrap(), "bindkey -e\n");
        assert_eq!(mode(&managed), 0o640);
    }

    /// Records `systemctl` calls; `active` is what `is-active` answers.
    #[derive(Default)]
    struct FakeSystemd {
        active: bool,
        calls: std::cell::RefCell<Vec<String>>,
    }

    impl Systemd for FakeSystemd {
        fn is_active(&self, _unit: &str) -> bool {
            self.active
        }
        fn run(&self, args: &[&str], steps: &mut Steps) -> bool {
            self.calls.borrow_mut().push(args.join(" "));
            steps.done(format!("systemctl {}", args.join(" ")));
            true
        }
    }

    impl FakeSystemd {
        fn take(&self) -> Vec<String> {
            std::mem::take(&mut *self.calls.borrow_mut())
        }
    }

    fn host<'a>(systemd: Option<&'a dyn Systemd>, sudo_user: Option<&str>) -> Host<'a> {
        Host {
            systemd,
            proc_root: None,
            sudo_user: sudo_user.map(Into::into),
            now: 1_794_139_200.0,
        }
    }

    /// A real root without systemd as init (a container, OpenRC) gets no system manager, so
    /// an upgrade restarts a detached team server detached instead of through the unit.
    #[test]
    fn systemd_is_driven_only_where_it_is_the_init() {
        let real = SystemRoot::real();
        let booted = Host::detect(&real, true);
        assert!(booted.systemd.is_some() && booted.proc_root.is_some());
        let other_init = Host::detect(&real, false);
        assert!(other_init.systemd.is_none(), "no systemctl calls without systemd");
        assert!(other_init.proc_root.is_some(), "the detached server is still restarted");
        let prefixed = Host::detect(&SystemRoot::prefixed("/tmp/x"), true);
        assert!(prefixed.systemd.is_none() && prefixed.proc_root.is_none());
    }

    #[test]
    fn the_packaged_unit_matches_the_team_paths() {
        let line = |key: &str| {
            UNIT.lines()
                .find_map(|l| l.strip_prefix(key)?.strip_prefix('='))
                .unwrap_or_else(|| panic!("no {key}= in the unit"))
        };
        assert_eq!(line("ExecStart"), format!("{} serve --scope team", team::BINARY));
        let rw: Vec<&str> = line("ReadWritePaths").split(' ').collect();
        assert!(rw.contains(&team::RUNTIME_DIR) && rw.contains(&team::TLS_DIR), "{rw:?}");
        assert_eq!(format!("/var/lib/{}", line("StateDirectory")), team::STATE_DIR);
        assert_eq!(line("BindPaths"), team::LOGS);
        assert_eq!(line("CapabilityBoundingSet"), "");
        assert_eq!(line("ProtectSystem"), "strict");
        assert_eq!(line("ProtectHome"), "tmpfs");
        assert!(line("RestrictAddressFamilies").contains("AF_NETLINK"));
        assert!(UNIT.contains(UNIT_LOGS_LINE));
        assert!(
            TMPFILES_CONF
                .lines()
                .any(|l| l == format!("d {} 0700 root root -", team::RUNTIME_DIR))
        );
        // The unit installed with the default log root is the packaged file itself.
        assert_eq!(render_unit(Path::new(team::LOGS)).unwrap(), UNIT);
    }

    #[test]
    fn the_unit_binds_the_configured_log_root() {
        let unit = render_unit(Path::new("/srv/cinelog-logs")).unwrap();
        assert!(unit.contains("\nBindPaths=/srv/cinelog-logs\n"), "{unit}");
        assert!(!unit.contains("/root/logs"));
        for bad in ["relative", "/a b", "/a:b", "/100%", "/a\"b", "/$x"] {
            assert!(render_unit(Path::new(bad)).is_err(), "{bad}");
        }

        let mut f = fixture();
        f.opts.allow_unverified = true;
        let cfg = f.opts.sys.root.join(team::CONFIG);
        std::fs::create_dir_all(cfg.parent().unwrap()).unwrap();
        std::fs::write(&cfg, "[paths]\nlogs = \"~/team-logs\"\n").unwrap();
        install_on(&f.opts, &host(None, None), &mut Steps::default()).unwrap();
        let unit = std::fs::read_to_string(f.opts.sys.root.join(team::UNIT)).unwrap();
        assert!(unit.contains("\nBindPaths=/root/team-logs\n"), "{unit}");
        assert!(f.opts.sys.root.join("/root/team-logs").is_dir());
    }

    #[test]
    fn systemd_is_reloaded_enabled_and_restarted_only_on_change() {
        let mut f = fixture();
        f.opts.allow_unverified = true;
        let sd = FakeSystemd::default();
        install_on(&f.opts, &host(Some(&sd), None), &mut Steps::default()).unwrap();
        assert_eq!(sd.take(), ["daemon-reload", "enable --now cinelog-team.service"]);

        // Running and unchanged: enable --now is a no-op for systemd, no restart.
        let sd = FakeSystemd {
            active: true,
            ..FakeSystemd::default()
        };
        install_on(&f.opts, &host(Some(&sd), None), &mut Steps::default()).unwrap();
        assert_eq!(sd.take(), ["enable --now cinelog-team.service"]);

        // A new binary restarts the running server.
        std::fs::write(&f.opts.binary.read, b"\x7fELF newer").unwrap();
        f.opts.upgrade = true;
        let mut steps = Steps::default();
        install_on(&f.opts, &host(Some(&sd), None), &mut steps).unwrap();
        assert_eq!(
            sd.take(),
            ["enable --now cinelog-team.service", "try-restart cinelog-team.service"]
        );
        assert!(steps.to_string().contains("systemctl try-restart"), "{steps}");

        uninstall_on(&f.opts.sys, &host(Some(&sd), None), &mut Steps::default()).unwrap();
        assert_eq!(sd.take(), ["disable --now cinelog-team.service", "daemon-reload"]);
        assert!(!f.opts.sys.root.join(team::UNIT).exists());
        assert!(f.opts.sys.root.join(team::TLS_DIR).is_dir(), "the certificate is kept");
    }

    #[test]
    fn the_installing_member_gets_the_first_token_once() {
        let mut f = fixture();
        f.opts.allow_unverified = true;
        let mut steps = Steps::default();
        install_on(&f.opts, &host(None, Some("alice")), &mut steps).unwrap();
        let tokens_file = f.opts.sys.root.join(team::TOKENS);
        let tf = cinelog_web::tokens::TokenFile::load(&tokens_file).unwrap();
        assert_eq!(tf.tokens.len(), 1);
        assert_eq!(tf.tokens[0].name, "alice");
        assert_eq!(mode(&tokens_file), 0o600);
        let text = steps.to_string();
        let secret = text
            .lines()
            .find_map(|l| l.split("(shown once): ").nth(1))
            .unwrap_or_else(|| panic!("{text}"));
        assert_eq!(tf.verify(secret).map(|t| t.name.as_str()), Some("alice"));
        assert!(!std::fs::read_to_string(&tokens_file).unwrap().contains(secret));

        let mut again = Steps::default();
        install_on(&f.opts, &host(None, Some("alice")), &mut again).unwrap();
        assert!(
            again.to_string().contains("kept the team web token of alice"),
            "{again}"
        );
        assert!(!again.to_string().contains("shown once"));
        assert_eq!(cinelog_web::tokens::TokenFile::load(&tokens_file).unwrap(), tf);

        for (user, expect) in [
            (None, "no $SUDO_USER"),
            (Some("root"), "no $SUDO_USER"),
            (Some("a b"), "not a valid"),
        ] {
            let mut s = Steps::default();
            install_on(&f.opts, &host(None, user), &mut s).unwrap();
            assert!(s.to_string().contains(expect), "{user:?}: {s}");
        }
        assert_eq!(
            cinelog_web::tokens::TokenFile::load(&tokens_file).unwrap().tokens.len(),
            1
        );
    }

    #[test]
    fn the_certificate_is_created_once_and_its_fingerprint_printed() {
        let mut f = fixture();
        f.opts.allow_unverified = true;
        let fingerprint = |steps: &Steps| {
            let text = steps.to_string();
            text.lines()
                .find_map(|l| l.split("fingerprint ").nth(1).map(str::to_owned))
                .unwrap_or_else(|| panic!("{text}"))
        };
        let mut steps = Steps::default();
        install_on(&f.opts, &host(None, None), &mut steps).unwrap();
        assert!(steps.to_string().contains("new self-signed"), "{steps}");
        let first = fingerprint(&steps);
        let tls_dir = f.opts.sys.root.join(team::TLS_DIR);
        let opts = TlsOptions {
            dir: tls_dir.clone(),
            cert: None,
            key: None,
            auto_regen: false,
            names: Vec::new(),
        };
        let info = tls::inspect(&opts).unwrap().expect("a certificate in the team TLS dir");
        assert_eq!(info.fingerprint, first);
        assert_eq!(mode(&tls_dir.join("key.pem")) & 0o077, 0, "the key is private");

        // A second install keeps it: the fingerprint members pinned stays valid.
        let mut again = Steps::default();
        install_on(&f.opts, &host(None, None), &mut again).unwrap();
        assert_eq!(fingerprint(&again), first);
        assert!(!again.to_string().contains("new self-signed"), "{again}");
    }

    /// The server loads its certificate only at start: one the install regenerated must
    /// reach the running server, or members would pin a certificate it does not serve.
    #[test]
    fn a_regenerated_certificate_restarts_the_running_server() {
        let mut f = fixture();
        f.opts.allow_unverified = true;
        let sd = FakeSystemd {
            active: true,
            ..FakeSystemd::default()
        };
        install_on(&f.opts, &host(Some(&sd), None), &mut Steps::default()).unwrap();
        sd.take();
        install_on(&f.opts, &host(Some(&sd), None), &mut Steps::default()).unwrap();
        assert_eq!(sd.take(), ["enable --now cinelog-team.service"], "nothing changed");

        let tls_dir = f.opts.sys.root.join(team::TLS_DIR);
        for entry in std::fs::read_dir(&tls_dir).unwrap() {
            std::fs::remove_file(entry.unwrap().path()).unwrap();
        }
        let mut steps = Steps::default();
        install_on(&f.opts, &host(Some(&sd), None), &mut steps).unwrap();
        assert!(steps.to_string().contains("new self-signed"), "{steps}");
        assert_eq!(
            sd.take(),
            ["enable --now cinelog-team.service", "try-restart cinelog-team.service"]
        );

        // Not running: enable --now starts it with the new certificate anyway.
        for entry in std::fs::read_dir(&tls_dir).unwrap() {
            std::fs::remove_file(entry.unwrap().path()).unwrap();
        }
        let stopped = FakeSystemd::default();
        install_on(&f.opts, &host(Some(&stopped), None), &mut Steps::default()).unwrap();
        assert_eq!(stopped.take(), ["enable --now cinelog-team.service"]);
    }

    /// The printed URL and team-login command use the port the server has or will have.
    #[test]
    fn members_get_the_effective_url_and_the_exact_login_command() {
        let mut f = fixture();
        f.opts.allow_unverified = true;
        let note = |f: &Fixture| {
            let mut steps = Steps::default();
            install_on(&f.opts, &host(None, None), &mut steps).unwrap();
            let text = steps.to_string();
            let fp = text
                .lines()
                .find_map(|l| l.split("fingerprint ").nth(1).map(str::to_owned))
                .unwrap_or_else(|| panic!("{text}"));
            let login = text
                .lines()
                .find_map(|l| {
                    l.split('`')
                        .find(|c| c.starts_with("cinelog token team-login"))
                        .map(str::to_owned)
                })
                .unwrap_or_else(|| panic!("{text}"));
            assert_eq!(login.split("--fingerprint ").nth(1), Some(fp.as_str()), "{text}");
            let url = login.split_whitespace().nth(4).unwrap().to_owned();
            assert!(text.contains(&format!("the team web UI is {url} ")), "{text}");
            url
        };
        let host_name = cinelog_core::paths::hostname();
        assert_eq!(note(&f), format!("https://{host_name}:10000"));

        let r = f.opts.sys.root.clone();
        let paths = team_paths(&r);
        std::fs::write(paths.migrated_port_file(), "10500\n").unwrap();
        assert_eq!(note(&f), format!("https://{host_name}:10500"), "root's old viewer port");

        let mut meta = cinelog_index::db::open_meta(&DbPaths::in_dir(&paths.db_dir)).unwrap();
        meta.set_setting(settings_keys::PORT, Some("10003")).unwrap();
        drop(meta);
        assert_eq!(
            note(&f),
            format!("https://{host_name}:10003"),
            "the persisted port wins"
        );

        std::fs::write(
            r.join(team::CONFIG),
            "[server]\nhostnames = [\"team.lan\"]\nbind = [\"0.0.0.0:10443\"]\n",
        )
        .unwrap();
        assert_eq!(note(&f), "https://team.lan:10443");

        // A running server knows best.
        ServerInfo {
            pid: std::process::id(),
            url: "https://team.lan:10007".into(),
            port: 10007,
            fingerprint: String::new(),
            version: String::new(),
        }
        .write(&paths.runtime_dir)
        .unwrap();
        assert_eq!(note(&f), "https://team.lan:10007");
    }

    #[test]
    fn real_root_requires_privileges() {
        let opts = Options {
            sys: System::new(SystemRoot::real(), 1000),
            ..fixture().opts
        };
        assert!(
            install(&opts, &mut Steps::default())
                .unwrap_err()
                .to_string()
                .contains("sudo")
        );
        assert!(uninstall(&opts.sys, &mut Steps::default()).is_err());
    }
}
