//! Test fixtures of the serve modules: cast and registry writers, a process probe, and a
//! small plain-text pipeline (one part, output as text, commands from `cmd`/`done`), so the
//! scheduler is tested independently of the avt renderer and the legacy importer.

use std::collections::HashMap;
use std::fs::File;
use std::io::BufWriter;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use cinelog_cast::{CastReader, CastWriter, EventCode, Header, ResumePoint, V3Writer};
use cinelog_core::Scope;
use cinelog_core::registry::{ProcessProbe, RecState, Registry, RegistryEntry, TtyKey};
use cinelog_index::model::{
    Checkpoint, Chunk, ChunkKind, Command, FileStamp, IndexBatch, LegacyImportRow, LegacyStatus, PollBudget,
    SessionCounters, SessionMeta, SessionState, TmuxLocation, legacy_uuid,
};
use cinelog_index::{CastKind, IndexError, PipelineFactory, SessionPipeline, classify};
use cinelog_proto::CastMarker;
use serde::{Deserialize, Serialize};

use super::engine::{Sources, sha256_first_mib};

pub const UUID_A: &str = "01934f5e-7b2a-7c3d-8e4f-00000000000a";
pub const UUID_B: &str = "01934f5e-7b2a-7c3d-8e4f-00000000000b";
pub const UUID_C: &str = "01934f5e-7b2a-7c3d-8e4f-00000000000c";
pub const UUID_D: &str = "01934f5e-7b2a-7c3d-8e4f-00000000000d";

/// Wall clock of t=0 in every test cast.
pub const START_EPOCH: i64 = 1_731_070_000;
pub const BOOT_ID: &str = "boot";
pub const STARTTIME: u64 = 7;

fn session_json(uuid: &str, part: u32) -> String {
    serde_json::json!({
        "cl": 1, "k": "session", "uuid": uuid, "part": part,
        "start_epoch_us": START_EPOCH * 1_000_000, "scope": "personal",
        "host": "h", "user": "u", "attribution": "ancestor",
    })
    .to_string()
}

pub fn cmd_json(seq: u64, sh: u32, n: u64, typed: &str) -> String {
    serde_json::json!({"cl": 1, "k": "cmd", "seq": seq, "sh": sh, "n": n, "typed": typed}).to_string()
}

pub fn done_json(seq: u64, sh: u32, n: u64, exit: i32) -> String {
    serde_json::json!({"cl": 1, "k": "done", "seq": seq, "sh": sh, "n": n, "exit": exit}).to_string()
}

/// A cinelog cast being written (v3, `session` marker at t=0). Times are absolute seconds.
pub struct CastFile {
    path: PathBuf,
    w: V3Writer<BufWriter<File>>,
    last: f64,
}

