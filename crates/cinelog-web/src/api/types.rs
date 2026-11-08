//! Wire types of the JSON API (docs/API.md). Frozen for M3 by the contracts work package:
//! additive changes only, through the contracts owner. `web/src/lib/api/types.ts` is
//! generated from this file (see [`typescript`]); never edit the TS file by hand.
//!
//! Conventions:
//! - field names are snake_case on both sides; every field is always present, absent values
//!   are `null` (query parameter structs are the exception: their fields are optional);
//! - `ts`, `ts_*` and `*_at` are UTC Unix epoch seconds (float, µs precision); `t`, `t_*`
//!   are session-relative cast seconds;
//! - integers are JSON numbers (all below 2^53);
//! - sessions are addressed by uuid and commands by `(uuid, seq)`; no file system path is
//!   ever accepted, and the only path ever returned is [`ImportFile::file`] (relative to the
//!   logs root, display only);
//! - all text (commands, output, snippets, labels) is untrusted terminal data: render it as
//!   text nodes, never as HTML.

use std::fmt::Write as _;

use cinelog_index::model::{self as im, SNIPPET_CLOSE, SNIPPET_OPEN};
use serde::{Deserialize, Serialize};
use ts_rs::TS;

/// Seconds (float).
pub type Seconds = f64;

// ------------------------------------------------------------------ enums

/// Recording scope of a server and its sessions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, TS)]
#[serde(rename_all = "lowercase")]
pub enum Scope {
    Team,
    Personal,
}

/// `sessions.state`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, TS)]
#[serde(rename_all = "lowercase")]
pub enum SessionState {
    Live,
    Ended,
    Crashed,
    Degraded,
    Error,
}

/// Kind of an output chunk.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, TS)]
#[serde(rename_all = "lowercase")]
pub enum ChunkKind {
    /// Primary-screen rows of a command.
    Primary,
    /// Alt-screen snapshot (vim, less, top, …).
    Alt,
    /// Output outside any command (prompts, leading-space commands, legacy without banners).
    Gap,
    /// On-screen rows of a command that is still running.
    Provisional,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, TS)]
#[serde(rename_all = "lowercase")]
pub enum HitKind {
    /// The command line matched (`typed`, `expanded` or `cwd`).
    Command,
    /// An output chunk matched.
    Output,
}

/// `in=` of `GET /api/search`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize, TS)]
#[serde(rename_all = "lowercase")]
pub enum SearchIn {
    /// Command lines only (default).
    #[default]
    Cmd,
    /// Output only.
    Out,
    Both,
}

/// `format=` of `GET /api/sessions/{uuid}/export`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, TS)]
#[serde(rename_all = "lowercase")]
pub enum ExportFormat {
    /// asciicast v3 slice, playable on its own.
    Cast,
    /// Rendered terminal rows as plain text.
    Txt,
    /// Self-contained, script-free HTML snippet.
    Html,
}

/// How the player loads a session (see [`PlayInfo`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, TS)]
#[serde(rename_all = "lowercase")]
pub enum PlayMode {
    /// One asciicast v3 stream (`GET /api/sessions/{uuid}/cast`).
    Cast,
    /// asciinema-player `format: 'segmented'` (`…/play/index.json`).
    Segmented,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, TS)]
#[serde(rename_all = "lowercase")]
pub enum LegacyStatus {
    Imported,
    Hidden,
    Skipped,
    Failed,
}

/// Machine-readable error class of an [`ApiError`]; the HTTP status is fixed per code.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    /// 400: malformed parameter or body, or a host outside the allowlist.
    BadRequest,
    /// 401: no, expired or unbound session cookie; wrong token.
    Unauthorized,
    /// 403: `Origin` mismatch on a mutating request.
    Forbidden,
    /// 404: unknown session, command, segment or route.
    NotFound,
    /// 405.
    MethodNotAllowed,
    /// 409: the request conflicts with the current state (for example a forget target that
    /// does not exist in the index).
    Conflict,
    /// 413: the cast is above `index.playback_segment_threshold_mb`; use segmented playback.
    PayloadTooLarge,
    /// 415: a mutating request without `Content-Type: application/json`.
    UnsupportedMediaType,
    /// 429: login rate limit (5 per minute per IP); see `Retry-After`.
    TooManyRequests,
    /// 503: the index is not available yet (first start, rebuild) or a component is missing.
    Unavailable,
    /// 500.
    Internal,
}

impl ErrorCode {
    pub const fn status(self) -> u16 {
        match self {
            Self::BadRequest => 400,
            Self::Unauthorized => 401,
            Self::Forbidden => 403,
            Self::NotFound => 404,
            Self::MethodNotAllowed => 405,
            Self::Conflict => 409,
            Self::PayloadTooLarge => 413,
            Self::UnsupportedMediaType => 415,
            Self::TooManyRequests => 429,
            Self::Unavailable => 503,
            Self::Internal => 500,
        }
    }
}

// ------------------------------------------------------------------ errors, health, auth

