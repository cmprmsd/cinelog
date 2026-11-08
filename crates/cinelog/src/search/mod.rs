//! `cinelog search`, `show`, `forget`, `index`, `status` and `import --report` (work package
//! M2-E; plan §9.5, §6.4 "Status", §7.6, §11.8).
//!
//! Search, show, status and the import report read through a read-only
//! [`cinelog_index::db::open_reader`] on the scope's DB (personal, or the team DB at EUID 0);
//! forget records into [`cinelog_index::db::open_meta`] and, while no indexer holds the writer
//! lock, purges index.db at once; index runs [`crate::serve::open_engine`].
//!
//! Every subcommand body takes a [`Store`] and an [`Env`], so the tests drive them against
//! the in-memory `fake::MemIndex` with a fixed clock, zone and server state.

mod forget;
mod list;
mod pick;
mod show;
mod status;
mod team;
mod text;
mod when;

use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use cinelog_core::config::IndexConfig;
use cinelog_core::server::ServerInfo;
use cinelog_index::model::{DbPaths, OnceOptions};
use cinelog_index::{IndexError, IndexWriter, MetaStore, Searcher, db};
use jiff::Timestamp;
use jiff::tz::TimeZone;

use crate::cli::{ForgetArgs, ImportArgs, IndexArgs, SearchArgs, ShowArgs, StatusArgs};
use crate::ctx::Ctx;
use text::Style;

/// Exit statuses of these subcommands (docs/PROTOCOL.md §6.1).
pub mod code {
    pub const OK: u8 = 0;
    /// No match / not found / some casts failed.
    pub const NONE: u8 = 1;
    pub const ERROR: u8 = 2;
}

pub fn run(args: &SearchArgs) -> ExitCode {
    let ctx = Ctx::load();
    let mut env = Env::current(&ctx);
    let fzf;
    let ui = if args.json {
        list::Ui::Json
    } else if args.print || !std::io::stdout().is_terminal() {
        list::Ui::Plain
    } else {
        match pick::Fzf::find() {
            Some(f) => {
                fzf = f.team(args.team);
                list::Ui::Pick(&fzf)
            }
            None => list::Ui::Plain,
        }
    };
    if args.team {
        env.server = team::server(&ctx);
        return exit("search", |out, err| {
            let store = team::store(&ctx)?;
            list::search(args, store.as_ref(), &env, ui, out, err)
        });
    }
    let store = DbStore::new(&ctx);
    exit("search", |out, err| list::search(args, &store, &env, ui, out, err))
}

pub fn show(args: &ShowArgs) -> ExitCode {
    let ctx = Ctx::load();
    let env = Env::current(&ctx);
    if args.team {
        return exit("show", |out, err| {
            let store = team::store(&ctx)?;
            show::show(args, store.as_ref(), &env, out, err)
        });
    }
    let store = DbStore::new(&ctx);
    exit("show", |out, err| show::show(args, &store, &env, out, err))
}

pub fn forget(args: &ForgetArgs) -> ExitCode {
    let ctx = Ctx::load();
    let store = DbStore::new(&ctx);
    let by = forget::forgetter(ctx.env.euid, &ctx.env.user, std::process::id(), |req| {
        use cinelog_detect::Detector;
        ctx.detector().principal(req)
    });
    exit("forget", |out, err| forget::forget(args, &store, &by, out, err))
}

pub fn index(args: &IndexArgs) -> ExitCode {
    let ctx = Ctx::load();
    let opts = OnceOptions {
        rebuild: args.rebuild,
        paths: args.paths.clone(),
    };
    let result = crate::serve::open_engine(&ctx).and_then(|mut e| e.index_once(&opts));
    exit("index", |out, err| status::index_summary(result, out, err))
}

pub fn status(args: &StatusArgs) -> ExitCode {
    let ctx = Ctx::load();
    let env = Env::current(&ctx);
    let store = DbStore::new(&ctx);
    let db = store.paths.clone();
    let daemon = status::Daemon {
        serve_lock_held: crate::serve::lock::is_held(&ctx.paths.runtime_dir),
        writer_lock_held: lock_held(&db.writer_lock),
        server: env.server.clone(),
    };
    exit("status", |out, _| status::status(args, &store, &db, &daemon, &env, out))
}

