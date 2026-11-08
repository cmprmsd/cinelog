//! Data types exchanged between the M2 work packages (plan §6, §9.5, §11). Frozen for M2:
//! additive changes only, through the contracts owner.
//!
//! Time conventions everywhere: `t*` fields are session-relative cast offsets in seconds
//! (the part's `t_offset` already added), `ts*` fields are UTC Unix epoch seconds.

use std::collections::BTreeMap;
use std::fmt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::Duration;

use cinelog_core::config::IndexConfig;
use cinelog_proto::{CastMarker, CmdRecord, Done, Scope, SessionInfo, TmuxContext};
use serde::{Deserialize, Serialize};

use crate::error::IndexError;

/// Recorder-assigned command number, unique per session (legacy: importer-assigned).
pub type Seq = u64;

// ------------------------------------------------------------------ constants

pub const INDEX_DB: &str = "index.db";
pub const META_DB: &str = "meta.db";
/// flock(2) of the single writer (§6.4). `cinelog index` runs only when it is free.
pub const WRITER_LOCK: &str = "index.db.writer.lock";
/// Nightly meta backups kept next to meta.db (§6.3).
pub const META_BACKUPS_KEPT: usize = 14;

/// Snippet highlight delimiters (§6.5, §7.6). Never HTML; renderers escape the text and then
/// replace these.
pub const SNIPPET_OPEN: char = '\u{E000}';
pub const SNIPPET_CLOSE: char = '\u{E001}';

/// FTS candidate cap per query (§6.5): more matches set [`SearchPage::capped`].
pub const CANDIDATE_CAP: usize = 5000;
/// Terms with fewer characters cannot use the trigram index (§6.5).
pub const MIN_TRIGRAM_CHARS: usize = 3;
/// Time limit of the newest-first scan for short terms (§6.5).
pub const SHORT_TERM_SCAN: Duration = Duration::from_millis(500);

/// Live indexing commit interval (§6.4).
pub const COMMIT_INTERVAL: Duration = Duration::from_secs(2);
/// Logical lines per output chunk (bytes come from `index.chunk_kb`).
pub const CHUNK_LINES: usize = 128;
/// Alt-screen snapshot interval while the alt screen stays active (§6.4).
pub const ALT_SNAPSHOT_SECS: f64 = 10.0;
/// Playback segment cut: about every 2 MiB of events or 60 s (§6.4).
pub const PLAYBACK_SEGMENT_BYTES: u64 = 2 << 20;
pub const PLAYBACK_SEGMENT_SECS: f64 = 60.0;

/// Keys of `index.db` table `index_info`.
pub mod info_keys {
    /// Last indexer error (single line, no cast content).
    pub const LAST_ERROR: &str = "last_error";
    /// Pid of the process holding the writer lock.
    pub const WRITER_PID: &str = "writer_pid";
    /// ts of the writer's last commit.
    pub const WRITER_HEARTBEAT: &str = "writer_heartbeat";
    /// cinelog version that created this index.
    pub const BUILT_VERSION: &str = "built_version";
    /// ts when a rebuild started; removed when it completes.
    pub const REBUILD_STARTED: &str = "rebuild_started";
}

/// Keys of `meta.db` table `settings`.
pub mod settings_keys {
    /// The persisted server port (§7.1), imported once from `<db_dir>/migrated-port`.
    pub const PORT: &str = "port";
    pub const CERT_FINGERPRINT: &str = "cert_fingerprint";
    /// ts of the last nightly meta backup.
    pub const LAST_BACKUP: &str = "last_backup";
    /// Prefix of the playback span of a forgotten command: `forget_span/<uuid>/<seq>`.
    pub const FORGET_SPAN_PREFIX: &str = "forget_span/";
}

// ------------------------------------------------------------------ locations

/// Database files of one scope, all in `Paths::db_dir`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DbPaths {
    pub dir: PathBuf,
    pub index: PathBuf,
    pub meta: PathBuf,
    pub writer_lock: PathBuf,
}

impl DbPaths {
    pub fn in_dir(dir: impl Into<PathBuf>) -> Self {
        let dir = dir.into();
        Self {
            index: dir.join(INDEX_DB),
            meta: dir.join(META_DB),
            writer_lock: dir.join(WRITER_LOCK),
            dir,
        }
    }

    /// `meta-YYYYMMDD.jsonl.zst` for `date`.
    pub fn meta_backup(&self, date: jiff::civil::Date) -> PathBuf {
        self.dir.join(format!("meta-{}.jsonl.zst", date.strftime("%Y%m%d")))
    }
}

/// Identity of a file version: discovery skips casts whose stamp did not change.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct FileStamp {
    pub size: u64,
    /// Seconds since the epoch, with sub-second precision.
    pub mtime: f64,
    pub ino: u64,
}

impl FileStamp {
    pub fn of(path: &Path) -> std::io::Result<Self> {
        Ok(Self::from_metadata(&std::fs::metadata(path)?))
    }

    pub fn from_metadata(md: &std::fs::Metadata) -> Self {
        Self {
            size: md.size(),
            mtime: md.mtime() as f64 + f64::from(md.mtime_nsec() as u32) / 1e9,
            ino: md.ino(),
        }
    }
}

// ------------------------------------------------------------------ enums stored as TEXT