/// Body of every non-2xx JSON response. `message` is for humans and never contains
/// terminal content, tokens or paths.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct ApiError {
    pub error: ErrorCode,
    pub message: String,
}

/// `GET /healthz` (unauthenticated).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct Health {
    pub version: String,
}

/// `GET /api/login` (unauthenticated): what the login page shows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct LoginInfo {
    pub version: String,
    pub scope: Scope,
    /// SHA-256 of the certificate's DER, uppercase hex pairs joined by `:` (the format
    /// `cinelog install`, `doctor` and `tls show` print).
    pub fingerprint: String,
}

/// `POST /api/login` body. 204 + `Set-Cookie` on success.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct LoginRequest {
    /// The token as printed by `cinelog token new` / `install` (base32, case-insensitive,
    /// surrounding whitespace ignored).
    pub token: String,
}

/// `GET /api/me`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
pub struct Me {
    /// Token name (TEAM: the member; PERSONAL: the user). Author of annotations and forgets.
    pub principal: String,
    pub scope: Scope,
    pub host: String,
    pub version: String,
    /// Marker protocol version (1).
    pub protocol: u32,
    pub fingerprint: String,
    /// When the session ends without activity (idle TTL, capped by the absolute TTL).
    pub expires_at: Seconds,
}

// ------------------------------------------------------------------ sessions

/// tmux location of a pane recording. All `null` for shells outside tmux.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, TS)]
pub struct TmuxLocation {
    /// `<server_pid>:<server_starttime>`: one tmux server instance.
    pub server: Option<String>,
    /// `$3`.
    pub session_id: Option<String>,
    /// Session name at the last `ctx` marker.
    pub session: Option<String>,
    /// Window index.
    pub window: Option<String>,
    pub window_name: Option<String>,
    /// Pane id (`%7`).
    pub pane: Option<String>,
}

/// One recorded session (a cast, with all its rotation parts).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
pub struct Session {
    pub uuid: String,
    pub host: String,
    pub scope: Scope,
    pub user: String,
    pub euid: Option<u32>,
    pub sudo_user: Option<String>,
    pub claimed_user: Option<String>,
    /// `ancestor`, `loginuid`, `inherited`, `none` or `legacy`.
    pub attribution: Option<String>,
    pub login_via: Option<String>,
    pub tty: Option<String>,
    /// PTY master holder: `tmux: server`, `sshd-session`, `terminator`, `sudo`, …
    pub holder_comm: Option<String>,
    pub tmux: TmuxLocation,
    /// Outer session of a nested recording (sudo inside a user shell).
    pub parent_uuid: Option<String>,
    pub parts: u32,
    pub compressed: bool,
    pub cast_bytes: Option<u64>,
    pub started_at: Seconds,
    pub ended_at: Option<Seconds>,
    /// Cast length in seconds as far as indexed (`null` before the first index pass).
    pub duration: Option<Seconds>,
    pub exit_status: Option<i32>,
    /// IANA zone of the recording host at start.
    pub tz: Option<String>,
    pub cols: u16,
    pub rows: u16,
    pub term: Option<String>,
    pub legacy: bool,
    pub hidden: bool,
    pub state: SessionState,
    pub commands: u64,
}

/// `GET /api/sessions`: newest `started_at` first.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
pub struct SessionPage {
    pub sessions: Vec<Session>,
    /// Pass as `cursor=` for the next page; `null` on the last page.
    pub next: Option<String>,
}

/// Short reference to a related session (parent, children).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
pub struct SessionRef {
    pub uuid: String,
    pub user: String,
    pub scope: Scope,
    pub state: SessionState,
    pub started_at: Seconds,
    pub ended_at: Option<Seconds>,
    pub holder_comm: Option<String>,
}

/// A recorder marker: `pause`, `resume`, `nested`, `multiplexer`, `ctx`, `gap`, `rotate`,
/// `error`, `clock`, … (docs/PROTOCOL.md §5.1). Unknown kinds may appear; ignore them.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
pub struct Marker {
    pub t: Seconds,
    pub ts: Seconds,
    pub kind: String,
    /// The marker's JSON fields (never contains the nonce).
    #[ts(type = "Record<string, unknown>")]
    pub data: serde_json::Value,
}

/// One rotation part of a cast.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
pub struct CastPart {
    pub part: u32,
    pub compressed: bool,
    pub bytes: Option<u64>,
    /// Session time at which this part starts.
    pub t_offset: Seconds,
}

/// `GET /api/sessions/{uuid}`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
pub struct SessionDetail {
    pub session: Session,
    pub parent: Option<SessionRef>,
    /// Nested sessions started from this one, oldest first.
    pub children: Vec<SessionRef>,
    /// Oldest first.
    pub markers: Vec<Marker>,
    pub parts: Vec<CastPart>,
    /// The `meta.forgotten` rows of this session.
    pub forgotten: Vec<Forgotten>,
    /// What playback and export mask, by start: forgotten ranges, and each forgotten
    /// command from the end of the command before it to its own end. Known after the
    /// command has left index.db, so clients draw these rather than deriving them.
    pub masked: Vec<MaskedSpan>,
    /// How [`PlayInfo`] will load this session.
    pub play_mode: PlayMode,
}