pub fn import_report(args: &ImportArgs) -> ExitCode {
    let ctx = Ctx::load();
    let store = DbStore::new(&ctx);
    exit("import", |out, _| status::import_report(args, &store, out))
}

/// Runs `body` on locked stdout/stderr and maps an error to `cinelog: <what>: <error>`.
fn exit(what: &str, body: impl FnOnce(&mut dyn Write, &mut dyn Write) -> anyhow::Result<u8>) -> ExitCode {
    let mut out = std::io::stdout().lock();
    let mut err = std::io::stderr().lock();
    let result = body(&mut out, &mut err);
    let _ = out.flush();
    match result {
        Ok(c) => ExitCode::from(c),
        Err(e) if is_broken_pipe(&e) => ExitCode::SUCCESS,
        Err(e) => {
            let _ = writeln!(err, "cinelog: {what}: {e:#}");
            ExitCode::from(code::ERROR)
        }
    }
}

fn is_broken_pipe(e: &anyhow::Error) -> bool {
    e.downcast_ref::<std::io::Error>()
        .is_some_and(|e| e.kind() == std::io::ErrorKind::BrokenPipe)
}

// ------------------------------------------------------------------ environment

/// What the subcommands need from the process besides the databases.
#[derive(Debug, Clone)]
pub struct Env {
    pub tz: TimeZone,
    pub now: Timestamp,
    /// ANSI styles on stdout.
    pub style: Style,
    /// The logical working directory (`$PWD` when it names the current directory).
    pub pwd: Option<PathBuf>,
    pub home: PathBuf,
    /// The running web server of this scope (`server.json`), once there is one (M3).
    pub server: Option<ServerInfo>,
    /// Open URLs with `xdg-open`: a graphical session that is not reached over ssh (§9.5).
    pub can_open: bool,
}

impl Env {
    fn current(ctx: &Ctx) -> Self {
        let var = |k: &str| std::env::var_os(k).filter(|v| !v.is_empty());
        // `show` is the fzf preview: fzf renders ANSI there although stdout is a pipe.
        let color = (std::io::stdout().is_terminal() || var("FZF_PREVIEW_COLUMNS").is_some())
            && var("NO_COLOR").is_none()
            && std::env::var("TERM").map_or(true, |t| t != "dumb");
        let graphical = var("DISPLAY").is_some() || var("WAYLAND_DISPLAY").is_some();
        Self {
            tz: TimeZone::system(),
            now: Timestamp::now(),
            style: Style { color },
            pwd: logical_pwd(),
            home: ctx.env.home.clone(),
            server: ServerInfo::read_live(&ctx.paths.runtime_dir),
            can_open: graphical && var("SSH_CONNECTION").is_none() && ctx.env.euid != 0,
        }
    }
}

fn logical_pwd() -> Option<PathBuf> {
    let cwd = std::env::current_dir().ok()?;
    let same = |a: &Path, b: &Path| {
        use std::os::unix::fs::MetadataExt;
        match (std::fs::metadata(a), std::fs::metadata(b)) {
            (Ok(a), Ok(b)) => a.dev() == b.dev() && a.ino() == b.ino(),
            _ => false,
        }
    };
    match std::env::var_os("PWD").map(PathBuf::from) {
        Some(pwd) if pwd.is_absolute() && same(&pwd, &cwd) => Some(pwd),
        _ => Some(cwd),
    }
}

/// Whether another process holds an flock on `path` (a missing file is not held).
fn lock_held(path: &Path) -> bool {
    use nix::fcntl::{Flock, FlockArg};
    let Ok(file) = std::fs::File::open(path) else {
        return false;
    };
    match Flock::lock(file, FlockArg::LockSharedNonblock) {
        Ok(_unlocked_on_drop) => false,
        Err((_, errno)) => errno == nix::errno::Errno::EWOULDBLOCK,
    }
}

// ------------------------------------------------------------------ databases