impl CastFile {
    pub fn create(dir: &Path, name: &str, uuid: &str, part: u32) -> Self {
        std::fs::create_dir_all(dir).unwrap();
        let path = dir.join(name);
        let mut w = cinelog_cast::create(&path).unwrap();
        w.write_header(&Header::new_v3(80, 24, None, START_EPOCH, None))
            .unwrap();
        w.write_event(0, EventCode::Marker, &session_json(uuid, part)).unwrap();
        Self { path, w, last: 0.0 }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn event(&mut self, t: f64, code: &str, data: &str) {
        let code = EventCode::from_char(code.chars().next().unwrap());
        self.w.write_event((t * 1e6).round() as u64, code, data).unwrap();
        self.last = self.last.max(t);
    }

    pub fn cmd(&mut self, t: f64, seq: u64, sh: u32, n: u64, typed: &str) {
        self.event(t, "m", &cmd_json(seq, sh, n, typed));
    }

    pub fn done(&mut self, t: f64, seq: u64, sh: u32, n: u64, exit: i32) {
        self.event(t, "m", &done_json(seq, sh, n, exit));
    }

    pub fn output(&mut self, t: f64, data: &str) {
        self.event(t, "o", data);
    }

    pub fn exit(&mut self, t: f64, status: i32) {
        self.event(t, "x", &status.to_string());
    }

    pub fn flush(&mut self) {
        self.w.flush().unwrap();
    }
}

/// Writes a complete cinelog cast; `exit` appends an `x` event 0.1 s after the last one.
pub fn write_cast(
    dir: &Path,
    name: &str,
    uuid: &str,
    part: u32,
    events: &[(f64, &str, &str)],
    exit: Option<i32>,
) -> PathBuf {
    let mut c = CastFile::create(dir, name, uuid, part);
    for (t, code, data) in events {
        c.event(*t, code, data);
    }
    if let Some(x) = exit {
        let t = c.last + 0.1;
        c.exit(t, x);
    }
    c.flush();
    c.path
}

/// Writes an asciinema v2 cast without a `session` marker (absolute times).
pub fn write_legacy(dir: &Path, name: &str, events: &[(f64, &str, &str)]) -> PathBuf {
    std::fs::create_dir_all(dir).unwrap();
    let mut s = format!(r#"{{"version":2,"width":80,"height":24,"timestamp":{START_EPOCH}}}"#);
    s.push('\n');
    for (t, code, data) in events {
        s.push_str(&format!(
            "[{t}, \"{code}\", {}]\n",
            serde_json::to_string(data).unwrap()
        ));
    }
    let p = dir.join(name);
    std::fs::write(&p, s).unwrap();
    p
}

/// `<plain>.zst` next to `plain`, which is kept.
pub fn compress_copy(plain: &Path) -> PathBuf {
    let z = super::compress::zst_path(plain);
    cinelog_cast::compress_file(plain, &z, cinelog_cast::CompressOptions::default()).unwrap();
    z
}

pub fn registry_entry(pid: u32, uuid: &str, cast: &Path) -> RegistryEntry {
    RegistryEntry {
        format: 1,
        pid,
        starttime: STARTTIME,
        boot_id: BOOT_ID.into(),
        uuid: uuid.into(),
        nonce: cinelog_core::Nonce::from_bytes([1; 16]),
        scope: Scope::Personal,
        cast: cast.to_path_buf(),
        pts: String::new(),
        host_tty: String::new(),
        shell_pid: 0,
        parent_uuid: None,
        state: RecState::Live,
        state_reason: None,
        part: 1,
        version: String::new(),
        opts: String::new(),
    }
}

/// Registers a live recorder of `uuid` on `/dev/pts/<pts>` in `dir`.
pub fn write_registry(dir: &Path, pts: u32, pid: u32, uuid: &str, cast: &Path) -> PathBuf {
    write_registry_state(dir, pts, pid, uuid, cast, RecState::Live)
}

/// [`write_registry`] with the recorder in `state`.
pub fn write_registry_state(dir: &Path, pts: u32, pid: u32, uuid: &str, cast: &Path, state: RecState) -> PathBuf {
    let r = Registry::new(dir);
    r.ensure_dir().unwrap();
    let mut e = registry_entry(pid, uuid, cast);
    e.pts = format!("/dev/pts/{pts}");
    e.state = state;
    r.write(&TtyKey::Pts(pts), &e).unwrap()
}

/// Processes that are "running": pid → starttime. Clones share the table.
#[derive(Debug, Clone, Default)]
pub struct FakeProbe(Arc<Mutex<HashMap<u32, u64>>>);

impl FakeProbe {
    pub fn alive(procs: &[(u32, u64)]) -> Self {
        Self(Arc::new(Mutex::new(procs.iter().copied().collect())))
    }

    pub fn kill(&self, pid: u32) {
        self.0.lock().unwrap().remove(&pid);
    }
}

impl ProcessProbe for FakeProbe {
    fn starttime(&self, pid: u32) -> Option<u64> {
        self.0.lock().unwrap().get(&pid).copied()
    }

    fn boot_id(&self) -> Option<String> {
        Some(BOOT_ID.into())
    }
}

/// A session row with test defaults.
pub fn session_meta(uuid: &str) -> SessionMeta {
    SessionMeta {
        uuid: uuid.into(),
        host: "h".into(),
        scope: Scope::Personal,
        user: "u".into(),
        euid: Some(1000),
        sudo_user: None,
        claimed_user: None,
        attribution: None,
        login_via: None,
        tty: None,
        holder_comm: None,
        tmux: TmuxLocation::default(),
        parent_uuid: None,
        cast_path: PathBuf::from(format!("/l/{uuid}.cast")),
        parts: 1,
        compressed: false,
        cast_bytes: None,
        started_at: START_EPOCH as f64,
        ended_at: None,
        exit_status: None,
        tz: None,
        cols: 80,
        rows: 24,
        term: None,
        legacy: false,
        hidden: false,
        state: SessionState::Live,
        counters: SessionCounters::default(),
    }
}

/// Factories of [`TestPipeline`]s; legacy casts whose name contains `FAILME` fail to open.
pub fn test_sources() -> Sources {
    Sources {
        cinelog: Ok(Box::new(TestFactory::default())),
        legacy: Ok(Box::new(TestFactory { legacy: true })),
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct TestFactory {
    pub legacy: bool,
}

impl PipelineFactory for TestFactory {
    fn open(&self, cast: &Path, checkpoint: Option<Checkpoint>) -> cinelog_index::Result<Box<dyn SessionPipeline>> {
        if self.legacy && cast.to_string_lossy().contains("FAILME") {
            return Err(IndexError::Invalid("cannot import this cast".into()));
        }
        Ok(Box::new(TestPipeline::open(cast, checkpoint, self.legacy)?))
    }
}

/// Resumable state of a [`TestPipeline`] (its `pipeline_state` JSON).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct State {
    session: Option<SessionMeta>,
    open: Option<Command>,
    /// Output of the open command not yet in a final chunk.
    pending: String,
    exited: bool,
    last_t_us: u64,
}

pub struct TestPipeline {
    path: PathBuf,
    uuid: String,
    legacy: bool,
    reader: Box<dyn CastReader + Send>,
    st: State,
    reset: bool,
    finished: bool,
}

impl TestPipeline {
    fn open(path: &Path, checkpoint: Option<Checkpoint>, legacy: bool) -> cinelog_index::Result<Self> {
        let stamp = FileStamp::of(path)?;
        let uuid = if legacy {
            let name = path.file_name().unwrap().to_string_lossy();
            legacy_uuid(&name, stamp.size, &sha256_first_mib(path)?)
        } else {
            match classify(path)? {
                CastKind::Cinelog { uuid, .. } => uuid,
                other => return Err(IndexError::Invalid(format!("not a cinelog cast: {other:?}"))),
            }
        };
        let usable = checkpoint.filter(|c| c.file.ino == stamp.ino && c.byte_offset <= stamp.size);
        let reset = usable.is_none();
        let (reader, st) = match usable {
            Some(c) => {
                let st: State = serde_json::from_str(&c.pipeline_state)?;
                let at = ResumePoint {
                    offset: c.byte_offset,
                    t_us: st.last_t_us,
                };
                (cinelog_cast::open_at(path, at, None)?, st)
            }
            None => (cinelog_cast::open(path)?, State::default()),
        };
        let mut me = Self {
            path: path.to_path_buf(),
            uuid,
            legacy,
            reader,
            st,
            reset,
            finished: false,
        };
        if legacy && me.st.session.is_none() {
            let mut s = session_meta(&me.uuid);
            s.cast_path = path.to_path_buf();
            s.legacy = true;
            me.st.session = Some(s);
        }
        Ok(me)
    }

    fn ts(&self, t: f64) -> f64 {
        self.st.session.as_ref().map_or(0.0, |s| s.started_at) + t
    }

    fn chunk(&self, kind: ChunkKind, cmd_seq: Option<u64>, t: f64, text: &str) -> Option<Chunk> {
        let text = text.replace('\r', "").trim_end().to_owned();
        (!text.is_empty()).then(|| Chunk {
            kind,
            cmd_seq,
            t,
            ts: self.ts(t),
            line0: 0,
            text,
        })
    }

    /// Closes the open command (with `done`, or at the last event without one).
    fn close_open(&mut self, done: Option<&cinelog_proto::Done>, t: f64, b: &mut IndexBatch) {
        let Some(mut c) = self.st.open.take() else { return };
        let pending = std::mem::take(&mut self.st.pending);
        b.chunks
            .extend(self.chunk(ChunkKind::Primary, Some(c.seq), c.t_start, &pending));
        c.close(done, t, self.ts(t));
        b.commands.push(c);
        b.provisional = Some(Vec::new());
    }

    fn event(&mut self, ev: cinelog_cast::Event, b: &mut IndexBatch) {
        let t = ev.t_secs();
        self.st.last_t_us = ev.t_us;
        match ev.code {
            EventCode::Marker => match CastMarker::from_json(&ev.data) {
                Ok(Some(CastMarker::Session(info))) if ev.t_us == 0 => {
                    let mut s = SessionMeta::from_session_marker(&info, self.reader.header(), &self.path);
                    s.cast_path = self.path.clone();
                    self.st.session = Some(s);
                }
                Ok(Some(CastMarker::Cmd(rec))) => {
                    self.close_open(None, t, b);
                    let c = Command::from_cmd(&rec, t, self.ts(t), None, 0);
                    b.commands.push(c.clone());
                    self.st.open = Some(c);
                }
                Ok(Some(CastMarker::Done(d))) if self.st.open.as_ref().map(|c| c.seq) == d.seq => {
                    self.close_open(Some(&d.done), t, b);
                }
                _ => {}
            },
            EventCode::Output => match &self.st.open {
                Some(_) => self.st.pending.push_str(&ev.data),
                None => b.chunks.extend(self.chunk(ChunkKind::Gap, None, t, &ev.data)),
            },
            EventCode::Exit => {
                self.close_open(None, t, b);
                self.st.exited = true;
                let ts = self.ts(t);
                if let Some(s) = &mut self.st.session {
                    s.state = SessionState::Ended;
                    s.ended_at = Some(ts);
                    s.exit_status = ev.data.trim().parse().ok();
                }
                b.finished = true;
            }
            _ => {}
        }
    }

    fn seal(&self, mut b: IndexBatch) -> cinelog_index::Result<IndexBatch> {
        b.session = self.st.session.clone().expect("session known");
        b.checkpoint = Some(Checkpoint {
            part: 1,
            byte_offset: self.reader.position(),
            t: self.st.last_t_us as f64 / 1e6,
            transcript: Default::default(),
            open_stack: Vec::new(),
            pipeline_state: serde_json::to_string(&self.st)?,
            file: FileStamp::of(&self.path)?,
        });
        if b.finished && self.legacy {
            b.legacy = Some(LegacyImportRow {
                path: self.path.clone(),
                file: FileStamp::of(&self.path)?,
                sha256: sha256_first_mib(&self.path)?,
                commands_log: None,
                status: LegacyStatus::Imported,
                session_uuid: Some(self.uuid.clone()),
                banners: 0,
                log_lines: 0,
                aligned: 0,
                mean_conf: None,
                error: None,
            });
        }
        Ok(b)
    }

    fn new_batch(&mut self) -> IndexBatch {
        let mut b = IndexBatch::new(session_meta(&self.uuid));
        if std::mem::take(&mut self.reset) {
            b.reset = true;
            b.parts = vec![cinelog_index::model::CastPart {
                part: 1,
                path: self.path.clone(),
                compressed: super::discover::is_zst(&self.path),
                bytes: None,
                t_offset: 0.0,
                frames: None,
            }];
        }
        b
    }
}

impl SessionPipeline for TestPipeline {
    fn uuid(&self) -> &str {
        &self.uuid
    }

    fn poll(&mut self, budget: PollBudget) -> cinelog_index::Result<Option<IndexBatch>> {
        if self.finished {
            return Ok(None);
        }
        let reset = self.reset;
        let mut b = self.new_batch();
        let start = self.reader.position();
        let mut read = false;
        while self.reader.position() - start < budget.max_bytes {
            match self.reader.next_event() {
                None => break,
                Some(ev) => {
                    self.event(ev?, &mut b);
                    read = true;
                }
            }
        }
        if (!read && !reset) || self.st.session.is_none() {
            self.reset |= reset;
            return Ok(None);
        }
        if budget.live
            && !b.finished
            && let Some(c) = &self.st.open
        {
            let seq = c.seq;
            let t = c.t_start;
            b.provisional = Some(
                self.chunk(ChunkKind::Provisional, Some(seq), t, &self.st.pending)
                    .into_iter()
                    .collect(),
            );
        }
        self.finished = b.finished;
        self.seal(b).map(Some)
    }

    fn finish(&mut self, state: SessionState) -> cinelog_index::Result<IndexBatch> {
        let mut b = self.new_batch();
        let t = self.st.last_t_us as f64 / 1e6;
        self.close_open(None, t, &mut b);
        let ts = self.ts(t);
        if let Some(s) = &mut self.st.session {
            s.state = state;
            s.ended_at.get_or_insert(ts);
        }
        b.provisional = Some(Vec::new());
        b.finished = true;
        self.finished = true;
        self.seal(b)
    }

    fn is_finished(&self) -> bool {
        self.st.exited
    }
}
