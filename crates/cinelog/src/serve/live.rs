//! Live incremental indexing (§6.4 "Live"): sessions whose cast is still open. Their file is
//! stat'ed on every tick and read as soon as it grows (live polls, which also render the
//! provisional chunk of the running command); what was read is merged into one pending
//! batch that is committed every [`COMMIT_INTERVAL`], so a command is searchable within
//! seconds.
//!
//! [`COMMIT_INTERVAL`]: cinelog_index::model::COMMIT_INTERVAL

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use cinelog_core::RecState;
use cinelog_index::model::{IndexBatch, PollBudget, SessionState};
use cinelog_index::{IndexError, SessionPipeline};

use super::discover::LiveEntry;
use super::engine::{CommitClock, SharedWriter, apply, incomplete_state};

/// Bytes one live poll may read; a tick reads at most [`LIVE_POLLS_PER_TICK`] of them per
/// session, so one busy session cannot starve the others.
pub const LIVE_POLL_BYTES: u64 = 4 << 20;
pub const LIVE_POLLS_PER_TICK: usize = 2;
/// Pending output text that forces a commit before the interval is up (catch-up reads).
pub const PENDING_FLUSH_BYTES: usize = 8 << 20;

/// Folds `next` into `into` as if both had been applied in order.
pub fn merge(into: &mut IndexBatch, next: IndexBatch) {
    if next.reset {
        // A reset drops every derived row first, so nothing pending before it matters.
        *into = next;
        return;
    }
    into.session = next.session;
    for p in next.parts {
        match into.parts.iter_mut().find(|q| q.part == p.part) {
            Some(q) => *q = p,
            None => into.parts.push(p),
        }
    }
    for c in next.commands {
        match into.commands.iter_mut().find(|d| d.seq == c.seq) {
            Some(d) => *d = c,
            None => into.commands.push(c),
        }
    }
    into.chunks.extend(next.chunks);
    if next.provisional.is_some() {
        into.provisional = next.provisional;
    }
    into.markers.extend(next.markers);
    for s in next.segments {
        match into.segments.iter_mut().find(|t| t.seq == s.seq) {
            Some(t) => *t = s,
            None => into.segments.push(s),
        }
    }
    if next.checkpoint.is_some() {
        into.checkpoint = next.checkpoint;
    }
    if next.legacy.is_some() {
        into.legacy = next.legacy;
    }
    into.finished |= next.finished;
}

/// A session with an open cast and its pipeline.
pub struct OpenSession {
    pub uuid: String,
    pipeline: Box<dyn SessionPipeline>,
    /// Last registry view; `None` when the recorder has no entry (see `recorder_gone`).
    pub entry: Option<LiveEntry>,
    /// The recorder was last seen degraded (kept while it has no registry entry).
    degraded: bool,
    /// The part file being appended to.
    pub watched: PathBuf,
    /// Size of `watched` when the pipeline last caught up with it.
    caught_up_at: Option<(PathBuf, u64)>,
    pending: Option<IndexBatch>,
    pending_bytes: usize,
    /// Something was read since the last commit.
    dirty: bool,
    /// A batch with `finished` was read (the `x` event).
    finished: bool,
    last_commit: Instant,
    poll_bytes: u64,
    flush_bytes: usize,
}

impl OpenSession {
    /// `index_degraded`: the index shows the session as degraded; it counts while the
    /// recorder has no registry entry (a restart lost the registry view).
    pub fn new(
        uuid: String,
        pipeline: Box<dyn SessionPipeline>,
        entry: Option<LiveEntry>,
        watched: PathBuf,
        index_degraded: bool,
    ) -> Self {
        let degraded = entry.as_ref().map_or(index_degraded, |e| e.state == RecState::Degraded);
        Self {
            uuid,
            pipeline,
            entry,
            degraded,
            watched,
            caught_up_at: None,
            pending: None,
            pending_bytes: 0,
            dirty: false,
            finished: false,
            last_commit: Instant::now(),
            poll_bytes: LIVE_POLL_BYTES,
            flush_bytes: PENDING_FLUSH_BYTES,
        }
    }

    #[cfg(test)]
    pub fn with_limits(mut self, poll_bytes: u64, flush_bytes: usize) -> Self {
        self.poll_bytes = poll_bytes;
        self.flush_bytes = flush_bytes;
        self
    }

