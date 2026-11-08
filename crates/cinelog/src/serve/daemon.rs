//! The scheduler of `cinelog serve` (§6.4, §6.6): one loop that owns discovery and the
//! sessions, with the backfill pool and the compressor running beside it.
//!
//! - Registry scan every second: live recorders are opened at once as [`OpenSession`]s
//!   (they bypass the backfill queue, which gives them priority); their casts are stat'ed
//!   every [`DaemonConfig::stat_every`] and read as soon as they grow.
//! - Directory walk every minute: new or changed casts are planned ([`Job::plan`], which
//!   reads no cast content) and fed to the [`Pool`], cinelog casts first, newest first;
//!   open sessions keep being read while the walk runs. A backfilled cast without `x`
//!   continues as an open session while its recorder lives, else it is closed as crashed
//!   (or degraded).
//! - meta.forgotten is re-applied whenever it changes (`cinelog forget` while we hold the
//!   writer), the meta backup is written once a day, and the compressor runs daily (or on
//!   SIGUSR1, sent by `cinelog compress --now`).

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use cinelog_core::Scope;
use cinelog_core::registry::ProcessProbe;
use cinelog_index::model::{DbPaths, Forgotten, KnownCast, SessionState};
use cinelog_index::{IndexError, IndexWriter, MetaStore};

use super::backup;
use super::compress::{self, CompressOpts, CompressReport};
use super::discover::{FoundKind, LiveEntry, Walker, scan_registry};
use super::engine::{
    BACKFILL_POLL_BYTES, CRASH_GRACE, CommitClock, Fingerprint, Job, JobOutcome, JobStatus, Pool, SharedWriter,
    Sources, default_workers, lock, note_error, one_line, recorder_gone, shared, watched_part,
};
use super::live::OpenSession;

/// Settings and intervals of one scope's daemon.
#[derive(Debug, Clone)]
pub struct DaemonConfig {
    pub scope: Scope,
    pub logs_root: PathBuf,
    pub registry_dirs: Vec<PathBuf>,
    pub db: DbPaths,
    /// `<db_dir>/migrated-port`, imported once.
    pub migrated_port: PathBuf,
    pub workers: usize,
    pub poll_bytes: u64,
    /// How often open casts are stat'ed (growth is read within this).
    pub stat_every: Duration,
    pub registry_every: Duration,
    pub walk_every: Duration,
    pub commit_every: Duration,
    pub forget_every: Duration,
    /// Meta backup check.
    pub housekeeping_every: Duration,
    pub crash_grace: Duration,
    pub compress: CompressOpts,
    /// First compressor run after startup (the backfill goes first).
    pub compress_first: Duration,
    pub compress_every: Duration,
    /// Pause before a live session that failed to open or read is tried again.
    pub retry_after: Duration,
}

impl DaemonConfig {
    /// Production intervals (§6.4: registry 1 s, growth within 20 ms, walk 60 s, commit 2 s).
    pub fn new(
        scope: Scope,
        logs_root: PathBuf,
        registry_dirs: Vec<PathBuf>,
        db: DbPaths,
        migrated_port: PathBuf,
        compress: CompressOpts,
    ) -> Self {
        Self {
            scope,
            logs_root,
            registry_dirs,
            db,
            migrated_port,
            workers: default_workers(),
            poll_bytes: BACKFILL_POLL_BYTES,
            stat_every: Duration::from_millis(20),
            registry_every: Duration::from_secs(1),
            walk_every: Duration::from_secs(60),
            commit_every: cinelog_index::model::COMMIT_INTERVAL,
            forget_every: Duration::from_secs(5),
            housekeeping_every: Duration::from_secs(600),
            crash_grace: CRASH_GRACE,
            compress,
            compress_first: Duration::from_secs(600),
            compress_every: Duration::from_secs(86_400),
            retry_after: Duration::from_secs(10),
        }
    }
}

/// Flags set by signal handlers (or tests).
#[derive(Debug, Clone, Default)]
pub struct Signals {
    pub stop: Arc<AtomicBool>,
    pub compress_now: Arc<AtomicBool>,
}

