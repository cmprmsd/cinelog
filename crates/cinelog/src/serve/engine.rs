//! The indexing machinery shared by `cinelog serve` and `cinelog index` (§6.4): pipeline
//! sources, the plan that decides which casts need work, and the backfill pool (render
//! threads feeding one writer thread).

use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use cinelog_core::registry::ProcessProbe;
use cinelog_core::{RecState, Scope};
use cinelog_index::model::{
    Checkpoint, FileStamp, IndexBatch, KnownCast, LegacyImportRow, LegacyOptions, LegacyStatus, OnceOptions,
    OnceSummary, PollBudget, RenderOptions, SessionState, info_keys, legacy_uuid,
};
use cinelog_index::{IndexEngine, IndexError, IndexWriter, MetaStore, PipelineFactory, SessionPipeline, classify};
use crossbeam_channel::{Receiver, Sender, TrySendError};

use super::discover::{Found, FoundKind, LiveEntry, Walker, scan_registry};

/// The single index writer, shared by the writer thread and the live loop.
pub type SharedWriter = Arc<Mutex<Box<dyn IndexWriter>>>;

pub fn shared(writer: Box<dyn IndexWriter>) -> SharedWriter {
    Arc::new(Mutex::new(writer))
}

/// Locks the writer; a panic elsewhere must not stop indexing for good.
pub fn lock(w: &SharedWriter) -> MutexGuard<'_, Box<dyn IndexWriter>> {
    w.lock().unwrap_or_else(|e| e.into_inner())
}

/// Bytes one backfill poll may read (bounds memory per batch and the writer lock time).
pub const BACKFILL_POLL_BYTES: u64 = 4 << 20;
/// A cast without a registry entry is considered crashed once it has not changed for this
/// long (a recorder registers right after creating its cast).
pub const CRASH_GRACE: Duration = Duration::from_secs(30);
/// Size of the prefix hashed into a legacy session uuid.
const LEGACY_HASH_BYTES: u64 = 1 << 20;

pub fn now_ts() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// `min(4, cores/2)`, at least one (§6.4 "Backfill").
pub fn default_workers() -> usize {
    let cores = thread::available_parallelism().map(usize::from).unwrap_or(2);
    (cores / 2).clamp(1, 4)
}

/// One line, no cast content: what goes into `index_info.last_error` and the summary.
pub fn one_line(s: impl std::fmt::Display) -> String {
    s.to_string().replace(['\n', '\r'], " ")
}

/// Records `msg` as the indexer's last error (best effort).
pub fn note_error(w: &SharedWriter, msg: &str) {
    tracing::warn!("{msg}");
    if let Err(e) = lock(w).set_info(info_keys::LAST_ERROR, Some(msg)) {
        tracing::warn!("cannot record the last error: {e}");
    }
}

// ------------------------------------------------------------------ sources

/// Pipeline factories of one scope. A factory that cannot be built makes only its own
/// kind of cast fail.
pub struct Sources {
    pub cinelog: Result<Box<dyn PipelineFactory>, String>,
    pub legacy: Result<Box<dyn PipelineFactory>, String>,
}

impl Sources {
    pub fn new(render: RenderOptions, scope: Scope, logs_root: &Path, host: &str) -> Self {
        let legacy = LegacyOptions {
            render,
            scope,
            logs_root: logs_root.to_path_buf(),
            host: host.to_owned(),
        };
        Self {
            cinelog: cinelog_index::render::cast_pipelines(render).map_err(one_line),
            legacy: cinelog_index::legacy::pipelines(legacy).map_err(one_line),
        }
    }

    pub fn factory(&self, kind: &FoundKind) -> Result<&dyn PipelineFactory, String> {
        let f = match kind {
            FoundKind::Cinelog { .. } => &self.cinelog,
            FoundKind::Legacy => &self.legacy,
        };
        f.as_deref().map_err(Clone::clone)
    }
}

// ------------------------------------------------------------------ plan

/// What discovery saw of a session's files, to notice any change cheaply: the number of
/// parts and the stamp of the last one.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Fingerprint {
    pub parts: usize,
    pub last: Option<FileStamp>,
}

impl Fingerprint {
    pub fn of(found: &Found) -> Self {
        Self {
            parts: found.parts.len(),
            last: found.parts.last().and_then(|p| FileStamp::of(p).ok()),
        }
    }
}

/// Whether a cinelog cast has anything left to index, judged from the index. Only `live`
/// sessions always need a look; every other state (also `degraded`, which a restart cannot
/// tell from the registry any more) is settled until the files change.
pub fn cinelog_needs_work(found: &Found, known: Option<&KnownCast>) -> bool {
    let Some(k) = known else { return true };
    if k.state == SessionState::Live {
        // Possibly still growing, or a crash nobody has noticed yet.
        return true;
    }
    if found.parts.len() as u32 > k.parts.max(1) {
        return true;
    }
    let part = k.indexed_part.unwrap_or(k.parts).max(1);
    match found.plain_part_stamp(part) {
        // Compressed (or gone) since: the content is what was indexed.
        None => false,
        Some(now) => match k.indexed_file {
            Some(then) => !(then == now || (then.ino == now.ino && then.size == now.size)),
            None => true,
        },
    }
}