    /// Takes the latest registry view; a missing entry keeps the last known degraded state.
    pub fn set_entry(&mut self, entry: Option<LiveEntry>) {
        if let Some(e) = &entry {
            self.degraded = e.state == RecState::Degraded;
        }
        self.entry = entry;
    }

    pub fn is_finished(&self) -> bool {
        self.finished
    }

    fn take(&mut self, b: IndexBatch) {
        self.finished |= b.finished;
        self.pending_bytes += b.chunks.iter().map(|c| c.text.len()).sum::<usize>();
        match &mut self.pending {
            Some(p) => merge(p, b),
            None => self.pending = Some(b),
        }
        self.dirty = true;
    }

    fn poll(&mut self, live: bool) -> Result<bool, IndexError> {
        match self.pipeline.poll(PollBudget {
            max_bytes: self.poll_bytes,
            live,
        })? {
            Some(b) => {
                self.take(b);
                Ok(true)
            }
            None => Ok(false),
        }
    }

    /// Reads what was appended since the last call. Returns whether anything was read.
    pub fn read_growth(&mut self) -> Result<bool, IndexError> {
        if self.finished {
            return Ok(false);
        }
        let size = size_of(&self.watched);
        let now = (self.watched.clone(), size);
        if self.caught_up_at.as_ref() == Some(&now) {
            return Ok(false);
        }
        let mut progress = false;
        for _ in 0..LIVE_POLLS_PER_TICK {
            if !self.poll(true)? {
                self.caught_up_at = Some(now);
                return Ok(progress);
            }
            progress = true;
            if self.finished {
                break;
            }
        }
        Ok(progress)
    }

    /// Commits the pending batch when the interval is up (or it grew large, or the cast
    /// ended).
    pub fn commit_due(
        &mut self,
        w: &SharedWriter,
        clock: &CommitClock,
        interval: Duration,
    ) -> Result<bool, IndexError> {
        let due = self.finished
            || self.pending_bytes >= self.flush_bytes
            || (self.dirty && self.last_commit.elapsed() >= interval);
        if !due {
            return Ok(false);
        }
        self.commit(w, clock)
    }

    fn commit(&mut self, w: &SharedWriter, clock: &CommitClock) -> Result<bool, IndexError> {
        self.dirty = false;
        self.last_commit = Instant::now();
        let Some(mut b) = self.pending.take() else {
            return Ok(false);
        };
        self.pending_bytes = 0;
        if self.degraded && b.session.state == SessionState::Live {
            b.session.state = SessionState::Degraded;
        }
        apply(w, clock, &b)?;
        Ok(true)
    }

    /// Commits whatever is pending without finishing the session (shutdown).
    pub fn flush(&mut self, w: &SharedWriter, clock: &CommitClock) -> Result<(), IndexError> {
        self.commit(w, clock).map(drop)
    }

    /// The recorder is gone: reads the rest (committing whenever enough is pending, as
    /// the live loop does) and closes the session: `ended` if the cast has its `x`, else
    /// `degraded` or `crashed` ([`incomplete_state`]).
    pub fn finalize(mut self, w: &SharedWriter, clock: &CommitClock) -> Result<SessionState, IndexError> {
        while !self.finished && self.poll(false)? {
            if self.pending_bytes >= self.flush_bytes {
                self.commit(w, clock)?;
            }
        }
        let state = if self.finished {
            SessionState::Ended
        } else {
            let state = if self.pipeline.is_finished() {
                SessionState::Ended
            } else {
                incomplete_state(self.degraded)
            };
            let b = self.pipeline.finish(state)?;
            self.take(b);
            state
        };
        self.commit(w, clock)?;
        Ok(state)
    }
}