impl Signals {
    /// SIGTERM, SIGINT and SIGHUP stop the daemon; SIGUSR1 runs the compressor now.
    pub fn install() -> std::io::Result<Self> {
        use signal_hook::consts::{SIGHUP, SIGINT, SIGTERM, SIGUSR1};
        let s = Self::default();
        for sig in [SIGTERM, SIGINT, SIGHUP] {
            signal_hook::flag::register(sig, s.stop.clone())?;
        }
        signal_hook::flag::register(SIGUSR1, s.compress_now.clone())?;
        Ok(s)
    }
}

/// Counters of one daemon run (logged at shutdown, inspected by tests).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RunStats {
    pub backfilled: u64,
    pub failed: u64,
    pub live_opened: u64,
    pub ended: u64,
    pub crashed: u64,
    pub compress_runs: u64,
    pub compressed: u64,
    pub backups: u64,
}

pub struct Daemon {
    cfg: DaemonConfig,
    writer: SharedWriter,
    meta: Box<dyn MetaStore>,
    sources: Arc<Sources>,
    probe: Box<dyn ProcessProbe + Send>,
    clock: Arc<CommitClock>,
    walker: Walker,
    /// Sessions with an open cast, by session key.
    open: HashMap<PathBuf, OpenSession>,
    /// Keys handed to the pool, with their uuid when known.
    in_flight: HashMap<PathBuf, Option<String>>,
    queue: VecDeque<Job>,
    queued: HashSet<PathBuf>,
    /// What the last finished job or plan saw of each cast: unchanged casts are not planned
    /// again.
    seen: HashMap<PathBuf, Fingerprint>,
    /// Live sessions that failed to open or read, not retried before the instant.
    retry: HashMap<PathBuf, Instant>,
    /// Uuids of sessions that ended (read to their `x`) while their recorder may still be
    /// registered: a recorder writes `x` before it removes its entry, and such a session
    /// must not be reopened. Pruned once the entry is gone.
    settled: HashSet<String>,
    last_tick: Instant,
    /// Last registry view by uuid.
    live: HashMap<String, LiveEntry>,
    forgotten: Option<Vec<Forgotten>>,
    compressor: Option<JoinHandle<CompressReport>>,
    stats: RunStats,
}

impl Daemon {
    pub fn new(
        writer: Box<dyn IndexWriter>,
        meta: Box<dyn MetaStore>,
        sources: Sources,
        probe: Box<dyn ProcessProbe + Send>,
        cfg: DaemonConfig,
    ) -> Self {
        Self {
            cfg,
            writer: shared(writer),
            meta,
            sources: Arc::new(sources),
            probe,
            clock: Arc::new(CommitClock::default()),
            walker: Walker::new(),
            open: HashMap::new(),
            in_flight: HashMap::new(),
            queue: VecDeque::new(),
            queued: HashSet::new(),
            seen: HashMap::new(),
            retry: HashMap::new(),
            settled: HashSet::new(),
            last_tick: Instant::now(),
            live: HashMap::new(),
            forgotten: None,
            compressor: None,
            stats: RunStats::default(),
        }
    }

    fn busy(&self, key: &Path) -> bool {
        self.open.contains_key(key) || self.in_flight.contains_key(key) || self.queued.contains(key)
    }

