//! `cinelog install --team` (§5.6, §9.7, §10.3, §10.6 step 6; M1 part, no server): the
//! verified binary in `/usr/local/bin`, the root-owned plugin copy, the system zshrc block,
//! `/etc/cinelog`, `/var/lib/cinelog`, the log root and the runtime dir.
//!
//! With `--prefix DIR` every system path lives below DIR and no root privileges are needed;
//! that is the test mode. Without it the command must run as root and refuses to write
//! into any directory that is not root-owned and free of group/world write along its whole
//! path. Modes are only tightened on directories cinelog owns; system directories such as
//! `/usr/local/bin` are checked, never changed.

use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use cinelog_core::config::legacy;
use cinelog_core::location;
use cinelog_core::paths::{Paths, SystemRoot, team};
use cinelog_core::{Config, Scope};
use sha2::{Digest, Sha256};

use super::fsutil::{self, Outcome};
use super::{Steps, migrate, zshrc};
use crate::module;

pub const TMPFILES_LINE: &str = "d /run/cinelog 0700 root root -\n";

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
    let sys = &opts.sys;
    sys.check_privileges()?;
    let root = &sys.root;
    let home = no_home();
    let binary = check_binary(opts, steps)?;

    for (dir, mode, kind, what) in [
        (team::CONFIG_DIR, 0o700, DirKind::Owned, "team config"),
        (team::STATE_DIR, 0o700, DirKind::Owned, "team databases"),
        (team::RUNTIME_DIR, 0o700, DirKind::Owned, "team registry"),
        ("/etc/tmpfiles.d", 0o755, DirKind::System, ""),
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
    let outcome = fsutil::write_atomic(&tmpfiles, TMPFILES_LINE.as_bytes(), 0o644)?;
    steps.file(&outcome, &tmpfiles, "(recreates /run/cinelog at boot)", home);

    write_binary(opts, &binary, steps)?;
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

    let unit = root.join(team::UNIT);
    if root.is_real() && unit.exists() {
        super::systemctl(&["try-restart", "cinelog-team.service"], steps);
    }
    steps.note("recommended: `Defaults always_set_home` in sudoers or `sudo -i`, so root never runs user dotfiles");
    steps.note("for the team web server (later versions) give it a hostname of its own: server.hostnames in team.toml");
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

fn write_binary(opts: &Options, bytes: &[u8], steps: &mut Steps) -> Result<()> {
    let target = opts.sys.root.join(team::BINARY);
    let outcome = fsutil::write_atomic(&target, bytes, 0o755)?;
    opts.sys.require_secure(&target)?;
    steps.file(
        &outcome,
        &target,
        &format!("(cinelog {} from {})", opts.binary.version, opts.binary.path.display()),
        no_home(),
    );
    Ok(())
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
        if root.is_real() {
            super::systemctl(&["disable", "--now", "cinelog-team.service"], steps);
        }
        fsutil::remove_file(&unit)?;
        steps.done(format!("removed {}", unit.display()));
    }
    let tmpfiles = root.join(team::TMPFILES);
    if fsutil::remove_file(&tmpfiles)? {
        steps.done(format!("removed {}", tmpfiles.display()));
    }
    let paths = team_paths(root);
    steps.note(format!(
        "kept {} (running recorders use it), {}, {} and the casts in {}",
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
        assert_eq!(std::fs::read_to_string(r.join(team::TMPFILES)).unwrap(), TMPFILES_LINE);
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