/// Whether a legacy cast still needs importing: no import row for this version of the
/// file, or a failed one. `serve` remembers what it planned, so it retries a failed import
/// once per start (or when the file changes); `cinelog index` retries it on every run.
pub fn legacy_needs_work(found: &Found, row: Option<&LegacyImportRow>) -> bool {
    row.is_none_or(|r| r.file != found.stamp || r.status == LegacyStatus::Failed)
}

/// Lowercase hex sha256 of the first MiB of `path`, read without touching its atime.
pub fn sha256_first_mib(path: &Path) -> std::io::Result<String> {
    use sha2::{Digest, Sha256};
    let mut buf = Vec::with_capacity(LEGACY_HASH_BYTES as usize);
    classify::open_noatime(path)?
        .take(LEGACY_HASH_BYTES)
        .read_to_end(&mut buf)?;
    Ok(data_encoding::HEXLOWER.encode(&Sha256::digest(&buf)))
}

/// The deterministic uuid a legacy cast is imported under (to find its checkpoint).
pub fn legacy_session_uuid(found: &Found) -> Option<String> {
    let name = found.path.file_name()?.to_str()?;
    let sha = sha256_first_mib(&found.path).ok()?;
    Some(legacy_uuid(name, found.stamp.size, &sha))
}

/// Import row of a legacy cast whose pipeline produced nothing.
pub fn failed_legacy_row(found: &Found, error: &str) -> LegacyImportRow {
    let log = cinelog_index::legacy::commands_log_path(&found.path);
    LegacyImportRow {
        path: found.path.clone(),
        file: found.stamp,
        sha256: sha256_first_mib(&found.path).unwrap_or_default(),
        commands_log: log.is_file().then_some(log),
        status: LegacyStatus::Failed,
        session_uuid: None,
        banners: 0,
        log_lines: 0,
        aligned: 0,
        mean_conf: None,
        error: Some(one_line(error)),
    }
}

/// A cast to index in the background.
pub struct Job {
    pub found: Found,
    /// Known up front for cinelog casts; a legacy cast's uuid hashes its first MiB, which
    /// the render worker does ([`run_job`]), not the planner.
    pub uuid: Option<String>,
    pub checkpoint: Option<Checkpoint>,
    /// The session's state in the index when the job was planned.
    pub prior_state: Option<SessionState>,
}

impl Job {
    /// Builds the job for `found`; `None` when the index is already up to date with it
    /// (`force`: always). Cheap: no cast content is read.
    pub fn plan(
        found: Found,
        known: &HashMap<String, KnownCast>,
        writer: &dyn IndexWriter,
        force: bool,
    ) -> Result<Option<Self>, IndexError> {
        match &found.kind {
            FoundKind::Cinelog { uuid } => {
                let k = known.get(uuid);
                if !force && !cinelog_needs_work(&found, k) {
                    return Ok(None);
                }
                let checkpoint = match k {
                    Some(_) => writer.checkpoint(uuid)?,
                    None => None,
                };
                Ok(Some(Self {
                    uuid: Some(uuid.clone()),
                    found,
                    checkpoint,
                    prior_state: k.map(|k| k.state),
                }))
            }
            FoundKind::Legacy => {
                if !force && !legacy_needs_work(&found, writer.legacy_entry(&found.path)?.as_ref()) {
                    return Ok(None);
                }
                Ok(Some(Self {
                    found,
                    uuid: None,
                    checkpoint: None,
                    prior_state: None,
                }))
            }
        }
    }

    /// Backfill order: cinelog casts before legacy ones, newest first.
    pub fn sort(jobs: &mut [Self]) {
        jobs.sort_by(|a, b| {
            a.found
                .is_legacy()
                .cmp(&b.found.is_legacy())
                .then(b.found.stamp.mtime.total_cmp(&a.found.stamp.mtime))
        });
    }
}

// ------------------------------------------------------------------ jobs

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct JobStats {
    pub batches: u64,
    pub bytes: u64,
    pub commands: u64,
    pub chunks: u64,
}

impl JobStats {
    /// `seen` holds the command seqs of this job: a command is upserted by every batch that
    /// changes it, but counted once.
    fn add(&mut self, batch: &IndexBatch, last: &mut (u32, u64), seen: &mut HashSet<u64>) {
        self.batches += 1;
        self.commands += batch.commands.iter().filter(|c| seen.insert(c.seq)).count() as u64;
        self.chunks += batch.chunks.len() as u64;
        if let Some(cp) = &batch.checkpoint {
            self.bytes += if cp.part == last.0 {
                cp.byte_offset.saturating_sub(last.1)
            } else {
                cp.byte_offset
            };
            *last = (cp.part, cp.byte_offset);
        }
    }
}