macro_rules! text_enum {
    ($(#[$m:meta])* $name:ident { $($(#[$vm:meta])* $var:ident = $s:literal),+ $(,)? }) => {
        $(#[$m])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
        #[serde(rename_all = "lowercase")]
        pub enum $name { $($(#[$vm])* $var),+ }

        impl $name {
            pub const ALL: &'static [Self] = &[$(Self::$var),+];

            pub const fn as_str(self) -> &'static str {
                match self { $(Self::$var => $s),+ }
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(self.as_str())
            }
        }

        impl FromStr for $name {
            type Err = IndexError;
            fn from_str(s: &str) -> Result<Self, IndexError> {
                match s {
                    $($s => Ok(Self::$var),)+
                    _ => Err(IndexError::Invalid(format!(concat!(stringify!($name), " {:?}"), s))),
                }
            }
        }
    };
}

text_enum! {
    /// `sessions.state`.
    SessionState {
        /// The recorder is (believed to be) running.
        Live = "live",
        /// The cast has its `x` event (or is a complete legacy cast).
        Ended = "ended",
        /// The recorder died without `x` (registry pid/starttime/boot_id dead).
        Crashed = "crashed",
        /// The recorder hit a write error and went passthrough-only (§5.8).
        Degraded = "degraded",
        /// The cast could not be indexed (unreadable header, …).
        Error = "error",
    }
}

text_enum! {
    /// `outputs.kind`.
    ChunkKind {
        /// Primary-screen rows of a command, final.
        Primary = "primary",
        /// De-duplicated alt-screen snapshot rows.
        Alt = "alt",
        /// Rows outside any command (prompts, leading-space commands, legacy without banners).
        Gap = "gap",
        /// On-screen rows of a running command; replaced on every live tick.
        Provisional = "provisional",
    }
}

text_enum! {
    /// `legacy_import.status`.
    LegacyStatus {
        Imported = "imported",
        /// Imported with `sessions.hidden = 1` (`TERM=dumb` artefacts, §11.7).
        Hidden = "hidden",
        /// Not a legacy cast after all, or excluded by scope.
        Skipped = "skipped",
        Failed = "failed",
    }
}

// ------------------------------------------------------------------ sessions

/// tmux location of a pane recording (tree view, §8.2).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct TmuxLocation {
    /// `<server_pid>:<server_starttime>`.
    pub server: Option<String>,
    pub session_id: Option<String>,
    pub session: Option<String>,
    pub window: Option<String>,
    pub window_name: Option<String>,
    pub pane: Option<String>,
}

impl TmuxLocation {
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    /// Applies a `ctx` marker (renames, break/join/move-pane).
    pub fn apply_ctx(&mut self, ctx: &TmuxContext) {
        let set = |dst: &mut Option<String>, v: &str| {
            if !v.is_empty() {
                *dst = Some(v.to_owned());
            }
        };
        set(&mut self.session, &ctx.session_name);
        set(&mut self.window, &ctx.window_index);
        set(&mut self.window_name, &ctx.window_name);
        set(&mut self.pane, &ctx.pane_id);
    }
}

/// Cumulative per-session counters (absolute values, not deltas).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct SessionCounters {
    pub bad_lines: u64,
    pub bad_markers: u64,
    pub lossy_bytes: u64,
}

/// One `sessions` row (without the rowid). Upserted by uuid with every batch.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionMeta {
    pub uuid: String,
    pub host: String,
    pub scope: Scope,
    pub user: String,
    pub euid: Option<u32>,
    pub sudo_user: Option<String>,
    pub claimed_user: Option<String>,
    /// `cinelog_proto::Attribution` as text (`ancestor`, …, `legacy`).
    pub attribution: Option<String>,
    pub login_via: Option<String>,
    pub tty: Option<String>,
    pub holder_comm: Option<String>,
    pub tmux: TmuxLocation,
    pub parent_uuid: Option<String>,
    /// First part.
    pub cast_path: PathBuf,
    pub parts: u32,
    pub compressed: bool,
    pub cast_bytes: Option<u64>,
    pub started_at: f64,
    pub ended_at: Option<f64>,
    pub exit_status: Option<i32>,
    pub tz: Option<String>,
    pub cols: u16,
    pub rows: u16,
    pub term: Option<String>,
    pub legacy: bool,
    pub hidden: bool,
    pub state: SessionState,
    pub counters: SessionCounters,
}

impl SessionMeta {
    /// Session row of a cinelog cast from its header and its t=0 `session` marker.
    pub fn from_session_marker(info: &SessionInfo, header: &cinelog_cast::Header, cast_path: &Path) -> Self {
        let tmux = info
            .tmux
            .as_ref()
            .map(|t| TmuxLocation {
                server: (t.server_pid != 0).then(|| format!("{}:{}", t.server_pid, t.server_start)),
                session_id: non_empty(&t.session_id),
                pane: non_empty(&t.pane),
                ..TmuxLocation::default()
            })
            .unwrap_or_default();
        Self {
            uuid: info.uuid.clone(),
            host: info.host.clone(),
            scope: info.scope,
            user: info.user.clone(),
            euid: Some(info.euid),
            sudo_user: info.sudo_user.clone(),
            claimed_user: info.claimed_user.clone(),
            attribution: Some(serde_text(&info.attribution)),
            login_via: info.login_via.clone(),
            tty: non_empty(&info.tty),
            holder_comm: info.holder_comm.clone(),
            tmux,
            parent_uuid: info.parent_uuid.clone(),
            cast_path: cast_path.to_owned(),
            parts: info.part.max(1),
            compressed: false,
            cast_bytes: None,
            started_at: info.start_epoch_us as f64 / 1e6,
            ended_at: None,
            exit_status: None,
            tz: non_empty(&info.tz),
            cols: header.term.cols,
            rows: header.term.rows,
            term: header.term.term_type.clone(),
            legacy: false,
            hidden: false,
            state: SessionState::Live,
            counters: SessionCounters::default(),
        }
    }
}

/// One `cast_parts` row.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CastPart {
    /// 1-based.
    pub part: u32,
    pub path: PathBuf,
    pub compressed: bool,
    /// Uncompressed bytes, once known.
    pub bytes: Option<u64>,
    pub t_offset: f64,
    /// Frame table of a compressed part; `Some` replaces its `zst_frames` rows, `None`
    /// leaves them as they are.
    pub frames: Option<Vec<ZstFrameRow>>,
}

