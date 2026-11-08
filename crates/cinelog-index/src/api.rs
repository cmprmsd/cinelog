//! Interfaces between the M2 work packages. Frozen for M2; see the crate docs for who
//! implements which trait.

use std::io::{BufRead, Write};
use std::path::Path;

use crate::error::Result;
use crate::model::{
    Annotation, AnnotationQuery, AuthorInfo, Checkpoint, Chunk, CompressCandidate, CompressionUpdate, EntryDetail,
    ForgetOutcome, ForgetTarget, Forgotten, IndexBatch, IndexStatus, KnownCast, LegacyImportRow, OnceOptions,
    OnceSummary, OutputStats, Owner, PollBudget, SearchPage, SearchQuery, SessionState, SessionSummary, Stamp, TagInfo,
    TranscriptState,
};

/// Terminal transcript renderer (M2-A, `render::transcript`): avt 0.20 with a scrollback-0
/// observer. Rows are attributed to the current [`Owner`]; finished rows become [`Chunk`]s.
///
/// Rules every implementation follows:
/// - rows reach a chunk when they scroll off the primary screen, or at a [`Self::boundary`]
///   / [`Self::finish`]; soft-wrapped rows are joined into one logical line, trailing blanks
///   are trimmed, empty chunks are never produced;
/// - a chunk's `t`/`ts` is the first write of its first line (avt `dirty()`);
/// - a chunk ends at `chunk_bytes` or `chunk_lines`, and at every resize;
/// - while the alt screen is active, it is snapshotted (when it is left, at boundaries and
///   every `alt_snapshot_secs`), de-duplicated per owner span, as `ChunkKind::Alt`;
/// - an owner's output stops at `max_output_per_command` ([`OutputStats::truncated`]);
/// - `Owner::Gap` rows are dropped when `gap_output` is off; nothing is emitted when
///   `output` is off.
pub trait Transcript: Send {
    /// Feeds `o` event data.
    fn output(&mut self, at: Stamp, data: &str);
    fn resize(&mut self, at: Stamp, cols: u16, rows: u16);
    /// Ends the current owner's span at the cursor row and starts `next`'s span there: rows
    /// above the cursor go to the current owner, the cursor row and below to `next`. Owners
    /// may recur (output returns to an outer command after an inner one closes); their line
    /// ordinals continue.
    fn boundary(&mut self, at: Stamp, next: Owner);
    fn owner(&self) -> Owner;
    /// The logical line that ends on the row above the cursor, preceded by up to
    /// `max_lines - 1` earlier logical lines (oldest first). Used by the legacy importer to
    /// read the command line before a `Begin` banner (§11.3).
    fn lines_above_cursor(&self, max_lines: usize) -> Vec<String>;
    fn is_alt_screen(&self) -> bool;
    /// Finished chunks since the last call.
    fn take_chunks(&mut self) -> Vec<Chunk>;
    /// The current span's not yet final on-screen rows as `ChunkKind::Provisional`.
    fn provisional(&self) -> Vec<Chunk>;
    /// Closes the current span including every visible row; returns all remaining chunks.
    fn finish(&mut self, at: Stamp) -> Vec<Chunk>;
    fn output_stats(&self, owner: Owner) -> OutputStats;
    /// State to resume from with `render::restore_transcript`; resuming and feeding the
    /// rest must produce exactly the chunks a single pass produces.
    fn snapshot(&self) -> TranscriptState;
}

/// Indexing of one session's cast, resumable from a [`Checkpoint`] (M2-A for cinelog casts,
/// M2-C for legacy casts). Follows rotation parts by itself.
pub trait SessionPipeline: Send {
    fn uuid(&self) -> &str;
    /// Reads newly available events within `budget` and returns what to commit, `None` when
    /// nothing changed. Every returned batch carries a checkpoint at the last line boundary.
    fn poll(&mut self, budget: PollBudget) -> Result<Option<IndexBatch>>;
    /// Closes everything: open commands are closed at the last event (exit NULL), provisional
    /// chunks are cleared, `session.state = state`, `finished = true`.
    fn finish(&mut self, state: SessionState) -> Result<IndexBatch>;
    /// The cast is complete (`x` seen, or a legacy cast read to EOF).
    fn is_finished(&self) -> bool;
}

/// Opens [`SessionPipeline`]s (M2-A `render::cast_pipelines`, M2-C `legacy::pipelines`).
pub trait PipelineFactory: Send + Sync {
    /// `cast` is the session's first part. A checkpoint whose file stamp no longer matches
    /// (truncated or replaced file) is ignored and the first batch has `reset = true`.
    fn open(&self, cast: &Path, checkpoint: Option<Checkpoint>) -> Result<Box<dyn SessionPipeline>>;
}