pub enum JobStatus {
    /// The cast is complete and fully indexed.
    Finished,
    /// Read to the end, but the cast has no `x` yet: live, or crashed (the caller decides).
    Incomplete(Box<dyn SessionPipeline>),
    Failed(String),
    /// Shutdown: the job stopped after its last committed batch.
    Stopped,
}

impl JobStatus {
    fn label(&self) -> &'static str {
        match self {
            Self::Finished => "finished",
            Self::Incomplete(_) => "incomplete",
            Self::Failed(_) => "failed",
            Self::Stopped => "stopped",
        }
    }
}

pub struct JobOutcome {
    pub found: Found,
    pub uuid: Option<String>,
    pub prior_state: Option<SessionState>,
    pub stats: JobStats,
    pub status: JobStatus,
}

impl JobOutcome {
    fn failed(job: &Job, e: String) -> Self {
        Self {
            found: job.found.clone(),
            uuid: job.uuid.clone(),
            prior_state: job.prior_state,
            stats: JobStats::default(),
            status: JobStatus::Failed(e),
        }
    }
}

/// The state of an incomplete cast whose recorder is gone: `degraded` if the recorder (or,
/// after a restart, the index) last said so, else `crashed`.
pub fn incomplete_state(degraded: bool) -> SessionState {
    if degraded {
        SessionState::Degraded
    } else {
        SessionState::Crashed
    }
}

impl std::fmt::Debug for JobOutcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JobOutcome")
            .field("key", &self.found.key)
            .field("uuid", &self.uuid)
            .field("stats", &self.stats)
            .field("status", &self.status.label())
            .finish()
    }
}

enum Msg {
    Batch { key: PathBuf, batch: Box<IndexBatch> },
    Legacy(LegacyImportRow),
    Done(JobOutcome),
}

/// Resolves a legacy job's uuid and checkpoint (hashing its first MiB, outside the writer
/// lock but for the checkpoint lookup).
fn resolve_legacy(job: &mut Job, writer: &SharedWriter) -> Result<(), String> {
    if !job.found.is_legacy() || job.uuid.is_some() {
        return Ok(());
    }
    let uuid = legacy_session_uuid(&job.found).ok_or_else(|| "cannot read the cast".to_owned())?;
    job.checkpoint = lock(writer).checkpoint(&uuid).map_err(one_line)?;
    job.uuid = Some(uuid);
    Ok(())
}

/// Runs one job to the end of the available data, sending its batches in order.
fn run_job(
    mut job: Job,
    writer: &SharedWriter,
    sources: &Sources,
    budget: u64,
    stop: &AtomicBool,
    tx: &Sender<Msg>,
) -> JobOutcome {
    let resolved = resolve_legacy(&mut job, writer);
    let Job {
        found,
        mut uuid,
        checkpoint,
        prior_state,
    } = job;
    let key = found.key.clone();
    let mut stats = JobStats::default();
    let mut seqs = HashSet::new();
    let mut last = checkpoint.as_ref().map(|c| (c.part, c.byte_offset)).unwrap_or((1, 0));
    let legacy = found.is_legacy();
    let send = |b: IndexBatch| {
        tx.send(Msg::Batch {
            key: key.clone(),
            batch: Box::new(b),
        })
        .is_ok()
    };
    let fail_early = |e: String, found: Found, uuid| {
        if legacy {
            let _ = tx.send(Msg::Legacy(failed_legacy_row(&found, &e)));
        }
        JobOutcome {
            found,
            uuid,
            prior_state,
            stats: JobStats::default(),
            status: JobStatus::Failed(e),
        }
    };
    if let Err(e) = resolved {
        return fail_early(e, found, uuid);
    }
    let factory = match sources.factory(&found.kind) {
        Ok(f) => f,
        Err(e) => return fail_early(e, found, uuid),
    };
    let mut p = match factory.open(&found.path, checkpoint) {
        Ok(p) => p,
        Err(e) => return fail_early(one_line(e), found, uuid),
    };
    if uuid.is_none() {
        uuid = Some(p.uuid().to_owned());
    }
    let mut finished = false;
    let outcome = |stats, status| JobOutcome {
        found: found.clone(),
        uuid: uuid.clone(),
        prior_state,
        stats,
        status,
    };
    loop {
        if stop.load(Ordering::Relaxed) {
            return outcome(stats, JobStatus::Stopped);
        }
        match p.poll(PollBudget {
            max_bytes: budget,
            live: false,
        }) {
            Ok(Some(b)) => {
                stats.add(&b, &mut last, &mut seqs);
                finished |= b.finished;
                if !send(b) {
                    return outcome(stats, JobStatus::Stopped);
                }
            }
            Ok(None) => break,
            Err(e) => {
                let e = one_line(e);
                if legacy && stats.batches == 0 {
                    let _ = tx.send(Msg::Legacy(failed_legacy_row(&found, &e)));
                } else if let Ok(b) = p.finish(SessionState::Error) {
                    send(b);
                }
                return outcome(stats, JobStatus::Failed(e));
            }
        }
    }
    // Legacy casts are complete once read to the end; cinelog casts need their `x`.
    if !finished && (legacy || p.is_finished()) {
        match p.finish(SessionState::Ended) {
            Ok(b) => {
                stats.add(&b, &mut last, &mut seqs);
                finished = true;
                send(b);
            }
            Err(e) => return outcome(stats, JobStatus::Failed(one_line(e))),
        }
    }
    let status = if finished {
        JobStatus::Finished
    } else {
        JobStatus::Incomplete(p)
    };
    outcome(stats, status)
}