// ------------------------------------------------------------------ commands and output

/// One `commands` row (without rowid and session id).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Command {
    pub seq: Seq,
    pub parent_seq: Option<Seq>,
    pub depth: u32,
    pub shell_pid: Option<u32>,
    pub t_start: f64,
    pub t_end: Option<f64>,
    pub ts_start: f64,
    pub ts_end: Option<f64>,
    pub exit: Option<i32>,
    /// Stored as a JSON array; empty means unknown (NULL).
    pub pipestatus: Vec<i32>,
    pub cwd: Option<String>,
    pub user: Option<String>,
    pub euid: Option<u32>,
    pub typed: String,
    pub expanded: Option<String>,
    pub typed_trunc: bool,
    pub approx: bool,
    pub confidence: Option<f64>,
    pub out_bytes: u64,
    pub out_truncated: bool,
}

impl Command {
    /// An open command from a recorded `cmd` marker at `(t, ts)`.
    pub fn from_cmd(rec: &CmdRecord, t: f64, ts: f64, parent_seq: Option<Seq>, depth: u32) -> Self {
        let c = &rec.cmd;
        Self {
            seq: rec.seq,
            parent_seq,
            depth,
            shell_pid: Some(c.sh),
            t_start: t,
            t_end: None,
            ts_start: ts,
            ts_end: None,
            exit: None,
            pipestatus: Vec::new(),
            cwd: non_empty(&c.cwd),
            user: non_empty(&c.user),
            euid: c.euid,
            typed: c.typed.clone(),
            expanded: non_empty(&c.expanded),
            typed_trunc: c.trunc,
            approx: false,
            confidence: None,
            out_bytes: 0,
            out_truncated: false,
        }
    }

    /// Closes the command at `(t, ts)`. `done` is `None` for inner commands popped without
    /// their own `done` (exit stays NULL, §6.4).
    pub fn close(&mut self, done: Option<&Done>, t: f64, ts: f64) {
        self.t_end = Some(t);
        self.ts_end = Some(ts);
        if let Some(d) = done {
            self.exit = Some(d.exit);
            self.pipestatus = d.pipestatus.clone();
        }
    }

    pub fn is_open(&self) -> bool {
        self.t_end.is_none()
    }
}

/// One `outputs` row: a run of consecutive logical lines (soft wraps joined, trailing
/// blanks trimmed), at most `index.chunk_kb` KiB and [`CHUNK_LINES`] lines.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Chunk {
    pub kind: ChunkKind,
    /// Owning command; `None` for `gap` output.
    pub cmd_seq: Option<Seq>,
    pub t: f64,
    pub ts: f64,
    /// Ordinal of the first line in its owner's transcript (alt: snapshot line ordinal).
    pub line0: u64,
    pub text: String,
}

/// Who receives the rows a [`crate::api::Transcript`] renders.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Owner {
    Command(Seq),
    Gap,
}

impl Owner {
    pub fn cmd_seq(self) -> Option<Seq> {
        match self {
            Self::Command(s) => Some(s),
            Self::Gap => None,
        }
    }
}

/// A point in time of the session: both clocks, so renderers never need a clock map.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct Stamp {
    pub t: f64,
    pub ts: f64,
}

/// Output accounting of one owner (feeds `commands.out_bytes`/`out_truncated`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct OutputStats {
    pub bytes: u64,
    pub lines: u64,
    /// `index.max_output_per_command_mb` reached; later rows were dropped.
    pub truncated: bool,
}

/// Renderer settings, from `[index]`.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct RenderOptions {
    /// `index.output = false`: no output chunks at all (commands only).
    pub output: bool,
    /// `index.gap_output = false`: rows outside commands are dropped.
    pub gap_output: bool,
    pub chunk_bytes: usize,
    pub chunk_lines: usize,
    pub max_output_per_command: u64,
    pub alt_snapshots: bool,
    pub alt_snapshot_secs: f64,
}

impl Default for RenderOptions {
    fn default() -> Self {
        Self::from(&IndexConfig::default())
    }
}

impl From<&IndexConfig> for RenderOptions {
    fn from(c: &IndexConfig) -> Self {
        Self {
            output: c.output,
            gap_output: c.gap_output,
            chunk_bytes: (c.chunk_kb as usize).max(1) * 1024,
            chunk_lines: CHUNK_LINES,
            max_output_per_command: c.max_output_per_command_mb.saturating_mul(1 << 20),
            alt_snapshots: c.alt_snapshots,
            alt_snapshot_secs: ALT_SNAPSHOT_SECS,
        }
    }
}