/// The single writer of index.db (M2-B, `db::open_writer`). Holds the writer flock for its
/// lifetime. Each method is one transaction.
pub trait IndexWriter: Send {
    /// Applies one batch. Rows matching the forgotten list (see [`Self::apply_forgotten`])
    /// are dropped, so a rebuild never brings forgotten content back.
    fn apply(&mut self, batch: &IndexBatch) -> Result<()>;
    /// Replaces the writer's forgotten list (from meta.db) and deletes matching rows.
    fn apply_forgotten(&mut self, forgotten: &[Forgotten]) -> Result<ForgetOutcome>;
    /// Legacy files that produced no batch (failed, skipped).
    fn record_legacy(&mut self, row: &LegacyImportRow) -> Result<()>;
    fn record_compression(&mut self, update: &CompressionUpdate) -> Result<()>;
    /// `index_info`; `None` deletes the key.
    fn set_info(&mut self, key: &str, value: Option<&str>) -> Result<()>;
    /// Drops every derived row (`index --rebuild`); keeps the schema.
    fn clear(&mut self) -> Result<()>;

    fn known_casts(&self) -> Result<Vec<KnownCast>>;
    fn checkpoint(&self, uuid: &str) -> Result<Option<Checkpoint>>;
    fn legacy_entry(&self, path: &Path) -> Result<Option<LegacyImportRow>>;
    /// Finished (`ended`/`crashed`), non-legacy, uncompressed parts with `ended_at` before
    /// `ended_before`.
    fn compress_candidates(&self, ended_before: f64) -> Result<Vec<CompressCandidate>>;
}

/// Read-only queries (M2-B, `db::open_reader`). Implements the §6.5 query rules and
/// excludes `meta.forgotten` at query time.
pub trait Searcher: Send {
    fn search(&self, query: &SearchQuery) -> Result<SearchPage>;
    fn session(&self, uuid: &str) -> Result<Option<SessionSummary>>;
    /// `None` when the entry does not exist (or is forgotten).
    fn show(&self, entry: &crate::model::EntryRef, max_output_bytes: usize) -> Result<Option<EntryDetail>>;
    fn status(&self) -> Result<IndexStatus>;
    /// All `legacy_import` rows, path order (`cinelog import --report`).
    fn legacy_report(&self) -> Result<Vec<LegacyImportRow>>;
}

/// meta.db (M2-B, `db::open_meta`). Safe to open from several processes (WAL, busy timeout).
pub trait MetaStore: Send {
    /// Records a forget request; idempotent (returns the existing row on repeat).
    fn forget(&mut self, target: &ForgetTarget, by: &str) -> Result<Forgotten>;
    fn forgotten(&self) -> Result<Vec<Forgotten>>;
    fn setting(&self, key: &str) -> Result<Option<String>>;
    fn set_setting(&mut self, key: &str, value: Option<&str>) -> Result<()>;
    /// Insert or update by `uuid`; tags are created as needed and replaced.
    fn put_annotation(&mut self, annotation: &Annotation) -> Result<()>;
    /// Annotations of a session, including soft-deleted ones, by `created_at`.
    fn annotations(&self, session_uuid: &str) -> Result<Vec<Annotation>>;
    /// One annotation by uuid, soft-deleted ones included.
    fn annotation(&self, uuid: &str) -> Result<Option<Annotation>>;
    /// Annotations matching `query`, ordered by `(created_at, uuid)` (descending with
    /// `newest_first`).
    fn find_annotations(&self, query: &AnnotationQuery) -> Result<Vec<Annotation>>;
    /// Every tag, by name.
    fn tags(&self) -> Result<Vec<TagInfo>>;
    /// Every author of a non-deleted annotation, by name.
    fn authors(&self) -> Result<Vec<AuthorInfo>>;
    /// Creates the tag if needed and sets its colour (`None`: automatic). Names are stored
    /// as given (trimmed); callers normalise them.
    fn put_tag(&mut self, name: &str, color: Option<&str>) -> Result<TagInfo>;
    /// JSON Lines, one `{"table":…,"row":{…}}` per row of every table except
    /// `auth_sessions`. Returns the row count.
    fn export_jsonl(&self, out: &mut dyn Write) -> Result<u64>;
    /// Merges an export (upsert by natural key). Returns the row count.
    fn import_jsonl(&mut self, input: &mut dyn BufRead) -> Result<u64>;
}

/// One-shot discovery and backfill (M2-D, `serve::open_engine`), used by `cinelog index`.
pub trait IndexEngine {
    fn index_once(&mut self, opts: &OnceOptions) -> Result<OnceSummary>;
}