/// Number of commits (the writer itself records `index_info.writer_heartbeat`).
#[derive(Debug, Default)]
pub struct CommitClock {
    commits: AtomicU64,
}

impl CommitClock {
    pub fn record(&self) {
        self.commits.fetch_add(1, Ordering::Relaxed);
    }

    pub fn commits(&self) -> u64 {
        self.commits.load(Ordering::Relaxed)
    }
}

/// Applies `batch` through the shared writer and counts the commit.
pub fn apply(w: &SharedWriter, clock: &CommitClock, batch: &IndexBatch) -> Result<(), IndexError> {
    lock(w).apply(batch)?;
    clock.record();
    Ok(())
}

/// Backfill threads plus the writer thread (§6.4: render threads feed chunks over a channel
/// to the single writer). Batches of one job are applied in order; a job's outcome is
/// delivered only after all its batches were applied.
pub struct Pool {
    jobs: Option<Sender<Job>>,
    done: Receiver<JobOutcome>,
    threads: Vec<JoinHandle<()>>,
    stop: Arc<AtomicBool>,
}

impl Pool {
    pub fn start(
        workers: usize,
        writer: SharedWriter,
        clock: Arc<CommitClock>,
        sources: Arc<Sources>,
        budget: u64,
        stop: Arc<AtomicBool>,
    ) -> Self {
        let workers = workers.max(1);
        let (jobs_tx, jobs_rx) = crossbeam_channel::bounded::<Job>(workers);
        let (msg_tx, msg_rx) = crossbeam_channel::bounded::<Msg>(workers * 2);
        let (done_tx, done_rx) = crossbeam_channel::unbounded();
        let mut threads = Vec::with_capacity(workers + 1);
        for i in 0..workers {
            let (rx, tx, sources, stop) = (jobs_rx.clone(), msg_tx.clone(), sources.clone(), stop.clone());
            let writer = writer.clone();
            let spawned = thread::Builder::new()
                .name(format!("cinelog-render-{i}"))
                .spawn(move || {
                    for job in rx.iter() {
                        let failed = JobOutcome::failed(&job, "the indexer panicked on this cast".into());
                        let run = std::panic::AssertUnwindSafe(|| run_job(job, &writer, &sources, budget, &stop, &tx));
                        let outcome = std::panic::catch_unwind(run).unwrap_or(failed);
                        if tx.send(Msg::Done(outcome)).is_err() {
                            break;
                        }
                    }
                })
                .expect("spawn render thread");
            threads.push(spawned);
        }
        drop(msg_tx);
        let spawned = thread::Builder::new()
            .name("cinelog-writer".into())
            .spawn(move || writer_loop(&writer, &clock, &msg_rx, &done_tx))
            .expect("spawn writer thread");
        threads.push(spawned);
        Self {
            jobs: Some(jobs_tx),
            done: done_rx,
            threads,
            stop,
        }
    }

    /// Queues a job unless every worker is busy and the hand-over queue is full; the job
    /// comes back then.
    pub fn try_submit(&self, job: Job) -> Result<(), Box<Job>> {
        let Some(tx) = &self.jobs else {
            return Err(Box::new(job));
        };
        tx.try_send(job).map_err(|e| match e {
            TrySendError::Full(j) | TrySendError::Disconnected(j) => Box::new(j),
        })
    }

    pub fn submit(&self, job: Job) {
        if let Some(tx) = &self.jobs {
            let _ = tx.send(job);
        }
    }

    pub fn done(&self) -> &Receiver<JobOutcome> {
        &self.done
    }

    /// Stops the workers after their current poll and waits for the writer to apply what
    /// they produced. Outcomes still in flight are returned.
    pub fn shutdown(mut self, stop_now: bool) -> Vec<JobOutcome> {
        if stop_now {
            self.stop.store(true, Ordering::Relaxed);
        }
        self.jobs = None;
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
        self.done.try_iter().collect()
    }
}

impl Drop for Pool {
    fn drop(&mut self) {
        self.jobs = None;
        self.stop.store(true, Ordering::Relaxed);
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
    }
}