/// One command (chapter) of a session.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
pub struct Command {
    pub seq: u64,
    pub parent_seq: Option<u64>,
    /// Nesting depth (zsh inside zsh); 0 at the top.
    pub depth: u32,
    pub t_start: Seconds,
    /// `null` while running, or when the shell never reported it.
    pub t_end: Option<Seconds>,
    pub ts_start: Seconds,
    pub ts_end: Option<Seconds>,
    pub exit: Option<i32>,
    pub pipestatus: Vec<i32>,
    pub cwd: Option<String>,
    pub user: Option<String>,
    /// The command as typed (`$1` of preexec).
    pub typed: String,
    /// The expanded command (`$3`), when it differs.
    pub expanded: Option<String>,
    /// `typed` was cut at 16 KiB.
    pub typed_trunc: bool,
    /// Boundaries reconstructed by the legacy importer.
    pub approx: bool,
    pub confidence: Option<f64>,
    pub out_bytes: u64,
    pub out_truncated: bool,
}

/// `GET /api/sessions/{uuid}/commands`: all commands, `t_start` order.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
pub struct CommandList {
    pub session: String,
    pub commands: Vec<Command>,
}

/// `GET /api/show`: one entry with its output text, as `cinelog show` prints it (also the
/// preview of `search --team`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
pub struct Entry {
    pub session: Session,
    /// The addressed command, or the command running at `@<t>`; `null` for gap output and
    /// session entries.
    pub command: Option<Command>,
    /// Rendered output text (rows joined by `\n`), forgotten ranges removed.
    pub output: String,
    /// More output exists than `max_bytes`.
    pub output_truncated: bool,
}

// ------------------------------------------------------------------ tree