/// Serializable renderer state (inside a [`Checkpoint`]).
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct TranscriptState {
    /// avt `Terminal::dump()`.
    pub vt_dump: String,
    /// Primary rows scrolled off so far.
    pub evicted: u64,
    /// Renderer-private JSON (open spans, row times, partial lines, alt de-dup state, …).
    pub state: String,
}

// ------------------------------------------------------------------ markers and playback

/// One `markers` row.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MarkerRow {
    pub t: f64,
    pub ts: f64,
    pub kind: String,
    /// The `m` payload as in the cast.
    pub data_json: String,
}

impl MarkerRow {
    /// Marker kinds kept in `markers` for the timeline. `cmd`/`done` become commands,
    /// `session`/`hello` only feed the session row; unknown kinds are kept raw (§4.6).
    pub const TIMELINE_KINDS: &'static [&'static str] = &[
        "pause",
        "resume",
        "nested",
        "multiplexer",
        "ctx",
        "gap",
        "rotate",
        "error",
        "clock",
        "skip",
    ];

    /// Row for a decoded marker, `None` for kinds that are not kept.
    pub fn from_marker(marker: &CastMarker, at: Stamp) -> Option<Self> {
        let keep = match marker {
            CastMarker::Unknown { .. } => true,
            m => Self::TIMELINE_KINDS.contains(&m.kind()),
        };
        keep.then(|| Self {
            t: at.t,
            ts: at.ts,
            kind: marker.kind().to_owned(),
            data_json: marker.to_json(),
        })
    }
}

/// One `segments` row: a playback segment of the player's segmented format.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Segment {
    /// 0-based, in time order across parts.
    pub seq: u32,
    pub part: u32,
    pub t_start: f64,
    /// `None` for the last, still growing segment.
    pub t_end: Option<f64>,
    /// Uncompressed offset of the segment's first event line in `part`.
    pub byte_offset: u64,
    /// Terminal state at `t_start`.
    pub vt_dump: String,
}

/// When a pipeline cuts the next playback segment (§6.4).
pub fn segment_due(bytes_since_cut: u64, secs_since_cut: f64) -> bool {
    bytes_since_cut >= PLAYBACK_SEGMENT_BYTES || secs_since_cut >= PLAYBACK_SEGMENT_SECS
}

// ------------------------------------------------------------------ checkpoints and batches

/// One entry of the segmenter's stack of open commands (§6.4).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpenCommand {
    pub seq: Seq,
    /// Shell pid and shell-local counter of the `cmd` (`done` matches by both).
    pub sh: u32,
    pub n: u64,
    pub depth: u32,
}

/// `index_state` row: where to resume a session (always at an event-line boundary).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Checkpoint {
    pub part: u32,
    pub byte_offset: u64,
    pub t: f64,
    pub transcript: TranscriptState,
    pub open_stack: Vec<OpenCommand>,
    /// Pipeline-private JSON (clock anchors, segment cut state, legacy aligner state, …).
    pub pipeline_state: String,
    /// The part file as of this checkpoint.
    pub file: FileStamp,
}

/// Legacy import facts of one file (`legacy_import` row). Aggregates only.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LegacyImportRow {
    pub path: PathBuf,
    pub file: FileStamp,
    /// Lowercase hex sha256 of the first 1 MiB.
    pub sha256: String,
    pub commands_log: Option<PathBuf>,
    pub status: LegacyStatus,
    pub session_uuid: Option<String>,
    pub banners: u64,
    pub log_lines: u64,
    pub aligned: u64,
    pub mean_conf: Option<f64>,
    /// Single line, never cast content.
    pub error: Option<String>,
}

/// Everything one pipeline step produces for one session; the writer applies it in one
/// transaction (§6.4 "single writer").
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IndexBatch {
    /// Full session row, upserted by uuid.
    pub session: SessionMeta,
    /// Delete every derived row of this session first (cast replaced or truncated).
    pub reset: bool,
    pub parts: Vec<CastPart>,
    /// New or changed commands, upserted by `(session, seq)`.
    pub commands: Vec<Command>,
    /// Final chunks, appended. `cmd_seq` refers to a command of this or an earlier batch.
    pub chunks: Vec<Chunk>,
    /// `Some`: replaces all provisional chunks of the session (`Some(vec![])` clears them).
    pub provisional: Option<Vec<Chunk>>,
    pub markers: Vec<MarkerRow>,
    /// Upserted by `seq` (the last segment's `t_end` gets filled in later).
    pub segments: Vec<Segment>,
    pub checkpoint: Option<Checkpoint>,
    /// Legacy pipelines: the import row, written in the same transaction (final batch).
    pub legacy: Option<LegacyImportRow>,
    /// Last batch of this session: no more events will follow.
    pub finished: bool,
}

impl IndexBatch {
    pub fn new(session: SessionMeta) -> Self {
        Self {
            session,
            reset: false,
            parts: Vec::new(),
            commands: Vec::new(),
            chunks: Vec::new(),
            provisional: None,
            markers: Vec::new(),
            segments: Vec::new(),
            checkpoint: None,
            legacy: None,
            finished: false,
        }
    }