/// Access to one scope's index.db and meta.db.
pub trait Store {
    fn reader(&self) -> anyhow::Result<Box<dyn Searcher>>;
    fn meta(&self) -> anyhow::Result<Box<dyn MetaStore>>;
    /// The writer, `None` while an indexer holds the writer lock or before index.db exists.
    fn writer(&self) -> anyhow::Result<Option<Box<dyn IndexWriter>>>;
    fn has_index(&self) -> bool;
    /// Before a command forget: records in meta.db the span web playback masks for it (it
    /// is read from index.db, which the purge then drops). `false` when nothing was found.
    fn record_command_span(&self, _meta: &mut dyn MetaStore, _uuid: &str, _seq: u64) -> anyhow::Result<bool> {
        Ok(false)
    }
}

/// The real databases in `Paths::db_dir`.
pub struct DbStore {
    pub paths: DbPaths,
    pub index_cfg: IndexConfig,
}

impl DbStore {
    pub fn new(ctx: &Ctx) -> Self {
        Self {
            paths: DbPaths::in_dir(&ctx.paths.db_dir),
            index_cfg: ctx.config.index.clone(),
        }
    }
}

impl Store for DbStore {
    fn reader(&self) -> anyhow::Result<Box<dyn Searcher>> {
        if !self.has_index() {
            anyhow::bail!(
                "no index yet at {}: start the indexer with `cinelog serve` or run `cinelog index --once`",
                self.paths.index.display()
            );
        }
        Ok(db::open_reader(&self.paths)?)
    }

    fn meta(&self) -> anyhow::Result<Box<dyn MetaStore>> {
        Ok(db::open_meta(&self.paths)?)
    }