/// `GET /api/tree`: host › tmux server › tmux session › window › pane › sessions, plus the
/// non-tmux shells grouped by `holder_comm`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
pub struct Tree {
    /// By host name.
    pub hosts: Vec<TreeHost>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
pub struct TreeHost {
    pub host: String,
    /// Most recent activity first.
    pub tmux_servers: Vec<TreeTmuxServer>,
    /// Non-tmux shells by `holder_comm` (`null` group last: unknown holder, legacy casts).
    pub shells: Vec<TreeShellGroup>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
pub struct TreeTmuxServer {
    /// `<server_pid>:<server_starttime>`.
    pub server: String,
    pub sessions: Vec<TreeTmuxSession>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
pub struct TreeTmuxSession {
    pub session_id: Option<String>,
    pub name: Option<String>,
    /// By window index.
    pub windows: Vec<TreeWindow>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
pub struct TreeWindow {
    pub window: Option<String>,
    pub name: Option<String>,
    pub panes: Vec<TreePane>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
pub struct TreePane {
    pub pane: Option<String>,
    /// Newest first.
    pub sessions: Vec<TreeNode>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
pub struct TreeShellGroup {
    pub holder_comm: Option<String>,
    /// Newest first.
    pub sessions: Vec<TreeNode>,
}

/// A session in the tree. Sessions whose parent is in the same response are listed only
/// under that parent's `children`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
pub struct TreeNode {
    pub uuid: String,
    pub user: String,
    pub state: SessionState,
    pub started_at: Seconds,
    pub ended_at: Option<Seconds>,
    pub legacy: bool,
    pub hidden: bool,
    pub commands: u64,
    /// Oldest first.
    pub children: Vec<TreeNode>,
}

// ------------------------------------------------------------------ search

/// One piece of a highlighted snippet. Render `text` as a text node, inside `<mark>` when
/// `hit`. Concatenating all `text`s gives the snippet.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct SnippetPart {
    pub text: String,
    pub hit: bool,
}

/// One search result.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
pub struct SearchHit {
    /// Stable key within one result list: `c:<commands.id>` or `o:<outputs.id>`.
    pub id: String,
    pub kind: HitKind,
    /// Session uuid.
    pub session: String,
    /// The matched command, or the command owning the matched output; `null` for gap output.
    pub cmd_seq: Option<u64>,
    /// Seek position: the command start, or the output chunk's first write.
    pub t: Seconds,
    pub ts: Seconds,
    pub user: String,
    pub host: String,
    pub pane: Option<String>,
    pub tmux_session: Option<String>,
    pub exit: Option<i32>,
    pub cwd: Option<String>,
    pub typed: Option<String>,
    /// Command hits: `typed` with the matches marked. Output hits: text around the first
    /// match. Never empty.
    pub snippet: Vec<SnippetPart>,
    pub chunk_kind: Option<ChunkKind>,
    pub approx: bool,
    pub confidence: Option<f64>,
    pub legacy: bool,
}

/// `GET /api/search`: newest `ts` first.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
pub struct SearchResponse {
    pub hits: Vec<SearchHit>,
    pub next: Option<String>,
    /// Terms under 3 characters run a 500 ms newest-first scan; `true` when it stopped
    /// early (older matches may exist).
    pub partial: bool,
    /// More than 5000 candidates: refine the query.
    pub capped: bool,
}

// ------------------------------------------------------------------ status and import

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, TS)]
pub struct SessionCounts {
    pub live: u64,
    pub ended: u64,
    pub crashed: u64,
    pub degraded: u64,
    pub error: u64,
    pub legacy: u64,
    pub hidden: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
pub struct LiveLag {
    pub session: String,
    pub indexed_bytes: u64,
    pub file_bytes: Option<u64>,
    pub updated_at: Option<Seconds>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, TS)]
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

/// `GET /api/status`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
pub struct Status {
    pub version: String,
    pub protocol: u32,
    pub scope: Scope,
    pub host: String,
    pub server_started_at: Seconds,
    pub port: u16,
    pub fingerprint: String,
    pub schema_version: u32,
    pub index_bytes: u64,
    pub sessions: SessionCounts,
    pub commands: u64,
    pub outputs: u64,
    pub live: Vec<LiveLag>,
    pub backlog_bytes: u64,
    pub legacy: LegacyProgress,
    /// Last indexer error (one line, no cast content).
    pub last_error: Option<String>,
}

/// One legacy file of `GET /api/import`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
pub struct ImportFile {
    /// Relative to the scope's logs root. Display only; the API never accepts paths.
    pub file: String,
    pub status: LegacyStatus,
    pub session: Option<String>,
    pub banners: u64,
    pub log_lines: u64,
    pub aligned: u64,
    pub mean_conf: Option<f64>,
    pub error: Option<String>,
}

/// `GET /api/import`: path order.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
pub struct ImportReport {
    pub progress: LegacyProgress,
    pub files: Vec<ImportFile>,
}

// ------------------------------------------------------------------ forget

/// `POST /api/forget` body. Exactly one form: `session_uuid` alone (the whole session),
/// with `cmd_seq` (one command), or with both `t_from` and `t_to` (a range, inclusive).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(optional_fields)]
pub struct ForgetRequest {
    pub session_uuid: String,
    #[serde(default)]
    pub cmd_seq: Option<u64>,
    #[serde(default)]
    pub t_from: Option<Seconds>,
    #[serde(default)]
    pub t_to: Option<Seconds>,
}

/// What was forgotten.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum ForgetTarget {
    Session {
        uuid: String,
    },
    Command {
        uuid: String,
        seq: u64,
    },
    Range {
        uuid: String,
        t_from: Seconds,
        t_to: Seconds,
    },
}

/// A `meta.forgotten` row; also the `POST /api/forget` response (200; repeating a forget
/// returns the existing row).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
pub struct Forgotten {
    pub target: ForgetTarget,
    /// Principal who forgot it.
    pub by: String,
    pub at: Seconds,
}

/// A masked span of a session (session seconds).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
pub struct MaskedSpan {
    pub t_from: Seconds,
    /// `null`: a forgotten command that has not ended (masked to the end).
    pub t_to: Option<Seconds>,
    /// Principal who forgot it.
    pub by: String,
}

// ------------------------------------------------------------------ playback

/// `GET /api/sessions/{uuid}/play`: how to create the player. Polling it starts the
/// on-demand segment build of a large cast that has no segments yet.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
pub struct PlayInfo {
    pub mode: PlayMode,
    /// Player source URL: `/api/sessions/{uuid}/cast` or `/api/sessions/{uuid}/play/index.json`.
    pub url: String,
    /// `false` while segments are being built; poll again after about 500 ms.
    pub ready: bool,
    /// Build progress 0…1 while `!ready`.
    pub progress: Option<f64>,
    pub duration: Option<Seconds>,
    pub cols: u16,
    pub rows: u16,
}

/// Terminal of a segmented recording. `theme` is the cast header's theme, if any.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct PlayTerm {
    pub cols: u16,
    pub rows: u16,
    pub theme: Option<PlayTheme>,
}

/// asciicast v3 `term.theme`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct PlayTheme {
    pub fg: String,
    pub bg: String,
    /// 8 or 16 colours joined by `:`.
    pub palette: String,
}

/// `GET /api/sessions/{uuid}/play/index.json`: asciinema-player 3.18 segmented index
/// (validation rules in docs/API.md).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
pub struct PlayIndex {
    /// Always 1.
    pub version: u32,
    pub duration: Seconds,
    pub term: PlayTerm,
    /// Chapters `[t, "$ <typed>"]`, ascending.
    pub markers: Vec<(Seconds, String)>,
    pub segments: Vec<PlaySegmentRef>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
pub struct PlaySegmentRef {
    pub start: Seconds,
    /// Relative to the index URL: `seg/<n>`.
    pub url: String,
}

/// Terminal state at a segment's start: `init` is an avt dump fed before its events.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct PlaySnapshot {
    pub cols: u16,
    pub rows: u16,
    pub init: String,
}

