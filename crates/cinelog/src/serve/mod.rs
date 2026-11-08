//! `cinelog serve`, `compress` and `meta` (work package M2-D; plan §6.4, §6.6, §10.4).
//!
//! - [`lock`]: the serve.lock singleton and `--ensure` (systemd unit, else a detached serve);
//! - [`discover`]: registry scan and the logs walk (with [`cinelog_index::classify()`]);
//! - [`engine`]: pipeline sources, job planning, the backfill pool (`min(4, cores/2)` render
//!   threads feeding one writer thread) and the one-shot engine of `cinelog index`;
//! - [`live`]: incremental indexing of open casts (2 s commits, provisional chunks);
//! - [`daemon`]: the scheduler loop, crash detection, forget sync, daily compressor and
//!   nightly meta backup;
//! - [`compress`]: the zstd compressor and the reconciliation of interrupted runs;
//! - [`backup`]: meta export/import, nightly backups, the migrated port.
//!
//! The user unit is `packaging/systemd/cinelog.service`.

pub mod backup;
pub mod compress;
pub mod daemon;
pub mod discover;
pub mod engine;
pub mod live;
pub mod lock;
pub mod log;
#[cfg(test)]
pub(crate) mod testkit;

use std::collections::HashSet;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::AtomicBool;

use cinelog_core::Scope;
use cinelog_core::registry::SystemProbe;
use cinelog_index::model::{DbPaths, RenderOptions};
use cinelog_index::{IndexEngine, IndexError, IndexWriter, MetaStore, db};

use crate::cli::{CompressArgs, MetaAction, MetaArgs, ScopeArg, ServeArgs};
use crate::ctx::Ctx;
use compress::CompressOpts;
use daemon::{Daemon, DaemonConfig, Signals};
use engine::{BACKFILL_POLL_BYTES, CRASH_GRACE, Engine, EngineConfig, Sources, default_workers, shared};
use lock::{Acquire, EnsureAction, ServeLock};

/// Exit statuses (docs/PROTOCOL.md §6.1).
mod code {
    pub const OK: u8 = 0;
    /// Some casts failed to compress; `--ensure` could not start a server.
    pub const PARTIAL: u8 = 1;
    pub const ERROR: u8 = 2;
}

pub fn run(args: &ServeArgs) -> ExitCode {
    let ctx = Ctx::load();
    if let Err(e) = check_scope(&ctx, args.scope) {
        eprintln!("cinelog: serve: {e}");
        return ExitCode::from(code::ERROR);
    }
    if args.ensure {
        return ensure(&ctx);
    }
    let sink = log::init(&ctx.paths.state_dir);
    tracing::info!(
        "cinelog {} serve, {} scope, pid {}, logging to {sink}",
        env!("CARGO_PKG_VERSION"),
        ctx.scope(),
        std::process::id()
    );
    if !args.no_http {
        tracing::info!("this build has no web server yet: running the indexer only");
    }
    if let Some(e) = &ctx.config_error {
        tracing::warn!("{e}; using the defaults");
    }
    match serve(&ctx) {
        Ok(()) => ExitCode::from(code::OK),
        Err(e) => {
            tracing::error!("{e:#}");
            if !std::io::IsTerminal::is_terminal(&std::io::stderr()) {
                eprintln!("cinelog: serve: {e:#}");
            }
            ExitCode::from(code::ERROR)
        }
    }
}

fn check_scope(ctx: &Ctx, arg: Option<ScopeArg>) -> Result<(), String> {
    let wanted = match arg {
        None => return Ok(()),
        Some(ScopeArg::Personal) => Scope::Personal,
        Some(ScopeArg::Team) => Scope::Team,
    };
    match (wanted, ctx.scope()) {
        (w, s) if w == s => Ok(()),
        (Scope::Team, _) => Err("the team server runs as root (EUID 0)".into()),
        (_, _) => Err("the personal server does not run as root; root shells belong to the team scope".into()),
    }
}

fn serve(ctx: &Ctx) -> anyhow::Result<()> {
    let db = prepare_db_dir(ctx)?;
    if let Some(p) =
        cinelog_core::location::db_location_problem(&ctx.paths.logs_root).filter(|_| ctx.paths.logs_root.exists())
    {
        tracing::warn!("logs: {p}");
    }
    // Before the lock: `compress --now` signals whoever holds it.
    let signals = Signals::install()?;
    let serve_lock = match ServeLock::acquire(&ctx.paths.runtime_dir)? {
        Acquire::Acquired(l) => l,
        Acquire::Held { pid } => {
            let pid = pid.map_or_else(|| "?".to_owned(), |p| p.to_string());
            tracing::info!(
                "another server (pid {pid}) holds {}",
                lock::lock_path(&ctx.paths.runtime_dir).display()
            );
            return Ok(());
        }
    };
    tracing::debug!("holding {}", serve_lock.path().display());
    let Some(writer) = open_writer_waiting(&db, ctx, &signals.stop)? else {
        return Ok(());
    };
    let meta = db::open_meta(&db)?;
    let cfg = DaemonConfig::new(
        ctx.scope(),
        ctx.paths.logs_root.clone(),
        registry_dirs(ctx),
        db,
        ctx.paths.migrated_port_file(),
        CompressOpts::from_config(&ctx.config.retention),
    );
    tracing::info!("indexing {} into {}", cfg.logs_root.display(), cfg.db.dir.display());
    let daemon = Daemon::new(writer, meta, sources(ctx), Box::new(SystemProbe), cfg);
    daemon.run(&signals)?;
    drop(serve_lock);
    Ok(())
}