fn writer_loop(w: &SharedWriter, clock: &CommitClock, rx: &Receiver<Msg>, done: &Sender<JobOutcome>) {
    // Once a batch of a job fails, the rest of that job is skipped: the session resumes
    // from its last committed checkpoint on the next discovery.
    let mut failed: HashMap<PathBuf, String> = HashMap::new();
    for msg in rx.iter() {
        match msg {
            Msg::Batch { key, batch } => {
                if failed.contains_key(&key) {
                    continue;
                }
                if let Err(e) = apply(w, clock, &batch) {
                    let e = one_line(format!("{}: {e}", key.display()));
                    note_error(w, &e);
                    failed.insert(key, e);
                }
            }
            Msg::Legacy(row) => {
                if let Err(e) = lock(w).record_legacy(&row) {
                    note_error(w, &one_line(format!("{}: {e}", row.path.display())));
                }
            }
            Msg::Done(mut o) => {
                if let Some(e) = failed.remove(&o.found.key) {
                    o.status = JobStatus::Failed(e);
                }
                let _ = done.send(o);
            }
        }
    }
}

// ------------------------------------------------------------------ open sessions

/// Whether the recorder of an incomplete cast is gone: its registry entry is dead, or it
/// has none and the cast has not changed for `grace`.
pub fn recorder_gone(entry: Option<&LiveEntry>, watched: &Path, grace: Duration) -> bool {
    match entry {
        Some(e) => !e.alive,
        None => {
            let idle = std::fs::metadata(watched)
                .and_then(|m| m.modified())
                .ok()
                .and_then(|m| m.elapsed().ok());
            // A vanished file has no recorder either.
            idle.is_none_or(|d| d >= grace)
        }
    }
}

/// The file a live session grows: the registry's current part, else its last part on disk.
pub fn watched_part(key: &Path, entry: Option<&LiveEntry>) -> PathBuf {
    match entry {
        Some(e) => e.cast.clone(),
        None => cinelog_cast::find_parts(key).pop().unwrap_or_else(|| key.to_path_buf()),
    }
}

// ------------------------------------------------------------------ `cinelog index`

/// Settings of the indexer of one scope.
#[derive(Debug, Clone)]
pub struct EngineConfig {
    pub logs_root: PathBuf,
    pub registry_dirs: Vec<PathBuf>,
    pub scope: Scope,
    pub workers: usize,
    pub poll_bytes: u64,
    pub crash_grace: Duration,
}

/// The one-shot engine behind `cinelog index --once|--rebuild` (holds the writer lock).
pub struct Engine {
    writer: SharedWriter,
    meta: Option<Box<dyn MetaStore>>,
    sources: Arc<Sources>,
    probe: Box<dyn ProcessProbe>,
    cfg: EngineConfig,
}

impl Engine {
    pub fn new(
        writer: Box<dyn IndexWriter>,
        meta: Option<Box<dyn MetaStore>>,
        sources: Sources,
        probe: Box<dyn ProcessProbe>,
        cfg: EngineConfig,
    ) -> Self {
        Self {
            writer: shared(writer),
            meta,
            sources: Arc::new(sources),
            probe,
            cfg,
        }
    }

    fn summarize(&self, summary: &mut OnceSummary, o: &JobOutcome) {
        match &o.status {
            JobStatus::Failed(e) => {
                summary.failed += 1;
                summary.errors.push(format!("{}: {e}", o.found.path.display()));
            }
            JobStatus::Stopped => {}
            JobStatus::Finished | JobStatus::Incomplete(_) => summary.indexed += 1,
        }
        summary.bytes += o.stats.bytes;
        summary.commands += o.stats.commands;
        summary.chunks += o.stats.chunks;
    }
}