/// `GET /api/sessions/{uuid}/play/seg/{n}`. Events are `[abs_t, code, data]` with code
/// `o`, `i`, `r` (`"COLSxROWS"`) or `m` (chapter label).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
pub struct PlaySegment {
    pub snapshot: PlaySnapshot,
    pub events: Vec<(Seconds, String, String)>,
}

// ------------------------------------------------------------------ query parameters

/// Query of `GET /api/search`. Empty or missing `q` lists the newest commands.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, TS)]
#[ts(optional_fields)]
#[serde(default)]
pub struct SearchParams {
    pub q: Option<String>,
    #[serde(rename = "in")]
    pub target: Option<SearchIn>,
    /// Case-sensitive (default: Unicode case-insensitive).
    pub case: Option<bool>,
    pub user: Option<String>,
    pub host: Option<String>,
    /// `ts` lower bound, inclusive.
    pub from: Option<Seconds>,
    /// `ts` upper bound, inclusive.
    pub to: Option<Seconds>,
    /// A number, `ok` (0) or `fail` (non-zero).
    pub exit: Option<String>,
    /// Exact command cwd.
    pub cwd: Option<String>,
    /// tmux pane id (`%3`).
    pub pane: Option<String>,
    /// Only this session (uuid).
    pub session: Option<String>,
    /// Include hidden (`TERM=dumb` legacy) sessions.
    pub hidden: Option<bool>,
    /// 1…500, default 50.
    pub limit: Option<u32>,
    pub cursor: Option<String>,
}

/// Query of `GET /api/show`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(optional_fields)]
pub struct ShowParams {
    /// `<uuid>`, `<uuid>:<seq>` or `<uuid>@<t>` (the `EntryRef` forms of `cinelog show`).
    pub entry: String,
    /// Output bytes, default 65536, at most 1048576.
    #[serde(default)]
    pub max_bytes: Option<u32>,
}

/// Query of `GET /api/sessions`.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, TS)]
#[ts(optional_fields)]
#[serde(default)]
pub struct SessionParams {
    /// `true`: only `state = live`; `false`: only finished sessions.
    pub live: Option<bool>,
    pub host: Option<String>,
    pub user: Option<String>,
    /// `started_at` bounds, inclusive.
    pub from: Option<Seconds>,
    pub to: Option<Seconds>,
    pub pane: Option<String>,
    pub legacy: Option<bool>,
    pub hidden: Option<bool>,
    /// 1…500, default 100.
    pub limit: Option<u32>,
    pub cursor: Option<String>,
}

/// Query of `GET /api/tree`.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, TS)]
#[ts(optional_fields)]
#[serde(default)]
pub struct TreeParams {
    /// Sessions active in `[from, to]` (started before `to`, not ended before `from`).
    pub from: Option<Seconds>,
    pub to: Option<Seconds>,
    pub live: Option<bool>,
    pub hidden: Option<bool>,
}

/// Query of `GET /api/sessions/{uuid}/export`. `cmd` selects one command; otherwise
/// `from`/`to` (session seconds, default the whole session).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(optional_fields)]
pub struct ExportParams {
    pub format: ExportFormat,
    #[serde(default)]
    pub from: Option<Seconds>,
    #[serde(default)]
    pub to: Option<Seconds>,
    #[serde(default)]
    pub cmd: Option<u64>,
}

// ------------------------------------------------------------------ conversions

impl From<cinelog_core::Scope> for Scope {
    fn from(s: cinelog_core::Scope) -> Self {
        match s {
            cinelog_core::Scope::Team => Self::Team,
            cinelog_core::Scope::Personal => Self::Personal,
        }
    }
}

impl From<im::SessionState> for SessionState {
    fn from(s: im::SessionState) -> Self {
        match s {
            im::SessionState::Live => Self::Live,
            im::SessionState::Ended => Self::Ended,
            im::SessionState::Crashed => Self::Crashed,
            im::SessionState::Degraded => Self::Degraded,
            im::SessionState::Error => Self::Error,
        }
    }
}

impl From<im::ChunkKind> for ChunkKind {
    fn from(k: im::ChunkKind) -> Self {
        match k {
            im::ChunkKind::Primary => Self::Primary,
            im::ChunkKind::Alt => Self::Alt,
            im::ChunkKind::Gap => Self::Gap,
            im::ChunkKind::Provisional => Self::Provisional,
        }
    }
}

impl From<im::HitKind> for HitKind {
    fn from(k: im::HitKind) -> Self {
        match k {
            im::HitKind::Command => Self::Command,
            im::HitKind::Output => Self::Output,
        }
    }
}

impl From<im::LegacyStatus> for LegacyStatus {
    fn from(s: im::LegacyStatus) -> Self {
        match s {
            im::LegacyStatus::Imported => Self::Imported,
            im::LegacyStatus::Hidden => Self::Hidden,
            im::LegacyStatus::Skipped => Self::Skipped,
            im::LegacyStatus::Failed => Self::Failed,
        }
    }
}

impl From<SearchIn> for im::SearchTarget {
    fn from(s: SearchIn) -> Self {
        match s {
            SearchIn::Cmd => Self::Commands,
            SearchIn::Out => Self::Output,
            SearchIn::Both => Self::All,
        }
    }
}