/// The writer, waiting while a `cinelog index` run holds its lock; `None` if stopped
/// meanwhile.
fn open_writer_waiting(db: &DbPaths, ctx: &Ctx, stop: &AtomicBool) -> Result<Option<Box<dyn IndexWriter>>, IndexError> {
    let mut logged = false;
    loop {
        match db::open_writer(db, &ctx.config.index) {
            Ok(w) => return Ok(Some(w)),
            Err(IndexError::Locked(p)) => {
                if !logged {
                    tracing::info!("waiting for {} (cinelog index is running)", p.display());
                    logged = true;
                }
                for _ in 0..20 {
                    if stop.load(std::sync::atomic::Ordering::Relaxed) {
                        return Ok(None);
                    }
                    std::thread::sleep(std::time::Duration::from_millis(100));
                }
            }
            Err(e) => return Err(e),
        }
    }
}

/// Refuses synced or network DB directories (§6.1) and creates the directory (0700).
fn prepare_db_dir(ctx: &Ctx) -> Result<DbPaths, IndexError> {
    let dir = &ctx.paths.db_dir;
    if let Some(p) = cinelog_core::location::db_location_problem(dir) {
        return Err(IndexError::Invalid(p));
    }
    std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
    Ok(DbPaths::in_dir(dir))
}

/// Registry directories of the scope. A PERSONAL recorder started without
/// `$XDG_RUNTIME_DIR` (ssh, cron) registers in the state fallback, so both are scanned.
fn registry_dirs(ctx: &Ctx) -> Vec<PathBuf> {
    let mut dirs = vec![ctx.paths.runtime_dir.clone()];
    if ctx.scope() == Scope::Personal {
        let fallback = ctx.env.state_home().join("cinelog/run");
        if !dirs.contains(&fallback) {
            dirs.push(fallback);
        }
    }
    dirs
}

fn sources(ctx: &Ctx) -> Sources {
    Sources::new(
        RenderOptions::from(&ctx.config.index),
        ctx.scope(),
        &ctx.paths.logs_root,
        &cinelog_core::paths::hostname(),
    )
}

// ------------------------------------------------------------------ --ensure

fn ensure(ctx: &Ctx) -> ExitCode {
    let held = lock::is_held(&ctx.paths.runtime_dir);
    let (unit, unit_file) = match ctx.scope() {
        Scope::Personal => ("cinelog.service", crate::install::user_unit_file(&ctx.env)),
        Scope::Team => (
            "cinelog-team.service",
            ctx.root.join("/etc/systemd/system/cinelog-team.service"),
        ),
    };
    let user = ctx.scope() == Scope::Personal;
    // Only a unit the manager actually loads counts: one below a sandbox HOME or a
    // --prefix root must never make us start the real manager's unit.
    let managed = if user {
        crate::install::UserManager::real(ctx.env.euid).reads(&unit_file)
    } else {
        ctx.root.is_real()
    };
    match lock::decide(held, ctx.config.server.autostart, managed && unit_file.exists()) {
        EnsureAction::AlreadyRunning | EnsureAction::Disabled => ExitCode::from(code::OK),
        EnsureAction::NoUnit => {
            eprintln!(
                "cinelog: serve --ensure: server.autostart is \"systemd\" but {} is not installed",
                unit_file.display()
            );
            ExitCode::from(code::PARTIAL)
        }
        EnsureAction::Systemd { fallback } => {
            if systemctl_start(user, unit) {
                ExitCode::from(code::OK)
            } else if fallback {
                start_detached(ctx)
            } else {
                eprintln!("cinelog: serve --ensure: systemctl could not start {unit}");
                ExitCode::from(code::PARTIAL)
            }
        }
        EnsureAction::Detached => start_detached(ctx),
    }
}