fn size_of(p: &Path) -> u64 {
    std::fs::metadata(p).map(|m| m.len()).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::serve::engine::shared;
    use crate::serve::testkit::*;
    use cinelog_index::PipelineFactory;
    use cinelog_index::fake::MemIndex;
    use cinelog_index::model::{Chunk, ChunkKind, Command, Segment};

    fn cmd(seq: u64, exit: Option<i32>) -> Command {
        let mut c = Command::from_cmd(
            &serde_json::from_str(&format!(r#"{{"seq":{seq},"sh":1,"n":{seq},"typed":"c{seq}"}}"#)).unwrap(),
            seq as f64,
            seq as f64,
            None,
            0,
        );
        c.exit = exit;
        c
    }

    fn chunk(t: f64, kind: ChunkKind) -> Chunk {
        Chunk {
            kind,
            cmd_seq: None,
            t,
            ts: t,
            line0: 0,
            text: format!("t{t}"),
        }
    }

    fn seg(seq: u32, t_end: Option<f64>) -> Segment {
        Segment {
            seq,
            part: 1,
            t_start: 0.0,
            t_end,
            byte_offset: 0,
            vt_dump: String::new(),
        }
    }

    #[test]
    fn merge_upserts_and_appends() {
        let base = || IndexBatch::new(session_meta(UUID_A));
        let mut a = base();
        a.commands = vec![cmd(1, None), cmd(2, None)];
        a.chunks = vec![chunk(1.0, ChunkKind::Primary)];
        a.provisional = Some(vec![chunk(1.5, ChunkKind::Provisional)]);
        a.segments = vec![seg(0, None)];
        let mut b = base();
        b.session.state = SessionState::Ended;
        b.commands = vec![cmd(2, Some(0)), cmd(3, None)];
        b.chunks = vec![chunk(2.0, ChunkKind::Primary)];
        b.segments = vec![seg(0, Some(5.0)), seg(1, None)];
        b.finished = true;
        merge(&mut a, b);
        assert_eq!(a.session.state, SessionState::Ended);
        let seqs: Vec<_> = a.commands.iter().map(|c| (c.seq, c.exit)).collect();
        assert_eq!(seqs, [(1, None), (2, Some(0)), (3, None)]);
        assert_eq!(a.chunks.len(), 2);
        assert_eq!(a.provisional.as_ref().unwrap().len(), 1, "kept: next had none");
        assert_eq!(a.segments, [seg(0, Some(5.0)), seg(1, None)]);
        assert!(a.finished);

        let mut c = base();
        c.reset = true;
        c.chunks = vec![chunk(9.0, ChunkKind::Gap)];
        merge(&mut a, c.clone());
        assert_eq!(a, c);
    }

    #[test]
    fn grows_commits_on_interval_and_finishes() {
        let d = tempfile::tempdir().unwrap();
        let mut cast = CastFile::create(d.path(), "h_a.cast", UUID_A, 1);
        cast.flush();
        let factory = TestFactory::default();
        let p = factory.open(cast.path(), None).unwrap();
        let mem = MemIndex::new();
        let w = shared(Box::new(mem.clone()));
        let clock = CommitClock::default();
        let mut s = OpenSession::new(UUID_A.into(), p, None, cast.path().into(), false);
        assert!(s.read_growth().unwrap());
        assert!(!s.read_growth().unwrap(), "caught up");
        assert!(s.commit_due(&w, &clock, Duration::ZERO).unwrap());
        assert_eq!(mem.sessions().len(), 1);

        cast.cmd(0.5, 1, 9, 1, "make");
        cast.output(0.6, "building\r\n");
        cast.flush();
        assert!(s.read_growth().unwrap());
        assert!(
            !s.commit_due(&w, &clock, Duration::from_secs(3600)).unwrap(),
            "interval not up"
        );
        assert!(s.commit_due(&w, &clock, Duration::ZERO).unwrap());
        assert_eq!(mem.commands(UUID_A).len(), 1);
        assert!(mem.chunks(UUID_A).iter().any(|c| c.kind == ChunkKind::Provisional));
        assert_eq!(clock.commits(), 2);

        cast.done(0.7, 1, 9, 1, 2);
        cast.exit(0.8, 0);
        cast.flush();
        assert!(s.read_growth().unwrap());
        assert!(s.is_finished());
        assert!(
            s.commit_due(&w, &clock, Duration::from_secs(3600)).unwrap(),
            "x commits at once"
        );
        assert_eq!(mem.commands(UUID_A)[0].exit, Some(2));
        assert_eq!(mem.sessions()[0].state, SessionState::Ended);
        assert!(!mem.chunks(UUID_A).iter().any(|c| c.kind == ChunkKind::Provisional));
    }

    #[test]
    fn finalize_marks_crashes_and_degraded_state_is_shown() {
        let d = tempfile::tempdir().unwrap();
        let mut cast = CastFile::create(d.path(), "h_a.cast", UUID_A, 1);
        cast.cmd(0.5, 1, 9, 1, "sleep 100");
        cast.flush();
        let factory = TestFactory::default();
        let mem = MemIndex::new();
        let w = shared(Box::new(mem.clone()));
        let clock = CommitClock::default();
        let entry = LiveEntry {
            uuid: UUID_A.into(),
            key: cast.path().into(),
            cast: cast.path().into(),
            alive: true,
            state: RecState::Degraded,
        };
        let p = factory.open(cast.path(), None).unwrap();
        let mut s = OpenSession::new(UUID_A.into(), p, Some(entry.clone()), cast.path().into(), false);
        s.read_growth().unwrap();
        s.commit_due(&w, &clock, Duration::ZERO).unwrap();
        assert_eq!(mem.sessions()[0].state, SessionState::Degraded);

        // The degraded recorder exits and removes its entry: still degraded, not crashed.
        cast.output(1.0, "more\r\n");
        cast.flush();
        s.set_entry(None);
        let state = s.finalize(&w, &clock).unwrap();
        assert_eq!(state, SessionState::Degraded);
        assert_eq!(mem.sessions()[0].state, SessionState::Degraded);
        assert_eq!(mem.commands(UUID_A)[0].t_end, Some(1.0), "closed at the last event");
        assert!(mem.chunks(UUID_A).iter().any(|c| c.text.contains("more")));

        // A recorder that recovered (entry back to live) crashes as crashed; with no entry
        // at all, the index's view decides.
        let p = factory.open(cast.path(), None).unwrap();
        let mut s = OpenSession::new(UUID_A.into(), p, None, cast.path().into(), true);
        s.set_entry(Some(LiveEntry {
            state: RecState::Live,
            alive: false,
            ..entry
        }));
        assert_eq!(s.finalize(&w, &clock).unwrap(), SessionState::Crashed);
        let p = factory.open(cast.path(), None).unwrap();
        let s = OpenSession::new(UUID_A.into(), p, None, cast.path().into(), true);
        assert_eq!(s.finalize(&w, &clock).unwrap(), SessionState::Degraded);
    }

    #[test]
    fn finalize_commits_a_large_backlog_in_steps() {
        let d = tempfile::tempdir().unwrap();
        let mut cast = CastFile::create(d.path(), "h_a.cast", UUID_A, 1);
        for i in 0..40 {
            cast.output(0.1 + f64::from(i) * 0.01, &format!("line {i:02}\r\n"));
        }
        cast.flush();
        let factory = TestFactory::default();
        let mem = MemIndex::new();
        let w = shared(Box::new(mem.clone()));
        let clock = CommitClock::default();
        let p = factory.open(cast.path(), None).unwrap();
        let s = OpenSession::new(UUID_A.into(), p, None, cast.path().into(), false).with_limits(128, 40);
        assert_eq!(s.finalize(&w, &clock).unwrap(), SessionState::Crashed);
        assert!(clock.commits() > 3, "{} commits", clock.commits());
        let text: String = mem.chunks(UUID_A).iter().map(|c| c.text.clone()).collect();
        assert!((0..40).all(|i| text.contains(&format!("line {i:02}"))));
    }

    #[test]
    fn finalize_sees_a_late_exit() {
        let d = tempfile::tempdir().unwrap();
        let mut cast = CastFile::create(d.path(), "h_a.cast", UUID_A, 1);
        cast.flush();
        let factory = TestFactory::default();
        let mem = MemIndex::new();
        let w = shared(Box::new(mem.clone()));
        let clock = CommitClock::default();
        let p = factory.open(cast.path(), None).unwrap();
        let s = OpenSession::new(UUID_A.into(), p, None, cast.path().into(), false);
        cast.exit(0.3, 0);
        cast.flush();
        assert_eq!(s.finalize(&w, &clock).unwrap(), SessionState::Ended);
        assert_eq!(mem.sessions()[0].state, SessionState::Ended);
    }
}