impl From<&im::TmuxLocation> for TmuxLocation {
    fn from(t: &im::TmuxLocation) -> Self {
        Self {
            server: t.server.clone(),
            session_id: t.session_id.clone(),
            session: t.session.clone(),
            window: t.window.clone(),
            window_name: t.window_name.clone(),
            pane: t.pane.clone(),
        }
    }
}

impl From<&im::Command> for Command {
    fn from(c: &im::Command) -> Self {
        Self {
            seq: c.seq,
            parent_seq: c.parent_seq,
            depth: c.depth,
            t_start: c.t_start,
            t_end: c.t_end,
            ts_start: c.ts_start,
            ts_end: c.ts_end,
            exit: c.exit,
            pipestatus: c.pipestatus.clone(),
            cwd: c.cwd.clone(),
            user: c.user.clone(),
            typed: c.typed.clone(),
            expanded: c.expanded.clone().filter(|e| *e != c.typed),
            typed_trunc: c.typed_trunc,
            approx: c.approx,
            confidence: c.confidence,
            out_bytes: c.out_bytes,
            out_truncated: c.out_truncated,
        }
    }
}

impl From<&im::ForgetTarget> for ForgetTarget {
    fn from(t: &im::ForgetTarget) -> Self {
        match t {
            im::ForgetTarget::Session { uuid } => Self::Session { uuid: uuid.clone() },
            im::ForgetTarget::Command { uuid, seq } => Self::Command {
                uuid: uuid.clone(),
                seq: *seq,
            },
            im::ForgetTarget::Range { uuid, t_from, t_to } => Self::Range {
                uuid: uuid.clone(),
                t_from: *t_from,
                t_to: *t_to,
            },
        }
    }
}

impl From<&im::Forgotten> for Forgotten {
    fn from(f: &im::Forgotten) -> Self {
        Self {
            target: (&f.target).into(),
            by: f.by.clone(),
            at: f.at,
        }
    }
}

impl From<im::SessionCounts> for SessionCounts {
    fn from(c: im::SessionCounts) -> Self {
        Self {
            live: c.live,
            ended: c.ended,
            crashed: c.crashed,
            degraded: c.degraded,
            error: c.error,
            legacy: c.legacy,
            hidden: c.hidden,
        }
    }
}

impl From<im::LegacyProgress> for LegacyProgress {
    fn from(p: im::LegacyProgress) -> Self {
        Self {
            files: p.files,
            imported: p.imported,
            hidden: p.hidden,
            skipped: p.skipped,
            failed: p.failed,
            banners: p.banners,
            log_lines: p.log_lines,
            aligned: p.aligned,
        }
    }
}

/// Validation error of a [`ForgetRequest`] (400).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ForgetRequestError {
    #[error("session_uuid is not a uuid")]
    BadUuid,
    #[error("give either cmd_seq or both t_from and t_to, not both")]
    Ambiguous,
    #[error("t_from and t_to come together")]
    HalfRange,
    #[error("t_from and t_to must be finite, non-negative and ordered")]
    BadRange,
}

impl ForgetRequest {
    /// The index target this request names (uuid normalised to lowercase hyphenated).
    pub fn target(&self) -> Result<im::ForgetTarget, ForgetRequestError> {
        let uuid = uuid::Uuid::try_parse(self.session_uuid.trim())
            .map_err(|_| ForgetRequestError::BadUuid)?
            .hyphenated()
            .to_string();
        match (self.cmd_seq, self.t_from, self.t_to) {
            (None, None, None) => Ok(im::ForgetTarget::Session { uuid }),
            (Some(seq), None, None) => Ok(im::ForgetTarget::Command { uuid, seq }),
            (None, Some(t_from), Some(t_to)) => {
                if t_from.is_finite() && t_to.is_finite() && 0.0 <= t_from && t_from <= t_to {
                    Ok(im::ForgetTarget::Range { uuid, t_from, t_to })
                } else {
                    Err(ForgetRequestError::BadRange)
                }
            }
            (Some(_), _, _) => Err(ForgetRequestError::Ambiguous),
            _ => Err(ForgetRequestError::HalfRange),
        }
    }
}

/// Splits a snippet with the U+E000/U+E001 sentinels (§6.5) into parts. Adjacent parts of
/// the same kind are merged, empty parts dropped; stray or nested sentinels never leak into
/// the text. An unclosed hit runs to the end.
pub fn split_snippet(s: &str) -> Vec<SnippetPart> {
    fn push(parts: &mut Vec<SnippetPart>, text: &mut String, hit: bool) {
        if text.is_empty() {
            return;
        }
        match parts.last_mut() {
            Some(last) if last.hit == hit => last.text.push_str(text),
            _ => parts.push(SnippetPart {
                text: text.clone(),
                hit,
            }),
        }
        text.clear();
    }
    let mut parts = Vec::new();
    let mut text = String::new();
    let mut hit = false;
    for c in s.chars() {
        if c == SNIPPET_OPEN || c == SNIPPET_CLOSE {
            push(&mut parts, &mut text, hit);
            hit = c == SNIPPET_OPEN;
        } else {
            text.push(c);
        }
    }
    push(&mut parts, &mut text, hit);
    parts
}