    /// Whether applying the batch would change anything besides the session row.
    pub fn is_empty(&self) -> bool {
        !self.reset
            && self.parts.is_empty()
            && self.commands.is_empty()
            && self.chunks.is_empty()
            && self.provisional.is_none()
            && self.markers.is_empty()
            && self.segments.is_empty()
            && self.checkpoint.is_none()
            && self.legacy.is_none()
            && !self.finished
    }
}

/// How much one [`crate::api::SessionPipeline::poll`] may read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PollBudget {
    /// Stop after about this many uncompressed bytes (at the next line boundary).
    pub max_bytes: u64,
    /// Live tick: emit the provisional chunk of the open command.
    pub live: bool,
}

/// A compressed part (§6.6), recorded after the rename to `.cast.zst`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CompressionUpdate {
    pub uuid: String,
    pub part: u32,
    pub from: PathBuf,
    pub to: PathBuf,
    pub uncompressed_bytes: u64,
    pub compressed_bytes: u64,
    pub frames: Vec<ZstFrameRow>,
}

/// One `zst_frames` row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ZstFrameRow {
    pub seq: u32,
    pub c_offset: u64,
    pub c_len: u64,
    pub u_offset: u64,
    pub u_len: u64,
}

impl ZstFrameRow {
    pub fn from_frames(frames: &[cinelog_cast::ZstFrame]) -> Vec<Self> {
        frames
            .iter()
            .zip(0u32..)
            .map(|(f, seq)| Self {
                seq,
                c_offset: f.c_offset,
                c_len: f.c_len,
                u_offset: f.u_offset,
                u_len: f.u_len,
            })
            .collect()
    }

    pub fn to_frame(self) -> cinelog_cast::ZstFrame {
        cinelog_cast::ZstFrame {
            c_offset: self.c_offset,
            c_len: self.c_len,
            u_offset: self.u_offset,
            u_len: self.u_len,
        }
    }
}

/// A session as discovery sees it (from `sessions` + `index_state`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct KnownCast {
    pub uuid: String,
    pub cast_path: PathBuf,
    pub legacy: bool,
    pub state: SessionState,
    pub parts: u32,
    pub compressed: bool,
    pub ended_at: Option<f64>,
    /// From the checkpoint: the part and offset indexed so far, and the file stamp then.
    pub indexed_part: Option<u32>,
    pub indexed_offset: Option<u64>,
    pub indexed_file: Option<FileStamp>,
}

/// A finished, non-legacy part old enough to compress.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CompressCandidate {
    pub uuid: String,
    pub part: u32,
    pub path: PathBuf,
    pub ended_at: f64,
}

/// `cinelog index` (one-shot backfill).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct OnceOptions {
    /// Drop all derived rows first (`--rebuild`); meta.db is untouched.
    pub rebuild: bool,
    /// Only these casts; empty means discovery over the whole logs root.
    pub paths: Vec<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct OnceSummary {
    pub casts_seen: u64,
    pub indexed: u64,
    pub unchanged: u64,
    pub failed: u64,
    pub bytes: u64,
    pub commands: u64,
    pub chunks: u64,
    pub elapsed_secs: f64,
    /// One line per failed cast: `<path>: <error>` (no content).
    pub errors: Vec<String>,
}

/// Settings of the legacy importer (`legacy::pipelines`).
#[derive(Debug, Clone, PartialEq)]
pub struct LegacyOptions {
    pub render: RenderOptions,
    /// PERSONAL: the user is the file owner. TEAM: the directory name below `logs_root`,
    /// `euid = 0`. Legacy personal casts containing old sudo sessions stay personal (§15.12).
    pub scope: Scope,
    pub logs_root: PathBuf,
    /// `sessions.host` of imported casts (local only).
    pub host: String,
}

// ------------------------------------------------------------------ forget and meta

/// What `cinelog forget` removes (§7.6).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum ForgetTarget {
    Session {
        uuid: String,
    },
    Command {
        uuid: String,
        seq: Seq,
    },
    /// Session-relative seconds, inclusive.
    Range {
        uuid: String,
        t_from: f64,
        t_to: f64,
    },
}

impl ForgetTarget {
    pub fn uuid(&self) -> &str {
        match self {
            Self::Session { uuid } | Self::Command { uuid, .. } | Self::Range { uuid, .. } => uuid,
        }
    }

    /// `kind` column of `meta.forgotten`.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Session { .. } => "session",
            Self::Command { .. } => "command",
            Self::Range { .. } => "range",
        }
    }
}

/// One `meta.forgotten` row.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Forgotten {
    pub target: ForgetTarget,
    pub by: String,
    pub at: f64,
}

/// Rows removed from index.db by a forget pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ForgetOutcome {
    pub sessions: u64,
    pub commands: u64,
    pub outputs: u64,
}

/// One annotation (meta.db). Tags by name.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Annotation {
    pub uuid: String,
    pub session_uuid: String,
    pub cmd_seq: Option<Seq>,
    pub t_from: Option<f64>,
    pub t_to: Option<f64>,
    pub author: String,
    pub body: String,
    pub tags: Vec<String>,
    pub created_at: f64,
    pub updated_at: f64,
    pub deleted_at: Option<f64>,
}