    /// Runs until `signals.stop` is set. Pending live data is committed on the way out;
    /// sessions stay `live` and resume from their checkpoints on the next start.
    pub fn run(mut self, signals: &Signals) -> Result<RunStats, IndexError> {
        let started = Instant::now();
        self.startup();
        let pool = Pool::start(
            self.cfg.workers,
            self.writer.clone(),
            self.clock.clone(),
            self.sources.clone(),
            self.cfg.poll_bytes,
            Arc::new(AtomicBool::new(false)),
        );
        let now = Instant::now();
        let mut next_registry = now + self.cfg.registry_every;
        let mut next_walk = now + self.cfg.walk_every;
        let mut next_forget = now + self.cfg.forget_every;
        let mut next_house = now + self.cfg.housekeeping_every;
        let mut next_compress = now + self.cfg.compress_first;
        while !signals.stop.load(Ordering::Relaxed) {
            let now = Instant::now();
            if now >= next_registry {
                self.scan_registry();
                next_registry = now + self.cfg.registry_every;
            }
            if now >= next_walk {
                self.walk(false);
                next_walk = now + self.cfg.walk_every;
            }
            if now >= next_forget {
                self.sync_forgotten();
                next_forget = now + self.cfg.forget_every;
            }
            if now >= next_house {
                self.housekeeping();
                next_house = now + self.cfg.housekeeping_every;
            }
            if signals.compress_now.swap(false, Ordering::Relaxed) || now >= next_compress {
                self.start_compressor(&signals.stop);
                next_compress = now + self.cfg.compress_every;
            }
            self.reap_compressor(false);
            self.feed(&pool);
            self.live_tick();
            match pool.done().recv_timeout(self.cfg.stat_every) {
                Ok(o) => self.outcome(o),
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
                Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
                    return Err(IndexError::Invalid("the index writer thread stopped".into()));
                }
            }
            let done: Vec<JobOutcome> = pool.done().try_iter().collect();
            for o in done {
                self.outcome(o);
            }
        }
        tracing::info!("stopping");
        for o in pool.shutdown(true) {
            if !matches!(o.status, JobStatus::Incomplete(_)) {
                self.outcome(o);
            }
        }
        for (key, s) in &mut self.open {
            if let Err(e) = s.flush(&self.writer, &self.clock) {
                tracing::warn!("{}: final commit: {e}", key.display());
            }
        }
        self.reap_compressor(true);
        tracing::info!(
            "stopped after {:.0} s: {:?}, {} commits",
            started.elapsed().as_secs_f64(),
            self.stats,
            self.clock.commits()
        );
        Ok(self.stats)
    }

    fn startup(&mut self) {
        match backup::import_migrated_port(self.meta.as_mut(), &self.cfg.migrated_port) {
            Ok(Some(port)) => tracing::info!("imported the migrated port {port}"),
            Ok(None) => {}
            Err(e) => tracing::warn!("migrated port: {e}"),
        }
        self.sync_forgotten();
        // Sessions the index has as ended stay closed, also if their recorder is still in
        // the registry (`scan_registry` keeps only those).
        match lock(&self.writer).known_casts() {
            Ok(k) => {
                self.settled = k
                    .into_iter()
                    .filter(|k| k.state == SessionState::Ended)
                    .map(|k| k.uuid)
                    .collect();
            }
            Err(e) => note_error(&self.writer, &one_line(format!("known casts: {e}"))),
        }
        self.scan_registry();
        self.walk(true);
        self.housekeeping();
    }

    // -------------------------------------------------------------- live sessions

    fn scan_registry(&mut self) {
        let entries = scan_registry(&self.cfg.registry_dirs, self.cfg.scope, self.probe.as_ref());
        self.live = entries.into_iter().map(|e| (e.uuid.clone(), e)).collect();
        let now = Instant::now();
        self.retry.retain(|_, until| *until > now);
        self.settled.retain(|u| self.live.contains_key(u));
        let to_open: Vec<LiveEntry> = self
            .live
            .values()
            .filter(|e| e.alive && !self.open.contains_key(&e.key) && !self.in_flight.contains_key(&e.key))
            .filter(|e| !self.retry.contains_key(&e.key) && !self.settled.contains(&e.uuid))
            .cloned()
            .collect();
        for e in to_open {
            self.open_live(e);
        }
        let mut gone = Vec::new();
        for (key, s) in &mut self.open {
            s.set_entry(self.live.get(&s.uuid).cloned());
            s.watched = watched_part(key, s.entry.as_ref());
            if recorder_gone(s.entry.as_ref(), &s.watched, self.cfg.crash_grace) {
                gone.push(key.clone());
            }
        }
        for key in gone {
            if let Some(s) = self.open.remove(&key) {
                self.close(key, s);
            }
        }
    }

    fn open_live(&mut self, e: LiveEntry) {
        let first = cinelog_cast::find_parts(&e.key)
            .into_iter()
            .next()
            .unwrap_or_else(|| e.key.clone());
        let opened = self
            .sources
            .factory(&FoundKind::Cinelog { uuid: e.uuid.clone() })
            .map_err(IndexError::Invalid)
            .and_then(|f| {
                let checkpoint = lock(&self.writer).checkpoint(&e.uuid)?;
                f.open(&first, checkpoint)
            });
        match opened {
            Ok(p) => {
                if self.queued.remove(&e.key) {
                    self.queue.retain(|j| j.found.key != e.key);
                }
                tracing::debug!("live session {} ({})", e.uuid, e.cast.display());
                let watched = watched_part(&e.key, Some(&e));
                self.stats.live_opened += 1;
                let s = OpenSession::new(e.uuid.clone(), p, Some(e.clone()), watched, false);
                self.open.insert(e.key, s);
            }
            Err(err) => {
                // A recorder registers right after creating its cast: the header may not
                // be there yet. Only a persistent failure is worth a log line.
                tracing::debug!("cannot open live session {}: {err}", e.cast.display());
                self.retry.insert(e.key, Instant::now() + self.cfg.retry_after);
            }
        }
    }

    fn live_tick(&mut self) {
        self.last_tick = Instant::now();
        let mut finished = Vec::new();
        let mut failed = Vec::new();
        for (key, s) in &mut self.open {
            let step = s
                .read_growth()
                .and_then(|_| s.commit_due(&self.writer, &self.clock, self.cfg.commit_every));
            match step {
                Ok(_) if s.is_finished() => finished.push(key.clone()),
                Ok(_) => {}
                Err(e) => failed.push((key.clone(), e)),
            }
        }
        for key in finished {
            if let Some(s) = self.open.remove(&key) {
                self.settled.insert(s.uuid);
            }
            self.stats.ended += 1;
            self.remember(&key);
        }
        for (key, e) in failed {
            // Dropping the pipeline loses only what was not committed: the session is
            // reopened from its last checkpoint after `retry_after`.
            self.open.remove(&key);
            note_error(&self.writer, &one_line(format!("{}: {e}", key.display())));
            self.retry.insert(key, Instant::now() + self.cfg.retry_after);
        }
    }

    /// Closes a session whose recorder is gone: `ended` if the cast has its `x`, else
    /// `crashed` (or `degraded`, if the recorder or the index last said so).
    fn close(&mut self, key: PathBuf, s: OpenSession) {
        let uuid = s.uuid.clone();
        match s.finalize(&self.writer, &self.clock) {
            Ok(SessionState::Ended) => {
                self.stats.ended += 1;
                self.settled.insert(uuid);
            }
            Ok(state) => {
                tracing::info!("session {uuid} is {state}: its recorder is gone and the cast has no exit event");
                self.stats.crashed += 1;
            }
            Err(e) => note_error(&self.writer, &one_line(format!("{}: {e}", key.display()))),
        }
        self.remember(&key);
    }

    fn remember(&mut self, key: &Path) {
        let mut walk = self.walker.collect(&[key.to_path_buf()]);
        if let Some(found) = walk.casts.pop() {
            self.seen.insert(key.to_path_buf(), Fingerprint::of(&found));
        }
    }

    // -------------------------------------------------------------- backfill

    fn walk(&mut self, startup: bool) {
        let r = self.walker.walk(&self.cfg.logs_root);
        if startup {
            let busy: HashSet<PathBuf> = self
                .open
                .keys()
                .cloned()
                .chain(self.live.values().map(|e| e.key.clone()))
                .collect();
            let rec = compress::reconcile(&self.writer, &r.duplicates, &r.tmp_files, &busy);
            if rec.tmp_removed + rec.kept_zst + rec.kept_plain > 0 {
                tracing::info!("interrupted compressions cleaned up: {rec:?}");
            }
            if let Some(e) = rec.errors.last() {
                note_error(&self.writer, e);
            }
        }
        let known: HashMap<String, KnownCast> = match lock(&self.writer).known_casts() {
            Ok(k) => k.into_iter().map(|k| (k.uuid.clone(), k)).collect(),
            Err(e) => {
                note_error(&self.writer, &one_line(format!("known casts: {e}")));
                return;
            }
        };
        let mut planned = 0;
        for found in r.casts {
            if self.last_tick.elapsed() >= self.cfg.stat_every {
                self.live_tick();
            }
            if self.busy(&found.key) {
                continue;
            }
            let fp = Fingerprint::of(&found);
            if self.seen.get(&found.key) == Some(&fp) {
                continue;
            }
            let key = found.key.clone();
            let plan = {
                let w = lock(&self.writer);
                Job::plan(found, &known, w.as_ref(), false)
            };
            match plan {
                Ok(Some(job)) => {
                    self.queued.insert(key);
                    self.queue.push_back(job);
                    planned += 1;
                }
                Ok(None) => {
                    self.seen.insert(key, fp);
                }
                Err(e) => note_error(&self.writer, &one_line(format!("{}: {e}", key.display()))),
            }
        }
        if planned > 0 {
            Job::sort(self.queue.make_contiguous());
            tracing::info!("{planned} casts to index ({} queued)", self.queue.len());
        }
    }

    fn feed(&mut self, pool: &Pool) {
        while let Some(job) = self.queue.pop_front() {
            let key = job.found.key.clone();
            let uuid = job.uuid.clone();
            match pool.try_submit(job) {
                Ok(()) => {
                    self.queued.remove(&key);
                    self.in_flight.insert(key, uuid);
                }
                Err(job) => {
                    self.queue.push_front(*job);
                    break;
                }
            }
        }
    }

    fn outcome(&mut self, mut o: JobOutcome) {
        let key = o.found.key.clone();
        self.in_flight.remove(&key);
        match std::mem::replace(&mut o.status, JobStatus::Stopped) {
            JobStatus::Finished => {
                self.stats.backfilled += 1;
                self.seen.insert(key, Fingerprint::of(&o.found));
                if let (Some(u), false) = (o.uuid, o.found.is_legacy()) {
                    self.settled.insert(u);
                }
            }
            JobStatus::Failed(e) => {
                self.stats.failed += 1;
                note_error(&self.writer, &one_line(format!("{}: {e}", o.found.path.display())));
                self.seen.insert(key, Fingerprint::of(&o.found));
            }
            JobStatus::Stopped => {}
            JobStatus::Incomplete(p) => {
                self.stats.backfilled += 1;
                let uuid = o.uuid.unwrap_or_else(|| p.uuid().to_owned());
                let entry = self.live.get(&uuid).cloned();
                let watched = watched_part(&key, entry.as_ref());
                let gone = recorder_gone(entry.as_ref(), &watched, self.cfg.crash_grace);
                let degraded = o.prior_state == Some(SessionState::Degraded);
                let s = OpenSession::new(uuid, p, entry, watched, degraded);
                if gone {
                    self.close(key, s);
                } else {
                    self.open.insert(key, s);
                }
            }
        }
    }

    // -------------------------------------------------------------- housekeeping

    fn sync_forgotten(&mut self) {
        let list = match self.meta.forgotten() {
            Ok(l) => l,
            Err(e) => {
                tracing::warn!("meta.forgotten: {e}");
                return;
            }
        };
        if self.forgotten.as_ref() == Some(&list) {
            return;
        }
        match lock(&self.writer).apply_forgotten(&list) {
            Ok(out) => {
                if out.sessions + out.commands + out.outputs > 0 {
                    tracing::info!("forgotten: {out:?}");
                }
                self.forgotten = Some(list);
            }
            Err(e) => tracing::warn!("applying meta.forgotten: {e}"),
        }
    }

    fn housekeeping(&mut self) {
        let today = jiff::Zoned::now().date();
        match backup::nightly(self.meta.as_mut(), &self.cfg.db, today) {
            Ok(Some(p)) => {
                self.stats.backups += 1;
                tracing::info!("meta backup {}", p.display());
            }
            Ok(None) => {}
            Err(e) => note_error(&self.writer, &one_line(format!("meta backup: {e}"))),
        }
    }

    fn start_compressor(&mut self, stop: &Arc<AtomicBool>) {
        if self.compressor.is_some() {
            return;
        }
        let skip: HashSet<String> = self
            .open
            .values()
            .map(|s| s.uuid.clone())
            .chain(self.in_flight.values().flatten().cloned())
            .collect();
        let (w, opts, stop) = (self.writer.clone(), self.cfg.compress, stop.clone());
        let spawned = thread::Builder::new()
            .name("cinelog-compress".into())
            .spawn(move || match compress::candidates(&w, &opts, &skip) {
                Ok(list) => compress::compress(&w, &list, opts.level, &stop),
                Err(e) => CompressReport {
                    errors: vec![one_line(format!("compress candidates: {e}"))],
                    ..CompressReport::default()
                },
            });
        match spawned {
            Ok(h) => self.compressor = Some(h),
            Err(e) => tracing::warn!("cannot start the compressor: {e}"),
        }
    }

    fn reap_compressor(&mut self, wait: bool) {
        if !self.compressor.as_ref().is_some_and(|h| wait || h.is_finished()) {
            return;
        }
        let Some(h) = self.compressor.take() else { return };
        let Ok(r) = h.join() else {
            note_error(&self.writer, "the compressor panicked");
            return;
        };
        self.stats.compress_runs += 1;
        self.stats.compressed += r.compressed;
        if r.compressed > 0 {
            tracing::info!(
                "compressed {} casts: {} → {} bytes",
                r.compressed,
                r.bytes_in,
                r.bytes_out
            );
        }
        if let Some(e) = r.errors.last() {
            note_error(&self.writer, e);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::serve::testkit::*;
    use cinelog_core::RecState;
    use cinelog_index::fake::MemIndex;
    use cinelog_index::model::{ChunkKind, ForgetTarget, settings_keys};

    struct Harness {
        dir: tempfile::TempDir,
        mem: MemIndex,
        probe: FakeProbe,
        signals: Signals,
        handle: Option<JoinHandle<Result<RunStats, IndexError>>>,
    }

    impl Harness {
        fn new() -> Self {
            Self {
                dir: tempfile::tempdir().unwrap(),
                mem: MemIndex::new(),
                probe: FakeProbe::alive(&[]),
                signals: Signals::default(),
                handle: None,
            }
        }

        fn logs(&self) -> PathBuf {
            self.dir.path().join("logs")
        }

        fn run_dir(&self) -> PathBuf {
            self.dir.path().join("run")
        }

        fn db(&self) -> DbPaths {
            DbPaths::in_dir(self.dir.path().join("db"))
        }

        fn start(&mut self) {
            self.signals = Signals::default();
            std::fs::create_dir_all(self.db().dir).unwrap();
            let mut cfg = DaemonConfig::new(
                Scope::Personal,
                self.logs(),
                vec![self.run_dir()],
                self.db(),
                self.db().dir.join("migrated-port"),
                CompressOpts {
                    after_days: 14,
                    level: 3,
                },
            );
            cfg.workers = 2;
            cfg.poll_bytes = 256;
            cfg.stat_every = Duration::from_millis(5);
            cfg.registry_every = Duration::from_millis(20);
            cfg.walk_every = Duration::from_millis(100);
            cfg.commit_every = Duration::from_millis(50);
            cfg.forget_every = Duration::from_millis(20);
            cfg.crash_grace = Duration::from_millis(300);
            let d = Daemon::new(
                Box::new(self.mem.clone()),
                Box::new(self.mem.clone()),
                test_sources(),
                Box::new(self.probe.clone()),
                cfg,
            );
            let signals = self.signals.clone();
            self.handle = Some(thread::spawn(move || d.run(&signals)));
        }

        fn stop(&mut self) -> RunStats {
            self.signals.stop.store(true, Ordering::Relaxed);
            self.handle.take().unwrap().join().unwrap().unwrap()
        }

        fn state(&self, uuid: &str) -> Option<SessionState> {
            self.mem
                .sessions()
                .into_iter()
                .find(|s| s.uuid == uuid)
                .map(|s| s.state)
        }
    }

    impl Drop for Harness {
        fn drop(&mut self) {
            self.signals.stop.store(true, Ordering::Relaxed);
            if let Some(h) = self.handle.take() {
                let _ = h.join();
            }
        }
    }

    fn wait_for(what: &str, mut cond: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !cond() {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn backfills_follows_live_sessions_and_detects_crashes() {
        let mut h = Harness::new();
        let month = h.logs().join("2026-11");
        let done = [
            (0.1, "m", cmd_json(1, 7, 1, "make")),
            (0.2, "o", "ok\r\n".to_owned()),
            (0.3, "m", done_json(1, 7, 1, 0)),
        ];
        let ev: Vec<(f64, &str, &str)> = done.iter().map(|(t, c, s)| (*t, *c, s.as_str())).collect();
        write_cast(&month, "h_a.cast", UUID_A, 1, &ev, Some(0));
        write_legacy(&h.logs(), "old.cast", &[(1.0, "o", "$ ls\r\n")]);
        write_legacy(&h.logs(), "FAILME.cast", &[(1.0, "o", "x\r\n")]);
        // B: a live recorder; C: an incomplete cast nobody records any more.
        let mut b = CastFile::create(&month, "h_b.cast", UUID_B, 1);
        b.flush();
        let reg = write_registry(&h.run_dir(), 3, 42, UUID_B, b.path());
        h.probe = FakeProbe::alive(&[(42, STARTTIME)]);
        write_cast(&month, "h_c.cast", UUID_C, 1, &[(0.1, "o", "$ \r\n")], None);
        h.start();

        wait_for("backfill", || h.state(UUID_A) == Some(SessionState::Ended));
        wait_for("legacy import", || h.mem.sessions().iter().any(|s| s.legacy));
        wait_for("crash of C", || h.state(UUID_C) == Some(SessionState::Crashed));
        wait_for("live B", || h.state(UUID_B) == Some(SessionState::Live));
        assert!(
            h.mem
                .info(cinelog_index::model::info_keys::LAST_ERROR)
                .unwrap()
                .contains("FAILME")
        );

        // A command in the live pane is searchable within the commit interval.
        b.cmd(1.0, 1, 9, 1, "cargo build");
        b.output(1.1, "Compiling\r\n");
        b.flush();
        wait_for("live command", || h.mem.commands(UUID_B).len() == 1);
        wait_for("provisional output", || {
            h.mem
                .chunks(UUID_B)
                .iter()
                .any(|c| c.kind == ChunkKind::Provisional && c.text.contains("Compiling"))
        });
        b.done(1.5, 1, 9, 1, 0);
        b.flush();
        wait_for("done", || h.mem.commands(UUID_B)[0].exit == Some(0));

        // The recorder dies without `x`: crashed once the registry says so.
        b.output(2.0, "last words\r\n");
        b.flush();
        h.probe.kill(42);
        wait_for("crash of B", || h.state(UUID_B) == Some(SessionState::Crashed));
        assert!(h.mem.chunks(UUID_B).iter().any(|c| c.text.contains("last words")));
        assert!(!h.mem.chunks(UUID_B).iter().any(|c| c.kind == ChunkKind::Provisional));
        std::fs::remove_file(reg).unwrap();

        let stats = h.stop();
        assert_eq!(stats.live_opened, 1, "{stats:?}");
        assert_eq!(stats.crashed, 2, "{stats:?}");
        assert!(stats.failed >= 1, "{stats:?}");
        assert_eq!(stats.backups, 1, "{stats:?}");
    }

    #[test]
    fn an_exit_ends_the_live_session_and_a_restart_resumes() {
        let mut h = Harness::new();
        let month = h.logs().join("2026-11");
        let mut b = CastFile::create(&month, "h_b.cast", UUID_B, 1);
        b.cmd(0.5, 1, 9, 1, "vim");
        b.flush();
        write_registry(&h.run_dir(), 3, 42, UUID_B, b.path());
        h.probe = FakeProbe::alive(&[(42, STARTTIME)]);
        h.start();
        wait_for("live command", || h.mem.commands(UUID_B).len() == 1);
        h.stop();
        assert_eq!(
            h.state(UUID_B),
            Some(SessionState::Live),
            "a shutdown leaves sessions live"
        );

        b.done(1.0, 1, 9, 1, 3);
        b.cmd(1.1, 2, 9, 2, "ls");
        b.flush();
        h.start();
        wait_for("resumed", || h.mem.commands(UUID_B).len() == 2);
        b.done(1.2, 2, 9, 2, 0);
        b.exit(1.3, 0);
        b.flush();
        wait_for("ended", || h.state(UUID_B) == Some(SessionState::Ended));
        let exits: Vec<_> = h.mem.commands(UUID_B).iter().map(|c| c.exit).collect();
        assert_eq!(exits, [Some(3), Some(0)]);
        let stats = h.stop();
        assert_eq!(stats.ended, 1, "{stats:?}");
    }

    #[test]
    fn a_finished_session_is_not_reopened_while_still_registered() {
        let mut h = Harness::new();
        let month = h.logs().join("2026-11");
        let mut b = CastFile::create(&month, "h_b.cast", UUID_B, 1);
        b.cmd(0.5, 1, 9, 1, "true");
        b.flush();
        // The recorder writes `x` before it removes its entry: here it never does.
        write_registry(&h.run_dir(), 3, 42, UUID_B, b.path());
        h.probe = FakeProbe::alive(&[(42, STARTTIME)]);
        h.start();
        wait_for("live command", || h.mem.commands(UUID_B).len() == 1);
        b.done(0.6, 1, 9, 1, 0);
        b.exit(0.7, 0);
        b.flush();
        wait_for("ended", || h.state(UUID_B) == Some(SessionState::Ended));
        thread::sleep(Duration::from_millis(200)); // ten registry scans
        let stats = h.stop();
        assert_eq!((stats.live_opened, stats.ended), (1, 1), "{stats:?}");

        // Also not after a restart, while the entry is still there.
        h.start();
        thread::sleep(Duration::from_millis(200));
        let stats = h.stop();
        assert_eq!(stats, RunStats::default());
        assert_eq!(h.state(UUID_B), Some(SessionState::Ended));
    }

    #[test]
    fn a_degraded_session_stays_degraded_across_restarts() {
        let mut h = Harness::new();
        let month = h.logs().join("2026-11");
        let mut b = CastFile::create(&month, "h_b.cast", UUID_B, 1);
        b.cmd(0.5, 1, 9, 1, "cat big");
        b.flush();
        let reg = write_registry_state(&h.run_dir(), 3, 42, UUID_B, b.path(), RecState::Degraded);
        h.probe = FakeProbe::alive(&[(42, STARTTIME)]);
        h.start();
        wait_for("degraded", || h.state(UUID_B) == Some(SessionState::Degraded));
        h.stop();

        // While serve is down, the recorder writes a little more, exits and unregisters.
        b.output(1.0, "tail\r\n");
        b.flush();
        h.probe.kill(42);
        std::fs::remove_file(reg).unwrap();
        h.start();
        wait_for("closed", || {
            h.mem.commands(UUID_B).first().is_some_and(|c| c.t_end.is_some())
        });
        assert_eq!(h.state(UUID_B), Some(SessionState::Degraded));
        assert!(h.mem.chunks(UUID_B).iter().any(|c| c.text.contains("tail")));
        let stats = h.stop();
        assert_eq!(stats.backfilled, 1, "{stats:?}");

        // Settled: an unchanged degraded cast is not read again.
        h.start();
        thread::sleep(Duration::from_millis(250));
        let stats = h.stop();
        assert_eq!(stats.backfilled, 0, "{stats:?}");
        assert_eq!(h.state(UUID_B), Some(SessionState::Degraded));
    }

    #[test]
    fn forget_port_and_compression_while_running() {
        let mut h = Harness::new();
        std::fs::create_dir_all(h.db().dir).unwrap();
        std::fs::write(h.db().dir.join("migrated-port"), "8123\n").unwrap();
        let month = h.logs().join("2026-10");
        let old = write_cast(&month, "h_d.cast", UUID_D, 1, &[(0.1, "o", "old\r\n")], Some(0));
        let forget_me = write_cast(&month, "h_a.cast", UUID_A, 1, &[(0.1, "o", "secret\r\n")], Some(0));
        h.start();
        // Both casts ended in 2024 (START_EPOCH), long before `compress_after_days`.
        wait_for("backfill", || {
            h.state(UUID_A) == Some(SessionState::Ended) && h.state(UUID_D) == Some(SessionState::Ended)
        });
        assert_eq!(h.mem.setting(settings_keys::PORT).unwrap().as_deref(), Some("8123"));

        h.mem
            .clone()
            .forget(&ForgetTarget::Session { uuid: UUID_A.into() }, "me")
            .unwrap();
        wait_for("forget", || h.state(UUID_A).is_none());
        assert!(forget_me.exists(), "casts are never touched by forget");

        h.signals.compress_now.store(true, Ordering::Relaxed);
        wait_for("compression", || {
            h.mem.sessions().iter().any(|s| s.uuid == UUID_D && s.compressed)
        });
        assert!(!old.exists() && compress::zst_path(&old).exists());
        let stats = h.stop();
        assert_eq!((stats.compress_runs, stats.compressed), (1, 1), "{stats:?}");
        assert!(
            !h.mem.sessions().iter().any(|s| s.uuid == UUID_A),
            "a forgotten session is not re-indexed"
        );
    }
}