// ------------------------------------------------------------------ TypeScript

/// Relative to the workspace root.
pub const TS_FILE: &str = "web/src/lib/api/types.ts";

/// The generated TypeScript module: every wire type, declaration order as in this file.
pub fn typescript() -> String {
    let cfg = ts_rs::Config::new().with_large_int("number");
    let decls = [
        Scope::decl(&cfg),
        SessionState::decl(&cfg),
        ChunkKind::decl(&cfg),
        HitKind::decl(&cfg),
        SearchIn::decl(&cfg),
        ExportFormat::decl(&cfg),
        PlayMode::decl(&cfg),
        LegacyStatus::decl(&cfg),
        ErrorCode::decl(&cfg),
        ApiError::decl(&cfg),
        Health::decl(&cfg),
        LoginInfo::decl(&cfg),
        LoginRequest::decl(&cfg),
        Me::decl(&cfg),
        TmuxLocation::decl(&cfg),
        Session::decl(&cfg),
        SessionPage::decl(&cfg),
        SessionRef::decl(&cfg),
        Marker::decl(&cfg),
        CastPart::decl(&cfg),
        SessionDetail::decl(&cfg),
        Command::decl(&cfg),
        CommandList::decl(&cfg),
        Entry::decl(&cfg),
        Tree::decl(&cfg),
        TreeHost::decl(&cfg),
        TreeTmuxServer::decl(&cfg),
        TreeTmuxSession::decl(&cfg),
        TreeWindow::decl(&cfg),
        TreePane::decl(&cfg),
        TreeShellGroup::decl(&cfg),
        TreeNode::decl(&cfg),
        SnippetPart::decl(&cfg),
        SearchHit::decl(&cfg),
        SearchResponse::decl(&cfg),
        SessionCounts::decl(&cfg),
        LiveLag::decl(&cfg),
        LegacyProgress::decl(&cfg),
        Status::decl(&cfg),
        ImportFile::decl(&cfg),
        ImportReport::decl(&cfg),
        ForgetRequest::decl(&cfg),
        ForgetTarget::decl(&cfg),
        Forgotten::decl(&cfg),
        MaskedSpan::decl(&cfg),
        PlayInfo::decl(&cfg),
        PlayTerm::decl(&cfg),
        PlayTheme::decl(&cfg),
        PlayIndex::decl(&cfg),
        PlaySegmentRef::decl(&cfg),
        PlaySnapshot::decl(&cfg),
        PlaySegment::decl(&cfg),
        SearchParams::decl(&cfg),
        ShowParams::decl(&cfg),
        SessionParams::decl(&cfg),
        TreeParams::decl(&cfg),
        ExportParams::decl(&cfg),
    ];
    let mut out = String::from(
        "// Generated from crates/cinelog-web/src/api/types.rs by ts-rs. Do not edit.\n\
         // Regenerate: CINELOG_WRITE_TS=1 cargo test -p cinelog-web --lib api::types\n\
",
    );
    for d in decls {
        let _ = write!(out, "\n{}\n", export_decl(&d));
    }
    out
}