/// Filter of `MetaStore::find_annotations`; results are ordered by `(created_at, uuid)`.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct AnnotationQuery {
    /// Only annotations of these sessions; empty means every session.
    pub sessions: Vec<String>,
    pub cmd_seq: Option<Seq>,
    /// Only annotations carrying this tag (exact name).
    pub tag: Option<String>,
    pub author: Option<String>,
    /// Include soft-deleted annotations.
    pub include_deleted: bool,
    /// Keyset cursor: only rows strictly after this `(created_at, uuid)`.
    pub after: Option<(f64, String)>,
    /// At most this many rows; 0 means no limit.
    pub limit: usize,
}

/// A `tags` row with the number of annotations (not soft-deleted) that carry it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TagInfo {
    pub name: String,
    /// `#rrggbb`, or `None` for the automatic colour.
    pub color: Option<String>,
    pub count: u64,
}

// ------------------------------------------------------------------ search

/// Which tables a search covers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SearchTarget {
    /// typed, expanded, cwd (default).
    #[default]
    Commands,
    /// Output chunks only (`--out`).
    Output,
    /// Both (`-a`).
    All,
}

/// `--exit`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ExitFilter {
    Code(i32),
    Success,
    /// Any non-zero exit (NULL exits never match).
    Failure,
}

impl FromStr for ExitFilter {
    type Err = IndexError;
    fn from_str(s: &str) -> Result<Self, IndexError> {
        match s {
            "ok" | "success" => Ok(Self::Success),
            "fail" | "failure" | "!0" => Ok(Self::Failure),
            n => n
                .parse()
                .map(Self::Code)
                .map_err(|_| IndexError::Invalid(format!("exit filter {n:?} (number, ok or fail)"))),
        }
    }
}

impl ExitFilter {
    pub fn matches(self, exit: Option<i32>) -> bool {
        match (self, exit) {
            (Self::Code(c), Some(e)) => c == e,
            (Self::Success, Some(e)) => e == 0,
            (Self::Failure, Some(e)) => e != 0,
            (_, None) => false,
        }
    }
}

/// Filters joined to the FTS candidates (§6.5). All optional; `None` matches everything.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct SearchFilters {
    pub user: Option<String>,
    pub host: Option<String>,
    /// ts bounds, inclusive.
    pub since: Option<f64>,
    pub until: Option<f64>,
    pub exit: Option<ExitFilter>,
    /// Exact command cwd (`--cwd .` is resolved by the CLI).
    pub cwd: Option<String>,
    /// tmux pane id (`%3`).
    pub pane: Option<String>,
    pub session: Option<String>,
    /// Include `sessions.hidden = 1`.
    pub include_hidden: bool,
}

/// Pagination cursor `(ts, kind, id)`, ordered by `ts DESC, id DESC` (§6.5).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Cursor {
    pub ts: f64,
    pub kind: HitKind,
    pub id: i64,
}

impl fmt::Display for Cursor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let k = match self.kind {
            HitKind::Command => 'c',
            HitKind::Output => 'o',
        };
        write!(f, "{}:{k}:{}", self.ts, self.id)
    }
}