fn systemctl_start(user: bool, unit: &str) -> bool {
    let mut cmd = std::process::Command::new("systemctl");
    if user {
        cmd.arg("--user");
    }
    cmd.args(["start", "--no-block", unit])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

fn start_detached(ctx: &Ctx) -> ExitCode {
    let scope = match ctx.scope() {
        Scope::Personal => "personal",
        Scope::Team => "team",
    };
    match lock::spawn_detached(&lock::self_exe(), &["serve", "--scope", scope]) {
        Ok(_) => ExitCode::from(code::OK),
        Err(e) => {
            eprintln!("cinelog: serve --ensure: cannot start a server: {e}");
            ExitCode::from(code::PARTIAL)
        }
    }
}

// ------------------------------------------------------------------ compress

pub fn compress(args: &CompressArgs) -> ExitCode {
    let ctx = Ctx::load();
    let db = DbPaths::in_dir(&ctx.paths.db_dir);
    let mut out = std::io::stdout().lock();
    match compress_cmd(&ctx, &db, args.dry_run, &mut out) {
        Ok(c) => ExitCode::from(c),
        Err(e) => {
            eprintln!("cinelog: compress: {e:#}");
            ExitCode::from(code::ERROR)
        }
    }
}

fn compress_cmd(ctx: &Ctx, db: &DbPaths, dry_run: bool, out: &mut dyn std::io::Write) -> anyhow::Result<u8> {
    if !db.index.is_file() {
        writeln!(out, "nothing to compress: no index yet at {}", db.index.display())?;
        return Ok(code::OK);
    }
    let opts = CompressOpts::from_config(&ctx.config.retention);
    let writer = match db::open_writer(db, &ctx.config.index) {
        Ok(w) => Some(shared(w)),
        // The normal case while serve runs: list through a reader instead.
        Err(IndexError::Locked(_)) if dry_run => None,
        Err(IndexError::Locked(_)) => {
            // Verified to hold serve.lock: a stale pid in the file is never signalled.
            return match lock::holder_pid(&ctx.paths.runtime_dir) {
                Some(pid) => {
                    nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid as i32), nix::sys::signal::SIGUSR1)?;
                    writeln!(out, "asked the running server (pid {pid}) to compress now")?;
                    Ok(code::OK)
                }
                None => anyhow::bail!("the index is busy (cinelog serve or cinelog index holds its writer lock)"),
            };
        }
        Err(e) => return Err(e.into()),
    };
    let list = match &writer {
        Some(w) => compress::candidates(w, &opts, &HashSet::new())?,
        None => compress::preview(db::open_reader(db)?.as_ref(), &ctx.paths.logs_root, &opts)?,
    };
    let Some(writer) = writer.filter(|_| !dry_run) else {
        let mut total = 0;
        for c in &list {
            let size = std::fs::metadata(&c.path).map(|m| m.len()).unwrap_or(0);
            total += size;
            let ended = jiff::Timestamp::from_microsecond((c.ended_at * 1e6) as i64)
                .map(|t| t.strftime("%Y-%m-%d").to_string())
                .unwrap_or_default();
            writeln!(out, "{}\t{ended}\t{size}", c.path.display())?;
        }
        writeln!(
            out,
            "{} casts, {total} bytes, older than {} days",
            list.len(),
            opts.after_days
        )?;
        return Ok(code::OK);
    };
    let r = compress::compress(&writer, &list, opts.level, &AtomicBool::new(false));
    writeln!(
        out,
        "compressed {} of {} casts: {} → {} bytes",
        r.compressed, r.candidates, r.bytes_in, r.bytes_out
    )?;
    for e in &r.errors {
        eprintln!("cinelog: compress: {e}");
    }
    Ok(if r.errors.is_empty() { code::OK } else { code::PARTIAL })
}

// ------------------------------------------------------------------ meta

pub fn meta(args: &MetaArgs) -> ExitCode {
    let ctx = Ctx::load();
    let result = prepare_db_dir(&ctx).and_then(|db| {
        let mut m = db::open_meta(&db)?;
        match &args.action {
            MetaAction::Export { file } => backup::export(m.as_ref(), file).map(|n| ("exported", n, file)),
            MetaAction::Import { file } => backup::import(m.as_mut(), file).map(|n| ("imported", n, file)),
        }
    });
    match result {
        Ok((what, rows, file)) => {
            eprintln!("{what} {rows} rows ({})", display_file(file));
            ExitCode::from(code::OK)
        }
        Err(e) => {
            eprintln!("cinelog: meta: {e}");
            ExitCode::from(code::ERROR)
        }
    }
}

fn display_file(p: &Path) -> String {
    if p == Path::new("-") {
        "standard stream".into()
    } else {
        p.display().to_string()
    }
}

// ------------------------------------------------------------------ cinelog index

/// The one-shot discovery and backfill engine of `ctx`'s scope (takes the writer lock).
pub fn open_engine(ctx: &Ctx) -> Result<Box<dyn IndexEngine>, IndexError> {
    let db = prepare_db_dir(ctx)?;
    let writer = db::open_writer(&db, &ctx.config.index)?;
    let meta: Box<dyn MetaStore> = db::open_meta(&db)?;
    Ok(Box::new(Engine::new(
        writer,
        Some(meta),
        sources(ctx),
        Box::new(SystemProbe),
        EngineConfig {
            logs_root: ctx.paths.logs_root.clone(),
            registry_dirs: registry_dirs(ctx),
            scope: ctx.scope(),
            workers: default_workers(),
            poll_bytes: BACKFILL_POLL_BYTES,
            crash_grace: CRASH_GRACE,
        },
    )))
}