/// `decl()` yields `type X = …;` (with leading JSDoc); the module exports every type.
fn export_decl(decl: &str) -> String {
    let decl = decl.trim();
    match decl.rfind("\ntype ") {
        Some(i) => format!("{}\nexport {}", &decl[..i], &decl[i + 1..]),
        None => match decl.strip_prefix("type ") {
            Some(rest) => format!("export type {rest}"),
            None => decl.to_owned(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::path::PathBuf;

    fn ts_path() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..").join(TS_FILE)
    }

    /// The committed TS file matches the Rust types. `CINELOG_WRITE_TS=1` regenerates it.
    #[test]
    fn typescript_is_current() {
        let ts = typescript();
        let path = ts_path();
        if std::env::var_os("CINELOG_WRITE_TS").is_some() {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, &ts).unwrap();
            return;
        }
        let committed = std::fs::read_to_string(&path).unwrap_or_default();
        assert!(
            committed == ts,
            "{} is stale: run CINELOG_WRITE_TS=1 cargo test -p cinelog-web --lib api::types",
            TS_FILE
        );
    }

    #[test]
    fn typescript_shape() {
        let ts = typescript();
        assert!(!ts.contains("bigint"), "large ints map to number");
        assert!(ts.contains("export type Scope = \"team\" | \"personal\";"));
        assert!(ts.contains("export type SearchParams = {"));
        assert!(ts.contains("in?: SearchIn"), "{ts}");
        assert!(ts.contains("markers: Array<[number, string]>"), "{ts}");
        assert!(ts.contains("cmd_seq?: number"), "{ts}");
        assert!(ts.contains("data: Record<string, unknown>"));
        assert!(!ts.contains("\ntype "), "every declaration is exported");
    }

    #[test]
    fn wire_names() {
        assert_eq!(
            serde_json::to_value(ErrorCode::TooManyRequests).unwrap(),
            json!("too_many_requests")
        );
        assert_eq!(serde_json::to_value(SearchIn::Both).unwrap(), json!("both"));
        assert_eq!(
            serde_json::to_value(ForgetTarget::Command {
                uuid: "u".into(),
                seq: 3
            })
            .unwrap(),
            json!({"kind": "command", "uuid": "u", "seq": 3})
        );
        let p: SearchParams = serde_json::from_value(json!({"q": "ls", "in": "out", "case": true})).unwrap();
        assert_eq!(p.target, Some(SearchIn::Out));
        assert_eq!(p.case, Some(true));
        let idx = PlayIndex {
            version: 1,
            duration: 2.0,
            term: PlayTerm {
                cols: 80,
                rows: 24,
                theme: None,
            },
            markers: vec![(0.5, "$ ls".into())],
            segments: vec![PlaySegmentRef {
                start: 0.0,
                url: "seg/0".into(),
            }],
        };
        assert_eq!(
            serde_json::to_value(&idx).unwrap(),
            json!({"version": 1, "duration": 2.0, "term": {"cols": 80, "rows": 24, "theme": null},
                   "markers": [[0.5, "$ ls"]], "segments": [{"start": 0.0, "url": "seg/0"}]})
        );
    }

    #[test]
    fn error_status() {
        assert_eq!(ErrorCode::Unauthorized.status(), 401);
        assert_eq!(ErrorCode::Unavailable.status(), 503);
    }

    #[test]
    fn forget_request_forms() {
        let u = "0193A1B2-0000-7000-8000-000000000001";
        let req = |cmd_seq, t_from, t_to| ForgetRequest {
            session_uuid: u.into(),
            cmd_seq,
            t_from,
            t_to,
        };
        let lower = u.to_lowercase();
        assert_eq!(
            req(None, None, None).target(),
            Ok(im::ForgetTarget::Session { uuid: lower.clone() })
        );
        assert_eq!(
            req(Some(4), None, None).target(),
            Ok(im::ForgetTarget::Command {
                uuid: lower.clone(),
                seq: 4
            })
        );
        assert_eq!(
            req(None, Some(1.0), Some(2.0)).target(),
            Ok(im::ForgetTarget::Range {
                uuid: lower,
                t_from: 1.0,
                t_to: 2.0
            })
        );
        assert_eq!(
            req(Some(1), Some(1.0), Some(2.0)).target(),
            Err(ForgetRequestError::Ambiguous)
        );
        assert_eq!(req(None, Some(1.0), None).target(), Err(ForgetRequestError::HalfRange));
        assert_eq!(
            req(None, Some(3.0), Some(2.0)).target(),
            Err(ForgetRequestError::BadRange)
        );
        assert_eq!(
            req(None, Some(f64::NAN), Some(2.0)).target(),
            Err(ForgetRequestError::BadRange)
        );
        let bad = ForgetRequest {
            session_uuid: "../etc/passwd".into(),
            ..req(None, None, None)
        };
        assert_eq!(bad.target(), Err(ForgetRequestError::BadUuid));
    }

    #[test]
    fn snippets() {
        let p = |text: &str, hit| SnippetPart { text: text.into(), hit };
        assert_eq!(split_snippet(""), vec![]);
        assert_eq!(split_snippet("plain"), vec![p("plain", false)]);
        assert_eq!(
            split_snippet("a\u{E000}needle\u{E001}b<img onerror=x>"),
            vec![p("a", false), p("needle", true), p("b<img onerror=x>", false)]
        );
        assert_eq!(split_snippet("\u{E000}x\u{E001}\u{E000}y\u{E001}"), vec![p("xy", true)]);
        assert_eq!(split_snippet("a\u{E001}b\u{E000}c"), vec![p("ab", false), p("c", true)]);
        for parts in [
            split_snippet("\u{E000}\u{E000}a\u{E001}\u{E001}"),
            split_snippet("\u{E001}"),
        ] {
            assert!(parts.iter().all(|s| !s.text.contains(['\u{E000}', '\u{E001}'])));
        }
    }

    #[test]
    fn command_conversion_drops_identical_expansion() {
        let mut c = im::Command {
            seq: 1,
            parent_seq: None,
            depth: 0,
            shell_pid: None,
            t_start: 0.5,
            t_end: Some(1.0),
            ts_start: 10.5,
            ts_end: Some(11.0),
            exit: Some(0),
            pipestatus: vec![0],
            cwd: Some("/".into()),
            user: None,
            euid: None,
            typed: "ls".into(),
            expanded: Some("ls".into()),
            typed_trunc: false,
            approx: false,
            confidence: None,
            out_bytes: 3,
            out_truncated: false,
        };
        assert_eq!(Command::from(&c).expanded, None);
        c.expanded = Some("ls --color=tty".into());
        assert_eq!(Command::from(&c).expanded.as_deref(), Some("ls --color=tty"));
    }
}