impl FromStr for Cursor {
    type Err = IndexError;
    fn from_str(s: &str) -> Result<Self, IndexError> {
        let bad = || IndexError::Invalid(format!("cursor {s:?}"));
        let mut it = s.splitn(3, ':');
        let ts = it
            .next()
            .and_then(|v| v.parse::<f64>().ok())
            .filter(|v| v.is_finite())
            .ok_or_else(bad)?;
        let kind = match it.next() {
            Some("c") => HitKind::Command,
            Some("o") => HitKind::Output,
            _ => return Err(bad()),
        };
        let id = it.next().and_then(|v| v.parse().ok()).ok_or_else(bad)?;
        Ok(Self { ts, kind, id })
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SearchQuery {
    /// Substring, Unicode case-insensitive unless `case_sensitive`. Empty: newest commands.
    pub term: String,
    pub target: SearchTarget,
    pub case_sensitive: bool,
    pub filters: SearchFilters,
    pub limit: usize,
    pub cursor: Option<Cursor>,
}

impl SearchQuery {
    pub fn new(term: impl Into<String>) -> Self {
        Self {
            term: term.into(),
            target: SearchTarget::default(),
            case_sensitive: false,
            filters: SearchFilters::default(),
            limit: 200,
            cursor: None,
        }
    }

    /// Short terms run the time-limited scan instead of the trigram index.
    pub fn is_short(&self) -> bool {
        let n = self.term.chars().count();
        n > 0 && n < MIN_TRIGRAM_CHARS
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HitKind {
    Command,
    Output,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SearchHit {
    pub kind: HitKind,
    /// `commands.id` or `outputs.id`.
    pub id: i64,
    pub session_uuid: String,
    /// The command (for output hits: the owning command; `None` for gap output).
    pub cmd_seq: Option<Seq>,
    pub ts: f64,
    /// Seek position: the command start, or the chunk's `t` for output hits.
    pub t: f64,
    pub user: String,
    pub host: String,
    pub exit: Option<i32>,
    pub cwd: Option<String>,
    pub typed: Option<String>,
    /// Output hits: text around the first match with [`SNIPPET_OPEN`]/[`SNIPPET_CLOSE`].
    pub snippet: Option<String>,
    pub chunk_kind: Option<ChunkKind>,
    pub approx: bool,
    pub confidence: Option<f64>,
}

impl SearchHit {
    pub fn entry(&self) -> EntryRef {
        match self.cmd_seq {
            Some(seq) => EntryRef::Command {
                uuid: self.session_uuid.clone(),
                seq,
            },
            None => EntryRef::At {
                uuid: self.session_uuid.clone(),
                t: self.t,
            },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct SearchPage {
    pub hits: Vec<SearchHit>,
    pub next: Option<Cursor>,
    /// The short-term scan hit its time limit: there may be more (older) matches.
    pub partial: bool,
    /// More than [`CANDIDATE_CAP`] candidates: refine the query.
    pub capped: bool,
}

/// Address of a session, a command or a point in a session: `<uuid>`, `<uuid>:<seq>` or
/// `<uuid>@<t>`. This is the hidden first fzf field and the `cinelog show` argument (§9.5).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum EntryRef {
    Session { uuid: String },
    Command { uuid: String, seq: Seq },
    At { uuid: String, t: f64 },
}

impl EntryRef {
    pub fn uuid(&self) -> &str {
        match self {
            Self::Session { uuid } | Self::Command { uuid, .. } | Self::At { uuid, .. } => uuid,
        }
    }
}

impl fmt::Display for EntryRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Session { uuid } => f.write_str(uuid),
            Self::Command { uuid, seq } => write!(f, "{uuid}:{seq}"),
            Self::At { uuid, t } => write!(f, "{uuid}@{t}"),
        }
    }
}

impl FromStr for EntryRef {
    type Err = IndexError;
    fn from_str(s: &str) -> Result<Self, IndexError> {
        let bad = || IndexError::Invalid(format!("entry {s:?} (expected <uuid>, <uuid>:<seq> or <uuid>@<t>)"));
        let (uuid, rest) = match s.find([':', '@']) {
            Some(i) => (&s[..i], Some((s.as_bytes()[i], &s[i + 1..]))),
            None => (s, None),
        };
        let uuid = uuid::Uuid::try_parse(uuid).map_err(|_| bad())?.hyphenated().to_string();
        Ok(match rest {
            None => Self::Session { uuid },
            Some((b':', seq)) => Self::Command {
                uuid,
                seq: seq.parse().map_err(|_| bad())?,
            },
            Some((_, t)) => Self::At {
                uuid,
                t: t.parse::<f64>()
                    .ok()
                    .filter(|t| t.is_finite() && *t >= 0.0)
                    .ok_or_else(bad)?,
            },
        })
    }
}

/// Session header for `show`, `status` and search result rendering.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionSummary {
    pub uuid: String,
    pub host: String,
    pub user: String,
    pub scope: Scope,
    pub state: SessionState,
    pub started_at: f64,
    pub ended_at: Option<f64>,
    pub cast_path: PathBuf,
    pub tmux: TmuxLocation,
    pub legacy: bool,
    pub hidden: bool,
    pub commands: u64,
}

/// What `cinelog show <entry>` prints.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EntryDetail {
    pub session: SessionSummary,
    /// The addressed command (`Command`, or the command running at `At`'s t).
    pub command: Option<Command>,
    /// Output text: of the command, of the gap span around `At`, or of the session start.
    pub output: String,
    /// More output exists than `max_output_bytes`.
    pub output_truncated: bool,
}

// ------------------------------------------------------------------ status

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct SessionCounts {
    pub live: u64,
    pub ended: u64,
    pub crashed: u64,
    pub degraded: u64,
    pub error: u64,
    pub legacy: u64,
    pub hidden: u64,
}

/// Indexer lag of one live session.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LiveLag {
    pub uuid: String,
    pub cast_path: PathBuf,
    pub indexed_bytes: u64,
    /// Current size of the indexed part (stat at status time).
    pub file_bytes: Option<u64>,
    /// ts of the last checkpoint.
    pub updated_at: Option<f64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct LegacyProgress {
    pub files: u64,
    pub imported: u64,
    pub hidden: u64,
    pub skipped: u64,
    pub failed: u64,
    pub banners: u64,
    pub log_lines: u64,
    pub aligned: u64,
}

/// `cinelog status` / `/api/status` (§6.4 "Status").
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct IndexStatus {
    pub schema_version: u32,
    pub index_bytes: u64,
    pub sessions: SessionCounts,
    pub commands: u64,
    pub outputs: u64,
    pub live: Vec<LiveLag>,
    /// Unindexed bytes over all known casts (file sizes minus checkpoint offsets).
    pub backlog_bytes: u64,
    pub legacy: LegacyProgress,
    pub last_error: Option<String>,
    /// All of `index_info`.
    pub info: BTreeMap<String, String>,
}

// ------------------------------------------------------------------ legacy identity

/// Deterministic session uuid of a legacy cast (UUIDv8 over sha256 of file name, size and
/// the sha256 of its first 1 MiB), so annotations keyed by uuid survive `index --rebuild`.
pub fn legacy_uuid(file_name: &str, size: u64, sha256_first_mib: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(b"cinelog-legacy-v1\0");
    h.update(file_name.as_bytes());
    h.update(b"\0");
    h.update(size.to_le_bytes());
    h.update(sha256_first_mib.as_bytes());
    let digest = h.finalize();
    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    uuid::Builder::from_custom_bytes(bytes)
        .into_uuid()
        .hyphenated()
        .to_string()
}

// ------------------------------------------------------------------ helpers

fn non_empty(s: &str) -> Option<String> {
    (!s.is_empty()).then(|| s.to_owned())
}

fn serde_text<T: Serialize>(v: &T) -> String {
    match serde_json::to_value(v) {
        Ok(serde_json::Value::String(s)) => s,
        Ok(other) => other.to_string(),
        Err(_) => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const U: &str = "01934f5e-7b2a-7c3d-8e4f-0123456789ab";

    #[test]
    fn text_enums_roundtrip() {
        for s in SessionState::ALL {
            assert_eq!(s.as_str().parse::<SessionState>().unwrap(), *s);
            assert_eq!(serde_json::to_string(s).unwrap(), format!("\"{s}\""));
        }
        for k in ChunkKind::ALL {
            assert_eq!(k.as_str().parse::<ChunkKind>().unwrap(), *k);
        }
        for k in LegacyStatus::ALL {
            assert_eq!(k.as_str().parse::<LegacyStatus>().unwrap(), *k);
        }
        assert!("bogus".parse::<SessionState>().is_err());
    }

    #[test]
    fn entry_refs() {
        for s in [U.to_owned(), format!("{U}:42"), format!("{U}@12.5")] {
            assert_eq!(s.parse::<EntryRef>().unwrap().to_string(), s);
        }
        assert_eq!(
            format!("{}:7", U.to_uppercase()).parse::<EntryRef>().unwrap(),
            EntryRef::Command { uuid: U.into(), seq: 7 }
        );
        for bad in ["", "nope", &format!("{U}:x"), &format!("{U}@-1"), &format!("{U}@nan")] {
            assert!(bad.parse::<EntryRef>().is_err(), "{bad}");
        }
    }

    #[test]
    fn cursor_roundtrip() {
        let c = Cursor {
            ts: 1_731_070_000.123456,
            kind: HitKind::Output,
            id: 99,
        };
        assert_eq!(c.to_string().parse::<Cursor>().unwrap(), c);
        assert!("1:x:2".parse::<Cursor>().is_err());
        assert!("inf:c:2".parse::<Cursor>().is_err());
    }

    #[test]
    fn exit_filter() {
        assert_eq!("ok".parse::<ExitFilter>().unwrap(), ExitFilter::Success);
        assert_eq!("fail".parse::<ExitFilter>().unwrap(), ExitFilter::Failure);
        assert_eq!("130".parse::<ExitFilter>().unwrap(), ExitFilter::Code(130));
        assert!("x".parse::<ExitFilter>().is_err());
        assert!(ExitFilter::Failure.matches(Some(1)));
        assert!(!ExitFilter::Failure.matches(None));
        assert!(ExitFilter::Success.matches(Some(0)));
    }

    #[test]
    fn short_terms() {
        assert!(SearchQuery::new("ab").is_short());
        assert!(SearchQuery::new("äö").is_short());
        assert!(!SearchQuery::new("abc").is_short());
        assert!(!SearchQuery::new("").is_short());
    }

    #[test]
    fn legacy_uuid_is_stable_and_valid() {
        let a = legacy_uuid("host_2024.cast", 10, "ab");
        assert_eq!(a, legacy_uuid("host_2024.cast", 10, "ab"));
        assert_ne!(a, legacy_uuid("host_2024.cast", 11, "ab"));
        let u = uuid::Uuid::parse_str(&a).unwrap();
        assert_eq!(u.get_version_num(), 8);
    }

    #[test]
    fn render_options_from_config() {
        let o = RenderOptions::default();
        assert_eq!(o.chunk_bytes, 16 * 1024);
        assert_eq!(o.max_output_per_command, 64 << 20);
        assert!(o.output && o.gap_output && o.alt_snapshots);
    }

    #[test]
    fn marker_rows_keep_timeline_kinds_only() {
        let at = Stamp { t: 1.0, ts: 2.0 };
        let gap = CastMarker::from_json(r#"{"cl":1,"k":"gap","dropped_bytes":5,"from_t":1,"to_t":2}"#)
            .unwrap()
            .unwrap();
        let row = MarkerRow::from_marker(&gap, at).unwrap();
        assert_eq!(row.kind, "gap");
        assert!(row.data_json.starts_with(r#"{"cl":1,"k":"gap""#));
        let unknown = CastMarker::from_json(r#"{"cl":1,"k":"future","x":1}"#)
            .unwrap()
            .unwrap();
        assert_eq!(MarkerRow::from_marker(&unknown, at).unwrap().kind, "future");
        let cmd = CastMarker::from_json(r#"{"cl":1,"k":"cmd","seq":1,"sh":5,"n":1,"typed":"ls"}"#)
            .unwrap()
            .unwrap();
        assert!(MarkerRow::from_marker(&cmd, at).is_none());
    }

    #[test]
    fn segment_policy() {
        assert!(!segment_due(0, 0.0));
        assert!(segment_due(PLAYBACK_SEGMENT_BYTES, 1.0));
        assert!(segment_due(1, PLAYBACK_SEGMENT_SECS));
    }

    #[test]
    fn db_paths() {
        let p = DbPaths::in_dir("/s/host");
        assert_eq!(p.index, Path::new("/s/host/index.db"));
        assert_eq!(p.writer_lock, Path::new("/s/host/index.db.writer.lock"));
        let d = jiff::civil::date(2026, 11, 8);
        assert_eq!(p.meta_backup(d), Path::new("/s/host/meta-20261108.jsonl.zst"));
    }
}