    fn writer(&self) -> anyhow::Result<Option<Box<dyn IndexWriter>>> {
        if !self.has_index() {
            return Ok(None);
        }
        match db::open_writer(&self.paths, &self.index_cfg) {
            Ok(w) => Ok(Some(w)),
            Err(IndexError::Locked(_)) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    fn has_index(&self) -> bool {
        self.paths.index.is_file()
    }

    fn record_command_span(&self, meta: &mut dyn MetaStore, uuid: &str, seq: u64) -> anyhow::Result<bool> {
        Ok(cinelog_web::play::record_forgotten_command_span(
            &self.paths,
            meta,
            uuid,
            seq,
        )?)
    }
}

#[cfg(test)]
pub(crate) mod testutil {
    //! A [`Store`] over `fake::MemIndex` with sessions, commands and output for the tests.

    use cinelog_index::fake::MemIndex;
    use cinelog_index::model::*;
    use cinelog_proto::Scope;

    use super::*;

    pub const U1: &str = "01934f5e-7b2a-7c3d-8e4f-0123456789ab";
    pub const U2: &str = "01934f5e-7b2a-7c3d-8e4f-0123456789cd";
    /// 2026-11-08T12:00:00Z
    pub const T0: f64 = 1_794_139_200.0;

    pub struct MemStore {
        pub mem: MemIndex,
        pub has_index: bool,
        /// Simulates a running indexer holding the writer lock.
        pub locked: bool,
    }

    impl Store for MemStore {
        fn reader(&self) -> anyhow::Result<Box<dyn Searcher>> {
            anyhow::ensure!(self.has_index, "no index yet");
            Ok(Box::new(self.mem.clone()))
        }
        fn meta(&self) -> anyhow::Result<Box<dyn MetaStore>> {
            Ok(Box::new(self.mem.clone()))
        }
        fn writer(&self) -> anyhow::Result<Option<Box<dyn IndexWriter>>> {
            Ok((self.has_index && !self.locked).then(|| Box::new(self.mem.clone()) as Box<dyn IndexWriter>))
        }
        fn has_index(&self) -> bool {
            self.has_index
        }
    }

    pub fn env() -> Env {
        Env {
            tz: TimeZone::UTC,
            now: Timestamp::from_second(T0 as i64 + 3600).unwrap(),
            style: Style::PLAIN,
            pwd: Some("/home/alice/src".into()),
            home: "/home/alice".into(),
            server: None,
            can_open: false,
        }
    }

    pub fn session(uuid: &str, user: &str, started: f64) -> SessionMeta {
        SessionMeta {
            uuid: uuid.into(),
            host: "box".into(),
            scope: Scope::Personal,
            user: user.into(),
            euid: Some(1000),
            sudo_user: None,
            claimed_user: None,
            attribution: None,
            login_via: None,
            tty: Some("/dev/pts/3".into()),
            holder_comm: Some("tmux: server".into()),
            tmux: TmuxLocation {
                session: Some("main".into()),
                window: Some("1".into()),
                pane: Some("%3".into()),
                ..TmuxLocation::default()
            },
            parent_uuid: None,
            cast_path: format!("/home/{user}/logs/2026-11/box_{uuid}.cast").into(),
            parts: 1,
            compressed: false,
            cast_bytes: None,
            started_at: started,
            ended_at: None,
            exit_status: None,
            tz: Some("UTC".into()),
            cols: 80,
            rows: 24,
            term: Some("xterm-256color".into()),
            legacy: false,
            hidden: false,
            state: SessionState::Live,
            counters: SessionCounters::default(),
        }
    }

    pub fn command(seq: Seq, t: f64, typed: &str, exit: Option<i32>, cwd: &str) -> Command {
        Command {
            seq,
            parent_seq: None,
            depth: 0,
            shell_pid: Some(42),
            t_start: t,
            t_end: exit.map(|_| t + 2.5),
            ts_start: T0 + t,
            ts_end: exit.map(|_| T0 + t + 2.5),
            exit,
            pipestatus: exit.map(|e| vec![e]).unwrap_or_default(),
            cwd: Some(cwd.into()),
            user: None,
            euid: Some(1000),
            typed: typed.into(),
            expanded: None,
            typed_trunc: false,
            approx: false,
            confidence: None,
            out_bytes: 0,
            out_truncated: false,
        }
    }

    pub fn chunk(cmd_seq: Option<Seq>, t: f64, text: &str) -> Chunk {
        Chunk {
            kind: if cmd_seq.is_some() {
                ChunkKind::Primary
            } else {
                ChunkKind::Gap
            },
            cmd_seq,
            t,
            ts: T0 + t,
            line0: 0,
            text: text.into(),
        }
    }

    /// Two sessions: U1 (alice, pane %3) with three commands and output, U2 (bob).
    pub fn batches() -> [IndexBatch; 2] {
        let mut b = IndexBatch::new(session(U1, "alice", T0));
        b.commands = vec![
            command(1, 1.0, "ls -la /tmp", Some(0), "/home/alice/src"),
            command(2, 10.0, "grep needle notes.txt", Some(1), "/home/alice/src"),
            command(3, 20.0, "make\ttest", None, "/tmp"),
        ];
        b.chunks = vec![
            chunk(Some(1), 1.1, "total 0\ndrwxrwxrwt  2 root root 40 tmp"),
            chunk(Some(2), 10.1, "the needle is \x1b[31mhere\x1b[0m"),
            chunk(None, 15.0, "alice@box ~/src % "),
        ];
        let mut b2 = IndexBatch::new(session(U2, "bob", T0 + 100.0));
        b2.commands = vec![command(1, 101.0, "cat needle.log", Some(0), "/srv")];
        [b, b2]
    }

    /// [`batches`] in a `fake::MemIndex`.
    pub fn store() -> MemStore {
        let mut mem = MemIndex::new();
        for b in batches() {
            mem.apply(&b).unwrap();
        }
        MemStore {
            mem,
            has_index: true,
            locked: false,
        }
    }

    /// Runs `f` with captured stdout/stderr.
    pub fn capture(f: impl FnOnce(&mut dyn Write, &mut dyn Write) -> anyhow::Result<u8>) -> (u8, String, String) {
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let c = f(&mut out, &mut err).unwrap();
        (c, String::from_utf8(out).unwrap(), String::from_utf8(err).unwrap())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lock_detection() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("index.db.writer.lock");
        assert!(!lock_held(&p), "missing file");
        let f = std::fs::File::create(&p).unwrap();
        assert!(!lock_held(&p), "unlocked file");
        let held = nix::fcntl::Flock::lock(f, nix::fcntl::FlockArg::LockExclusiveNonblock).unwrap();
        assert!(lock_held(&p));
        drop(held);
        assert!(!lock_held(&p));
    }

    /// search, show and forget through [`DbStore`] on a real index.db and meta.db.
    #[test]
    fn real_databases() {
        use clap::Parser;

        use super::testutil::{U1, U2, batches, capture, env};
        use crate::cli::{Cli, Command};

        let dir = tempfile::tempdir().unwrap();
        let store = DbStore {
            paths: DbPaths::in_dir(dir.path()),
            index_cfg: IndexConfig::default(),
        };
        {
            let mut w = db::open_writer(&store.paths, &store.index_cfg).unwrap();
            for b in batches() {
                w.apply(&b).unwrap();
            }
        }
        let parse = |argv: &[&str]| Cli::try_parse_from([&["cinelog"], argv].concat()).unwrap().command;
        let search = |argv: &[&str]| {
            let Command::Search(a) = parse(&[&["search", "--json"], argv].concat()) else {
                unreachable!()
            };
            let (c, out, err) = capture(|o, e| list::search(&a, &store, &env(), list::Ui::Json, o, e));
            assert!(c <= 1, "{err}");
            out.lines()
                .map(|l| {
                    serde_json::from_str::<serde_json::Value>(l).unwrap()["entry"]
                        .as_str()
                        .unwrap()
                        .to_owned()
                })
                .collect::<Vec<_>>()
        };
        let show = |id: &str| {
            let Command::Show(a) = parse(&["show", id]) else {
                unreachable!()
            };
            capture(|o, e| show::show(&a, &store, &env(), o, e))
        };
        let forget = |id: &str| {
            let Command::Forget(a) = parse(&["forget", id]) else {
                unreachable!()
            };
            capture(|o, e| forget::forget(&a, &store, "alice", o, e))
        };

        let mut all = search(&["-a", "needle"]);
        all.sort();
        all.dedup();
        assert!(
            all.contains(&format!("{U1}:2")) && all.contains(&format!("{U2}:1")),
            "{all:?}"
        );
        assert_eq!(search(&["needle"]).len(), 2, "commands only");

        let (c, out, _) = show(&format!("{U1}:2"));
        assert_eq!(c, 0);
        assert!(
            out.contains("grep needle notes.txt") && out.contains("the needle is"),
            "{out}"
        );

        // A running indexer holds the writer lock: the reader hides the session at once.
        let held = db::open_writer(&store.paths, &store.index_cfg).unwrap();
        let (c, out, err) = forget(U2);
        assert_eq!(c, 0, "{err}");
        assert!(out.contains("the running indexer removes it from index.db"), "{out}");
        assert!(
            !search(&["-a", "needle"]).iter().any(|e| e.starts_with(U2)),
            "hidden while locked"
        );
        assert_eq!(show(&format!("{U2}:1")).0, 1, "show hides it too");
        drop(held);

        // With the lock free, forget purges index.db itself, the earlier U2 entry included.
        let (c, out, err) = forget(&format!("{U1}:2"));
        assert_eq!(c, 0, "{err}");
        assert!(
            out.contains("removed from the index: 1 sessions, 2 commands, 1 output chunks"),
            "{out}"
        );
        assert!(search(&["-a", "needle"]).is_empty(), "{:?}", search(&["-a", "needle"]));
        assert_eq!(search(&["ls"]), [format!("{U1}:1")]);
        let forgotten = store.meta().unwrap().forgotten().unwrap();
        assert_eq!(forgotten.len(), 2);
        assert!(forgotten.iter().all(|f| f.by == "alice"));
    }

    #[test]
    fn missing_index_is_explained() {
        let dir = tempfile::tempdir().unwrap();
        let store = DbStore {
            paths: DbPaths::in_dir(dir.path()),
            index_cfg: IndexConfig::default(),
        };
        let e = store.reader().err().unwrap().to_string();
        assert!(e.contains("no index yet") && e.contains("cinelog serve"), "{e}");
        assert!(store.writer().unwrap().is_none(), "never creates index.db");
        assert!(!dir.path().join("index.db").exists());
    }
}