impl IndexEngine for Engine {
    fn index_once(&mut self, opts: &OnceOptions) -> Result<OnceSummary, IndexError> {
        let started = Instant::now();
        let mut summary = OnceSummary::default();
        if opts.rebuild {
            let mut w = lock(&self.writer);
            w.set_info(info_keys::REBUILD_STARTED, Some(&now_ts().to_string()))?;
            w.clear()?;
        }
        // Forgotten rows must not come back, also not through a rebuild.
        if let Some(meta) = &self.meta {
            lock(&self.writer).apply_forgotten(&meta.forgotten()?)?;
        }
        let mut walker = Walker::new();
        let walk = if opts.paths.is_empty() {
            walker.walk(&self.cfg.logs_root)
        } else {
            walker.collect(&opts.paths)
        };
        let live: HashMap<String, LiveEntry> =
            scan_registry(&self.cfg.registry_dirs, self.cfg.scope, self.probe.as_ref())
                .into_iter()
                .map(|e| (e.uuid.clone(), e))
                .collect();
        let known: HashMap<String, KnownCast> = lock(&self.writer)
            .known_casts()?
            .into_iter()
            .map(|k| (k.uuid.clone(), k))
            .collect();
        let mut jobs = Vec::new();
        {
            let w = lock(&self.writer);
            for found in walk.casts {
                summary.casts_seen += 1;
                match Job::plan(found, &known, w.as_ref(), opts.rebuild)? {
                    Some(job) => jobs.push(job),
                    None => summary.unchanged += 1,
                }
            }
        }
        Job::sort(&mut jobs);
        let clock = Arc::new(CommitClock::default());
        let stop = Arc::new(AtomicBool::new(false));
        let pool = Pool::start(
            self.cfg.workers,
            self.writer.clone(),
            clock.clone(),
            self.sources.clone(),
            self.cfg.poll_bytes,
            stop,
        );
        let total = jobs.len();
        // Submitting blocks while the workers are busy, so feed them from a thread and
        // collect outcomes here.
        let mut outcomes = Vec::with_capacity(total);
        thread::scope(|s| {
            s.spawn(|| {
                for job in jobs {
                    pool.submit(job);
                }
            });
            while outcomes.len() < total {
                match pool.done().recv() {
                    Ok(o) => outcomes.push(o),
                    Err(_) => break,
                }
            }
        });
        outcomes.extend(pool.shutdown(false));
        for o in &mut outcomes {
            self.summarize(&mut summary, o);
            let status = std::mem::replace(&mut o.status, JobStatus::Stopped);
            let JobStatus::Incomplete(mut p) = status else { continue };
            let entry = o.uuid.as_deref().and_then(|u| live.get(u));
            let watched = watched_part(&o.found.key, entry);
            if !recorder_gone(entry, &watched, self.cfg.crash_grace) {
                continue; // still recording: the server (or the next run) continues it
            }
            let degraded = match entry {
                Some(e) => e.state == RecState::Degraded,
                None => o.prior_state == Some(SessionState::Degraded),
            };
            match p.finish(incomplete_state(degraded)) {
                Ok(b) => apply(&self.writer, &clock, &b)?,
                Err(e) => {
                    summary.failed += 1;
                    summary
                        .errors
                        .push(format!("{}: {}", o.found.path.display(), one_line(e)));
                }
            }
        }
        let mut w = lock(&self.writer);
        if opts.rebuild {
            w.set_info(info_keys::REBUILD_STARTED, None)?;
        }
        if let Some(e) = summary.errors.last() {
            w.set_info(info_keys::LAST_ERROR, Some(e))?;
        }
        summary.elapsed_secs = started.elapsed().as_secs_f64();
        Ok(summary)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::serve::testkit::*;
    use cinelog_index::fake::MemIndex;

    fn engine(mem: &MemIndex, root: &Path, probe: FakeProbe, reg: Vec<PathBuf>) -> Engine {
        Engine::new(
            Box::new(mem.clone()),
            Some(Box::new(mem.clone())),
            test_sources(),
            Box::new(probe),
            EngineConfig {
                logs_root: root.to_path_buf(),
                registry_dirs: reg,
                scope: Scope::Personal,
                workers: 2,
                poll_bytes: 64,
                crash_grace: Duration::ZERO,
            },
        )
    }

    #[test]
    fn index_once_indexes_new_and_skips_unchanged() {
        let d = tempfile::tempdir().unwrap();
        let month = d.path().join("2026-11");
        let events = [
            (0.1, "m", cmd_json(1, 7, 1, "echo hi")),
            (0.2, "o", "hi\r\n".to_owned()),
            (0.3, "m", done_json(1, 7, 1, 0)),
        ];
        let ev: Vec<(f64, &str, &str)> = events.iter().map(|(t, c, s)| (*t, *c, s.as_str())).collect();
        write_cast(&month, "h_a_00000001.cast", UUID_A, 1, &ev, Some(0));
        write_legacy(d.path(), "old.cast", &[(1.0, "o", "$ ls\r\n")]);
        write_legacy(d.path(), "FAILME.cast", &[(1.0, "o", "x\r\n")]);
        let mem = MemIndex::new();
        let mut e = engine(&mem, d.path(), FakeProbe::alive(&[]), vec![]);
        let s = e.index_once(&OnceOptions::default()).unwrap();
        assert_eq!((s.casts_seen, s.indexed, s.failed, s.unchanged), (3, 2, 1, 0), "{s:?}");
        assert_eq!(s.commands, 1);
        assert!(s.errors[0].contains("FAILME"), "{:?}", s.errors);
        let cmds = mem.commands(UUID_A);
        assert_eq!(cmds.len(), 1);
        assert_eq!(cmds[0].exit, Some(0));
        let sess = mem.sessions();
        let a = sess.iter().find(|s| s.uuid == UUID_A).unwrap();
        assert_eq!(a.state, SessionState::Ended);
        let failed = mem.legacy_entry(&d.path().join("FAILME.cast")).unwrap().unwrap();
        assert_eq!(failed.status, LegacyStatus::Failed);
        assert_eq!(failed.sha256.len(), 64);
        assert!(mem.info(info_keys::LAST_ERROR).unwrap().contains("FAILME"));

        // A failed legacy import is retried on every run.
        let s = e.index_once(&OnceOptions::default()).unwrap();
        assert_eq!((s.indexed, s.unchanged, s.failed), (0, 2, 1), "{s:?}");

        let s = e
            .index_once(&OnceOptions {
                rebuild: true,
                paths: vec![],
            })
            .unwrap();
        assert_eq!(s.indexed + s.failed, 3, "{s:?}");
        assert_eq!(mem.commands(UUID_A).len(), 1);
        assert_eq!(mem.info(info_keys::REBUILD_STARTED), None);
    }

    #[test]
    fn incomplete_casts_crash_unless_their_recorder_lives() {
        let d = tempfile::tempdir().unwrap();
        let ev = [(0.1, "o", "$ \r\n")];
        write_cast(d.path(), "h_a.cast", UUID_A, 1, &ev, None);
        write_cast(d.path(), "h_b.cast", UUID_B, 1, &ev, None);
        let reg = d.path().join("run");
        write_registry(&reg, 1, 42, UUID_B, &d.path().join("h_b.cast"));
        let mem = MemIndex::new();
        let mut e = engine(&mem, d.path(), FakeProbe::alive(&[(42, 7)]), vec![reg]);
        let s = e.index_once(&OnceOptions::default()).unwrap();
        assert_eq!(s.indexed, 2, "{s:?}");
        let state = |u: &str| mem.sessions().into_iter().find(|s| s.uuid == u).unwrap().state;
        assert_eq!(state(UUID_A), SessionState::Crashed);
        assert_eq!(state(UUID_B), SessionState::Live);
        // Live sessions stay on the work list; crashed ones are settled.
        let s = e.index_once(&OnceOptions::default()).unwrap();
        assert_eq!((s.indexed, s.unchanged), (1, 1), "{s:?}");
    }

    #[test]
    fn degraded_sessions_stay_degraded_and_settle() {
        let d = tempfile::tempdir().unwrap();
        let mut cast = CastFile::create(d.path(), "h_a.cast", UUID_A, 1);
        cast.cmd(0.5, 1, 9, 1, "cat big");
        cast.flush();
        let reg = d.path().join("run");
        let entry = write_registry_state(&reg, 1, 42, UUID_A, cast.path(), RecState::Degraded);
        let mem = MemIndex::new();
        // The degraded recorder is dead: closed as degraded, not crashed.
        let mut e = engine(&mem, d.path(), FakeProbe::alive(&[]), vec![reg]);
        let state = |mem: &MemIndex| mem.sessions()[0].state;
        e.index_once(&OnceOptions::default()).unwrap();
        assert_eq!(state(&mem), SessionState::Degraded);
        // Without an entry, the index's view decides when the cast changed...
        std::fs::remove_file(entry).unwrap();
        cast.output(1.0, "tail\r\n");
        cast.flush();
        let s = e.index_once(&OnceOptions::default()).unwrap();
        assert_eq!(s.indexed, 1, "{s:?}");
        assert_eq!(state(&mem), SessionState::Degraded);
        assert!(mem.chunks(UUID_A).iter().any(|c| c.text.contains("tail")));
        // ...and an unchanged degraded cast is not read again.
        let s = e.index_once(&OnceOptions::default()).unwrap();
        assert_eq!((s.indexed, s.unchanged), (0, 1), "{s:?}");
    }

    #[test]
    fn legacy_jobs_hash_in_the_worker_and_resume_from_their_checkpoint() {
        let d = tempfile::tempdir().unwrap();
        let events: Vec<(f64, &str, String)> = (0..20).map(|i| (f64::from(i), "o", format!("line {i}\r\n"))).collect();
        let ev: Vec<(f64, &str, &str)> = events.iter().map(|(t, c, s)| (*t, *c, s.as_str())).collect();
        let path = write_legacy(d.path(), "old.cast", &ev);
        let mut mem = MemIndex::new();
        // A first, interrupted import committed one small batch.
        let mut p = TestFactory { legacy: true }.open(&path, None).unwrap();
        let first = p
            .poll(PollBudget {
                max_bytes: 64,
                live: false,
            })
            .unwrap()
            .unwrap();
        let resumed_at = first.checkpoint.as_ref().unwrap().byte_offset;
        mem.apply(&first).unwrap();

        let found = Walker::new().walk(d.path()).casts.remove(0);
        let size = found.stamp.size;
        let job = Job::plan(found.clone(), &HashMap::new(), &mem, false).unwrap().unwrap();
        assert_eq!(
            (job.uuid.as_deref(), job.checkpoint.is_none()),
            (None, true),
            "no hashing in plan"
        );
        let w = shared(Box::new(mem.clone()));
        let mut resolved = Job::plan(found, &HashMap::new(), &mem, false).unwrap().unwrap();
        resolve_legacy(&mut resolved, &w).unwrap();
        assert_eq!(resolved.uuid.as_deref(), Some(p.uuid()));
        assert_eq!(resolved.checkpoint.map(|c| c.byte_offset), Some(resumed_at));
        let stop = Arc::new(AtomicBool::new(false));
        let pool = Pool::start(
            1,
            w,
            Arc::new(CommitClock::default()),
            Arc::new(test_sources()),
            64,
            stop,
        );
        pool.submit(job);
        let o = pool.done().recv().unwrap();
        pool.shutdown(false);
        assert!(matches!(o.status, JobStatus::Finished), "{o:?}");
        assert_eq!(o.uuid.as_deref(), Some(p.uuid()));
        assert_eq!(o.stats.bytes, size - resumed_at, "read from the checkpoint on");
        let text: String = mem.chunks(p.uuid()).iter().map(|c| c.text.clone()).collect();
        assert!((0..20).all(|i| text.contains(&format!("line {i}"))));
    }

    #[test]
    fn forgotten_sessions_stay_forgotten_after_a_rebuild() {
        use cinelog_index::model::ForgetTarget;
        let d = tempfile::tempdir().unwrap();
        write_cast(d.path(), "h_a.cast", UUID_A, 1, &[(0.1, "o", "x\r\n")], Some(0));
        let mem = MemIndex::new();
        let mut e = engine(&mem, d.path(), FakeProbe::alive(&[]), vec![]);
        e.index_once(&OnceOptions::default()).unwrap();
        assert_eq!(mem.sessions().len(), 1);
        let mut m = mem.clone();
        m.forget(&ForgetTarget::Session { uuid: UUID_A.into() }, "me").unwrap();
        e.index_once(&OnceOptions {
            rebuild: true,
            paths: vec![],
        })
        .unwrap();
        assert!(mem.sessions().is_empty());
    }

    #[test]
    fn plan_rules() {
        let d = tempfile::tempdir().unwrap();
        let p = write_cast(d.path(), "h_a.cast", UUID_A, 1, &[], Some(0));
        let mut w = Walker::new();
        let found = w.walk(d.path()).casts.remove(0);
        let stamp = FileStamp::of(&p).unwrap();
        let known = |state, file: Option<FileStamp>| KnownCast {
            uuid: UUID_A.into(),
            cast_path: p.clone(),
            legacy: false,
            state,
            parts: 1,
            compressed: false,
            ended_at: Some(1.0),
            indexed_part: Some(1),
            indexed_offset: Some(stamp.size),
            indexed_file: file,
        };
        assert!(cinelog_needs_work(&found, None));
        assert!(!cinelog_needs_work(
            &found,
            Some(&known(SessionState::Ended, Some(stamp)))
        ));
        let touched = FileStamp {
            mtime: stamp.mtime + 5.0,
            ..stamp
        };
        assert!(!cinelog_needs_work(
            &found,
            Some(&known(SessionState::Ended, Some(touched)))
        ));
        let grown = FileStamp {
            size: stamp.size - 1,
            ..stamp
        };
        assert!(cinelog_needs_work(
            &found,
            Some(&known(SessionState::Ended, Some(grown)))
        ));
        assert!(cinelog_needs_work(
            &found,
            Some(&known(SessionState::Live, Some(stamp)))
        ));
        assert!(!cinelog_needs_work(
            &found,
            Some(&known(SessionState::Degraded, Some(stamp)))
        ));
        assert!(cinelog_needs_work(
            &found,
            Some(&known(SessionState::Degraded, Some(grown)))
        ));
        let mut k = known(SessionState::Ended, Some(stamp));
        k.parts = 0;
        assert!(!cinelog_needs_work(&found, Some(&k)));

        let row = failed_legacy_row(&found, "boom\nline");
        assert_eq!(row.error.as_deref(), Some("boom line"));
        assert!(legacy_needs_work(&found, Some(&row)), "a failed import is retried");
        let imported = LegacyImportRow {
            status: LegacyStatus::Imported,
            ..row.clone()
        };
        assert!(!legacy_needs_work(&found, Some(&imported)));
        let other = LegacyImportRow {
            file: FileStamp::default(),
            ..imported
        };
        assert!(legacy_needs_work(&found, Some(&other)));
        assert!(legacy_needs_work(&found, None));
    }

    #[test]
    fn a_failed_archived_legacy_cast_finds_its_plain_named_log() {
        let d = tempfile::tempdir().unwrap();
        let zst = d.path().join("old.cast.zst");
        std::fs::write(&zst, b"not zstd").unwrap();
        std::fs::write(d.path().join("old.cast.commands.log"), b"[1, \"m\", \"ls\"]\n").unwrap();
        let found = Found {
            key: zst.clone(),
            path: zst.clone(),
            kind: FoundKind::Legacy,
            parts: vec![zst.clone()],
            stamp: FileStamp::of(&zst).unwrap(),
            depth: 0,
        };
        let row = failed_legacy_row(&found, "bad");
        assert_eq!(row.commands_log, Some(d.path().join("old.cast.commands.log")));
    }

    #[test]
    fn crash_rule() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("x.cast");
        std::fs::write(&p, b"x").unwrap();
        assert!(!recorder_gone(None, &p, Duration::from_secs(60)), "fresh file");
        assert!(recorder_gone(None, &p, Duration::ZERO));
        assert!(recorder_gone(
            None,
            &d.path().join("gone.cast"),
            Duration::from_secs(60)
        ));
        let mut e = LiveEntry {
            uuid: UUID_A.into(),
            key: p.clone(),
            cast: p.clone(),
            alive: true,
            state: cinelog_core::RecState::Live,
        };
        assert!(!recorder_gone(Some(&e), &p, Duration::ZERO));
        e.alive = false;
        assert!(recorder_gone(Some(&e), &p, Duration::from_secs(60)));
    }
}
