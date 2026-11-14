//! Wire types of the JSON API (docs/API.md). Frozen for M3 and extended for M4 (live, UI
//! bus, timeline, annotations) by the contracts work packages: additive changes only,
//! through the contracts owner. `web/src/lib/api/types.ts` is
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

use std::collections::BTreeMap;
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
    /// 403: `Origin` mismatch on a mutating request, or a change to another principal's
    /// annotation that only its author may make.
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
    /// Index of the pane in its window, as tmux shows it (`null` when no `ctx` marker
    /// reported one, or from a server before 1.1).
    #[serde(default)]
    pub pane_index: Option<String>,
}

/// The tmux session a shell's tmux client attached to (the follow-up `multiplexer` marker,
/// PROTOCOL.md §4.4); its panes are recorded on their own.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, TS)]
pub struct TmuxAttach {
    /// [`TmuxLocation::server`] of the pane recordings of that server.
    pub server: String,
    /// `$3`.
    pub session_id: String,
    /// Session name when the client attached.
    pub session: Option<String>,
}

impl TmuxAttach {
    /// From the payload of a `multiplexer` marker; `None` without a usable `tmux` object.
    pub fn of_marker(data: &serde_json::Map<String, serde_json::Value>) -> Option<Self> {
        let t = data.get("tmux")?.as_object()?;
        let text = |k: &str| t.get(k)?.as_str().filter(|s| !s.is_empty()).map(str::to_owned);
        let pid = t.get("server_pid")?.as_u64().filter(|&p| p > 0)?;
        Some(Self {
            server: format!(
                "{pid}:{}",
                t.get("server_start").and_then(serde_json::Value::as_u64).unwrap_or(0)
            ),
            session_id: text("session_id")?,
            session: text("session_name"),
        })
    }
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
    /// Start of its oldest session in the response (tells servers of one host apart).
    pub first_at: Seconds,
    /// A pane recording of it is live.
    pub live: bool,
    pub sessions: Vec<TreeTmuxSession>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
pub struct TreeTmuxSession {
    pub session_id: Option<String>,
    pub name: Option<String>,
    /// A pane recording of it is live.
    pub live: bool,
    /// Sessions in its panes, nested ones included.
    pub recordings: u32,
    /// Shells in the response whose tmux client attached to it, newest first; they are
    /// also listed where they belong (their own pane or shell group).
    pub attached_from: Vec<TreeAttachedShell>,
    /// By window index.
    pub windows: Vec<TreeWindow>,
}

/// A shell that ran the tmux client of a [`TreeTmuxSession`] (paused meanwhile).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
pub struct TreeAttachedShell {
    pub uuid: String,
    pub user: String,
    pub state: SessionState,
    pub started_at: Seconds,
    pub holder_comm: Option<String>,
    pub tty: Option<String>,
    /// Set when that shell is itself a tmux pane (tmux inside tmux).
    pub tmux: TmuxLocation,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
pub struct TreeWindow {
    pub window: Option<String>,
    pub name: Option<String>,
    pub panes: Vec<TreePane>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
pub struct TreePane {
    /// Pane id (`%7`).
    pub pane: Option<String>,
    /// Its index in the window, from the newest session that knows it.
    pub index: Option<String>,
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
    /// Whose `sudo` it is (TEAM root shells); labels a nested child.
    pub sudo_user: Option<String>,
    /// tmux sessions its tmux client attached to, oldest first, each once.
    pub attached: Vec<TmuxAttach>,
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
    /// Tags of the non-deleted annotations on the hit's command (for output hits: the
    /// owning command), sorted and without duplicates; empty for gap output. Span
    /// annotations do not count. Absent from servers before the annotations overview.
    #[serde(default)]
    pub tags: Vec<String>,
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

// ------------------------------------------------------------------ live (M4)

/// First message of every `GET /api/sessions/{uuid}/live` connection: the asciinema-player
/// `eventsource` driver's reset, carrying the terminal at join time.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
pub struct LiveInit {
    pub cols: u16,
    pub rows: u16,
    /// Session seconds at the join (the last event read); the player clock starts here and
    /// every following event carries absolute session seconds.
    pub time: Seconds,
    /// avt dump of the terminal at `time`, forgotten ranges masked.
    pub init: String,
}

/// One live event `[t, code, data]`: absolute session seconds, code `o`, `i`, `r`
/// (`"COLSxROWS"`) or `m` (`$ <typed>` chapter label), rewritten and masked as `/cast`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
pub struct LiveEvent(pub Seconds, pub String, pub String);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, TS)]
#[serde(rename_all = "lowercase")]
pub enum LiveStreamState {
    Offline,
}

/// The recorder died without ending its cast (session `crashed`). Event `done` follows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct LiveOffline {
    pub state: LiveStreamState,
}

/// `data` of an unnamed (`message`) event of the live stream. The stream ends with an event
/// named `done` (empty data).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(untagged)]
pub enum LiveMessage {
    Init(LiveInit),
    Event(LiveEvent),
    Offline(LiveOffline),
}

// ------------------------------------------------------------------ UI bus (M4)

/// What a live session's recorder is doing (its registry entry).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, TS)]
#[serde(rename_all = "lowercase")]
pub enum RecStatus {
    Recording,
    /// A multiplexer client (tmux), a nested recording or `cinelog pause` holds the pause.
    Paused,
    /// The cast was closed after an I/O error; passthrough only until `cinelog resume`.
    Degraded,
}

/// A session being recorded now (`state = live`) with its recorder status.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
pub struct LiveSession {
    pub session: Session,
    pub status: RecStatus,
    /// `paused`: `multiplexer`, `nested` or `user`; `degraded`: the error. `null` while
    /// recording.
    pub reason: Option<String>,
    /// What holds the pause: the multiplexer client's comm (`tmux: client`), the nested
    /// session's uuid, or the `cinelog pause --reason` text.
    pub detail: Option<String>,
    /// ts the current status began, when known.
    pub since: Option<Seconds>,
    /// Paused by a nested recording: the child's scope, from the cast's `nested` marker for
    /// `detail` (`null` otherwise, or while that marker is not found). A child of the other
    /// scope (a TEAM `sudo -i` in a PERSONAL shell) is served by the other scope's server.
    pub child_scope: Option<Scope>,
    /// With `child_scope`: the child's session URL as its recorder reported it (`url` of the
    /// marker; only when that server ran).
    pub child_url: Option<String>,
}

/// Bus `hello`: sent first on every (re)connection; a full resync of the live sessions.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
pub struct BusHello {
    pub version: String,
    pub server_started_at: Seconds,
    /// Newest `started_at` first.
    pub live: Vec<LiveSession>,
}

/// Bus `session-end`: a live session left `live`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
pub struct SessionEnd {
    pub session: String,
    /// `ended`, `crashed` (recorder died: live viewers show offline), `degraded` or `error`.
    pub state: SessionState,
    pub ended_at: Option<Seconds>,
}

/// Bus `index-progress`: the indexer view of `Status`, at most every 2 s while it changes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
pub struct IndexProgress {
    pub backlog_bytes: u64,
    pub commands: u64,
    pub outputs: u64,
    pub live: Vec<LiveLag>,
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, TS)]
#[serde(rename_all = "lowercase")]
pub enum ChangeOp {
    Created,
    Updated,
    /// Soft-deleted: `deleted_at` is set.
    Deleted,
}

/// Bus `annotation`: an annotation was created, changed or deleted (by anyone).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
pub struct AnnotationEvent {
    pub op: ChangeOp,
    pub annotation: Annotation,
}

/// One event of `GET /api/events`. On the wire the SSE event name is `type` and the SSE
/// `data` is the JSON of `data` ([`Self::name`], [`Self::data_json`]); clients rebuild this
/// object from both.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(tag = "type", content = "data", rename_all = "kebab-case")]
pub enum BusEvent {
    Hello(BusHello),
    SessionStart(LiveSession),
    /// Recorder status or session metadata of a live session changed.
    SessionUpdate(LiveSession),
    SessionEnd(SessionEnd),
    IndexProgress(IndexProgress),
    /// Legacy import counters, at most every 2 s while an import runs.
    ImportProgress(LegacyProgress),
    Annotation(AnnotationEvent),
    /// A tag was created or its colour changed (`count` as of the change).
    Tag(Tag),
    /// Something was forgotten (`POST /api/forget` or `cinelog forget`): drop it from
    /// lists, reload playback of that session.
    Forgotten(Forgotten),
}

impl BusEvent {
    /// Every SSE event name, in declaration order.
    pub const NAMES: [&'static str; 9] = [
        "hello",
        "session-start",
        "session-update",
        "session-end",
        "index-progress",
        "import-progress",
        "annotation",
        "tag",
        "forgotten",
    ];

    /// The SSE event name.
    pub fn name(&self) -> &'static str {
        let i = match self {
            Self::Hello(_) => 0,
            Self::SessionStart(_) => 1,
            Self::SessionUpdate(_) => 2,
            Self::SessionEnd(_) => 3,
            Self::IndexProgress(_) => 4,
            Self::ImportProgress(_) => 5,
            Self::Annotation(_) => 6,
            Self::Tag(_) => 7,
            Self::Forgotten(_) => 8,
        };
        Self::NAMES[i]
    }

    /// The SSE `data` line: the JSON of the payload (one line).
    pub fn data_json(&self) -> String {
        let r = match self {
            Self::Hello(d) => serde_json::to_string(d),
            Self::SessionStart(d) | Self::SessionUpdate(d) => serde_json::to_string(d),
            Self::SessionEnd(d) => serde_json::to_string(d),
            Self::IndexProgress(d) => serde_json::to_string(d),
            Self::ImportProgress(d) => serde_json::to_string(d),
            Self::Annotation(d) => serde_json::to_string(d),
            Self::Tag(d) => serde_json::to_string(d),
            Self::Forgotten(d) => serde_json::to_string(d),
        };
        r.unwrap_or_else(|_| "null".to_owned())
    }
}

// ------------------------------------------------------------------ timeline (M4)

/// `group=` of `GET /api/timeline`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize, TS)]
#[serde(rename_all = "lowercase")]
pub enum TimelineGroup {
    /// Lanes are people, rows their panes and shells (default).
    #[default]
    User,
    /// Lanes are panes and shells, rows the people in them.
    Pane,
}

/// `GET /api/timeline`: command blocks of every session active in `[from, to]`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
pub struct Timeline {
    pub from: Seconds,
    pub to: Seconds,
    pub group: TimelineGroup,
    /// By label.
    pub lanes: Vec<TimelineLane>,
    /// Spans of at least 10 min inside `[from, to]` in which no block and no mark is drawn
    /// in any row (drawn collapsed, as a break mark). Pause, gap and forgotten spans count as
    /// busy (an open one until the session end, or now while live); skip and nested points
    /// count with 60 s on either side. Ascending.
    pub idle: Vec<TimeSpan>,
    /// Annotations of the sessions in the window, by `ts_from`.
    pub annotations: Vec<TimelineAnnotation>,
    /// Commands in the window (before aggregation).
    pub commands: u64,
    /// More than 20 000 commands: rows carry `density` instead of `blocks`.
    pub aggregated: bool,
    /// Width of a density bin when `aggregated`.
    pub bin_secs: Option<Seconds>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
pub struct TimelineLane {
    /// `user:<name>` (group `user`) or the row key of the pane (group `pane`).
    pub key: String,
    /// The person, or `<tmux session> › <pane label>` (the shell's label outside tmux).
    pub label: String,
    /// Group `user`: by [`TimelineRow::group`] (groups by first activity in the window, the
    /// panes of a tmux session by window and pane index). Group `pane`: by first activity.
    /// A nested row follows its parent.
    pub rows: Vec<TimelineRow>,
}

/// One drawn row: a pane (or non-tmux shell) of one person, or the sessions nested in such
/// a row that run as one user.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
pub struct TimelineRow {
    /// Unique within the response. Pane: `pane:<host>/<tmux server>/<pane>`; shell outside
    /// tmux: `tty:<host>/<tty>` (or `session:<uuid>` without a tty); with group `pane` the
    /// lane's row per person is `<pane key>|user:<name>`.
    pub key: String,
    /// `<window> <window name> · <pane index>` or `<holder_comm> <tty>`; a nested row
    /// `<user>` or `<user> (sudo)`; with group `pane` the person.
    pub label: String,
    pub host: String,
    /// The person: `sudo_user`, else `claimed_user`, else `user`.
    pub user: String,
    pub tmux: TmuxLocation,
    pub holder_comm: Option<String>,
    pub tty: Option<String>,
    /// The sub-group of the lane this row belongs to (group `user`): its tmux session, or
    /// the shells outside tmux of its host. Rows of one group follow each other. `null`
    /// with group `pane`.
    pub group: Option<TimelineRowGroup>,
    /// Key of the row this one is nested in (a `sudo` shell inside it); such a row directly
    /// follows its parent and the parent's other nested rows.
    pub parent: Option<String>,
    /// Nesting depth: 0, or the parent's plus one.
    pub depth: u32,
    /// Sessions drawn in this row, oldest first.
    pub sessions: Vec<TimelineSession>,
    /// By `ts_start`; empty when the response is `aggregated`.
    pub blocks: Vec<TimelineBlock>,
    /// Pauses, skips, nested starts, gaps and forgotten spans (always sent), by `ts`.
    pub marks: Vec<TimelineMark>,
    /// Only when `aggregated`: bins with at least one command, ascending.
    pub density: Vec<DensityBin>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, TS)]
#[serde(rename_all = "lowercase")]
pub enum TimelineRowGroupKind {
    /// The panes of one tmux session.
    Tmux,
    /// The shells outside tmux of one host.
    Shells,
}

/// See [`TimelineRow::group`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct TimelineRowGroup {
    /// `tmux:<host>/<tmux server>/<session id>` or `shells:<host>`.
    pub key: String,
    pub kind: TimelineRowGroupKind,
    /// The tmux session name (latest; else its id), or `shells`.
    pub label: String,
    pub host: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
pub struct TimelineSession {
    pub uuid: String,
    pub user: String,
    pub state: SessionState,
    pub started_at: Seconds,
    /// The drawn end. A non-live session that never closed gets its indexed end
    /// (`started_at + duration`, mapped through its `clock` markers), so open commands and
    /// open spans end where the server's idle breaks assume. `null` while live (draw to now).
    pub ended_at: Option<Seconds>,
    pub parent_uuid: Option<String>,
    pub legacy: bool,
}

/// One command. Click: open `/s/<session>?cmd=<seq>`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
pub struct TimelineBlock {
    pub session: String,
    pub seq: u64,
    pub t_start: Seconds,
    pub ts_start: Seconds,
    /// `null`: still running (draw to now), or never closed (draw to the session end).
    pub ts_end: Option<Seconds>,
    pub exit: Option<i32>,
    /// First line of `typed`, at most 200 characters (hover text).
    pub typed: String,
    pub cwd: Option<String>,
    /// Legacy boundaries (drawn hatched).
    pub approx: bool,
    pub depth: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, TS)]
#[serde(rename_all = "lowercase")]
pub enum TimelineMarkKind {
    /// Recording paused (span; `reason` `multiplexer`, `nested` or `user`).
    Pause,
    /// A leading-space command ran unrecorded (point).
    Skip,
    /// A nested recording started (point; `child` is its session).
    Nested,
    /// Output was dropped (disk full or stalled; span).
    Gap,
    /// Masked by a forget (span; `detail` is who forgot it).
    Forgotten,
}

/// A visible "not recorded here" marker (plan §15.2: every pause or skip shows).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
pub struct TimelineMark {
    pub session: String,
    pub kind: TimelineMarkKind,
    pub ts: Seconds,
    /// End of a span; `null` for points and for a span still open.
    pub ts_end: Option<Seconds>,
    pub reason: Option<String>,
    pub detail: Option<String>,
    /// `nested`: the child session.
    pub child: Option<String>,
    /// `nested`: the child's scope. A child of the other scope (a TEAM `sudo -i` in a
    /// PERSONAL shell) is served by the other scope's server, not this one.
    pub child_scope: Option<Scope>,
    /// `nested`: the child's session URL as its recorder reported it (`url` of the marker;
    /// only when that server ran).
    pub child_url: Option<String>,
    /// `pause` with reason `multiplexer`: the tmux session the client was attached to, when
    /// the recorder found it. Its panes are the rows of the group
    /// `tmux:<host>/<server>/<session_id>`.
    pub tmux: Option<TmuxAttach>,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, TS)]
pub struct DensityBin {
    /// Bin start; the bin covers `[ts, ts + bin_secs)`.
    pub ts: Seconds,
    pub commands: u32,
    /// Commands with a non-zero exit.
    pub failed: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, TS)]
pub struct TimeSpan {
    pub from: Seconds,
    pub to: Seconds,
}

/// An annotation placed on the timeline (wall time; a command annotation spans its command).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
pub struct TimelineAnnotation {
    pub uuid: String,
    pub session: String,
    /// [`TimelineRow::key`] of the session's row.
    pub row: String,
    pub cmd_seq: Option<u64>,
    pub ts_from: Seconds,
    pub ts_to: Seconds,
    pub tags: Vec<String>,
    /// Colour of the first tag that has one.
    pub color: Option<String>,
    pub author: String,
}

// ------------------------------------------------------------------ annotations and tags (M4)

/// A shared note (meta.db). Exactly one of `cmd_seq` or `t_from`/`t_to` is set.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
pub struct Annotation {
    pub uuid: String,
    pub session: String,
    pub cmd_seq: Option<u64>,
    /// Session seconds of a span annotation.
    pub t_from: Option<Seconds>,
    pub t_to: Option<Seconds>,
    /// Principal who wrote it; only the author may edit the body or delete it.
    pub author: String,
    /// Plain text; clients render it as text, with `http(s)://` URLs as links.
    pub body: String,
    /// Sorted names.
    pub tags: Vec<String>,
    pub created_at: Seconds,
    pub updated_at: Seconds,
    pub deleted_at: Option<Seconds>,
}

/// `GET /api/annotations`: oldest `created_at` first (`order=desc`: newest first).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
pub struct AnnotationPage {
    pub annotations: Vec<Annotation>,
    pub next: Option<String>,
    /// With `context=true`: where each annotation of the page sits, by annotation uuid. An
    /// annotation whose session index.db does not have (yet) has no entry. Empty without
    /// `context=true`. Absent from servers before the annotations overview.
    #[serde(default)]
    pub contexts: BTreeMap<String, AnnotationContext>,
}

/// Where an annotation sits: its session and its command or span, for lists outside the
/// session view (the annotations overview).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
pub struct AnnotationContext {
    /// The session's `user` and `host` (as `Session`).
    pub user: String,
    pub host: String,
    /// tmux pane id (`%7`); `null` outside tmux.
    pub pane: Option<String>,
    /// tmux session name.
    pub tmux_session: Option<String>,
    pub started_at: Seconds,
    /// Command annotations: the command; `null` for span annotations and for a command
    /// index.db does not have.
    pub command: Option<AnnotationCommand>,
    /// Wall time of the target: the command's `ts_start`…`ts_end`, or the span's
    /// `t_from`…`t_to` mapped like `TimelineAnnotation` (`started_at + t`, corrected by
    /// `clock` markers). `ts_to` is `null` for a command that is running or never closed.
    pub ts_from: Seconds,
    pub ts_to: Option<Seconds>,
}

/// The annotated command, in an [`AnnotationContext`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
pub struct AnnotationCommand {
    /// First line of `typed`, cut to 200 characters (as `TimelineBlock.typed`).
    pub typed: String,
    pub exit: Option<i32>,
    pub cwd: Option<String>,
}

/// `POST /api/annotations` body: `session_uuid` with `cmd_seq`, or with `t_from` and
/// `t_to` (session seconds, `t_from ≤ t_to`). A body or at least one tag is required.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(optional_fields)]
pub struct AnnotationCreate {
    pub session_uuid: String,
    #[serde(default)]
    pub cmd_seq: Option<u64>,
    #[serde(default)]
    pub t_from: Option<Seconds>,
    #[serde(default)]
    pub t_to: Option<Seconds>,
    #[serde(default)]
    pub body: Option<String>,
    #[serde(default)]
    pub tags: Option<Vec<String>>,
}

/// `PATCH /api/annotations/{uuid}` body; at least one field. `body` and `tags` (replace
/// all) are for the author only; `add_tags` is open to every principal.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, TS)]
#[ts(optional_fields)]
#[serde(default)]
pub struct AnnotationPatch {
    pub body: Option<String>,
    pub tags: Option<Vec<String>>,
    pub add_tags: Option<Vec<String>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct Tag {
    pub name: String,
    /// `#rrggbb`; `null`: the client derives a colour from the name.
    pub color: Option<String>,
    /// Annotations (not deleted) carrying it.
    pub count: u64,
}

/// `GET /api/tags`: by name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct TagList {
    pub tags: Vec<Tag>,
}

/// One author of annotations.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct Author {
    pub name: String,
    /// Annotations (not deleted, not gone with a forgotten session or command) they wrote.
    pub count: u64,
}

/// `GET /api/annotations/authors`: by name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct AuthorList {
    pub authors: Vec<Author>,
}

/// `POST /api/tags` body: creates the tag or sets its colour (absent or `null`: automatic).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(optional_fields)]
pub struct TagRequest {
    pub name: String,
    #[serde(default)]
    pub color: Option<String>,
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
    /// Command cwd: this directory and below (a trailing `/` is ignored).
    pub cwd: Option<String>,
    /// tmux pane id (`%3`).
    pub pane: Option<String>,
    /// Only this session (uuid).
    pub session: Option<String>,
    /// Include hidden (`TERM=dumb` legacy) sessions.
    pub hidden: Option<bool>,
    /// Comma-separated tag names (at most 16): only hits that one non-deleted annotation
    /// carrying all of them is attached to (docs/API.md §5.4).
    pub tag: Option<String>,
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

/// Query of `GET /api/timeline`. Default window: the last 24 h; at most 31 days.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, TS)]
#[ts(optional_fields)]
#[serde(default)]
pub struct TimelineParams {
    /// ts bounds; commands overlapping `[from, to]` are included.
    pub from: Option<Seconds>,
    pub to: Option<Seconds>,
    pub group: Option<TimelineGroup>,
    /// Only this person (`sudo_user`, else `claimed_user`, else `user`).
    pub user: Option<String>,
    pub host: Option<String>,
    /// Include hidden (`TERM=dumb` legacy) sessions.
    pub hidden: Option<bool>,
}

/// `order=` of `GET /api/annotations`, by `(created_at, uuid)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize, TS)]
#[serde(rename_all = "lowercase")]
pub enum SortOrder {
    /// Oldest first (default).
    #[default]
    Asc,
    /// Newest first.
    Desc,
}

/// Query of `GET /api/annotations`. All filters combine with AND.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, TS)]
#[ts(optional_fields)]
#[serde(default)]
pub struct AnnotationParams {
    /// Session uuid.
    pub session: Option<String>,
    /// With `session`: one command's annotations.
    pub cmd_seq: Option<u64>,
    /// Comma-separated tag names (at most 16): annotations carrying all of them.
    pub tag: Option<String>,
    /// Exact principal name.
    pub author: Option<String>,
    /// Only this annotation (uuid).
    pub uuid: Option<String>,
    /// Body contains this text, case-insensitively (at most 200 characters; blank: no
    /// filter).
    pub q: Option<String>,
    /// `created_at` lower bound, inclusive.
    pub from: Option<Seconds>,
    /// `created_at` upper bound, inclusive.
    pub to: Option<Seconds>,
    /// Default `asc` (oldest first).
    pub order: Option<SortOrder>,
    /// Fill [`AnnotationPage::contexts`].
    pub context: Option<bool>,
    /// Include soft-deleted annotations.
    pub deleted: Option<bool>,
    /// 1…500, default 200.
    pub limit: Option<u32>,
    pub cursor: Option<String>,
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
            pane_index: t.pane_index.clone(),
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

impl From<cinelog_core::RecState> for RecStatus {
    fn from(s: cinelog_core::RecState) -> Self {
        match s {
            cinelog_core::RecState::Live => Self::Recording,
            cinelog_core::RecState::Paused => Self::Paused,
            cinelog_core::RecState::Degraded => Self::Degraded,
        }
    }
}

impl From<&im::Annotation> for Annotation {
    fn from(a: &im::Annotation) -> Self {
        let mut tags = a.tags.clone();
        tags.sort();
        tags.dedup();
        Self {
            uuid: a.uuid.clone(),
            session: a.session_uuid.clone(),
            cmd_seq: a.cmd_seq,
            t_from: a.t_from,
            t_to: a.t_to,
            author: a.author.clone(),
            body: a.body.clone(),
            tags,
            created_at: a.created_at,
            updated_at: a.updated_at,
            deleted_at: a.deleted_at,
        }
    }
}

impl From<im::TagInfo> for Tag {
    fn from(t: im::TagInfo) -> Self {
        Self {
            name: t.name,
            color: t.color,
            count: t.count,
        }
    }
}

/// The registry's `state_reason` (`multiplexer: tmux: client`, `user: lunch`, …) split into
/// [`LiveSession::reason`] and [`LiveSession::detail`] at the first `": "`.
pub fn split_state_reason(state_reason: Option<&str>) -> (Option<String>, Option<String>) {
    match state_reason.map(str::trim).filter(|r| !r.is_empty()) {
        None => (None, None),
        Some(r) => match r.split_once(": ") {
            Some((reason, detail)) => (
                Some(reason.to_owned()),
                Some(detail.to_owned()).filter(|d| !d.is_empty()),
            ),
            None => (Some(r.to_owned()), None),
        },
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

// ------------------------------------------------------------------ annotation validation

/// Longest annotation body in bytes.
pub const MAX_ANNOTATION_BODY: usize = 16 * 1024;
/// Most tags on one annotation.
pub const MAX_TAGS: usize = 16;
/// Longest tag name in characters.
pub const MAX_TAG_CHARS: usize = 40;

/// Validation error of an annotation or tag request (400).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AnnotationRequestError {
    #[error("session_uuid is not a uuid")]
    BadUuid,
    #[error("give either cmd_seq or both t_from and t_to")]
    NoTarget,
    #[error("give either cmd_seq or both t_from and t_to, not both")]
    Ambiguous,
    #[error("t_from and t_to come together")]
    HalfRange,
    #[error("t_from and t_to must be finite, non-negative and ordered")]
    BadRange,
    #[error("an annotation needs a body or a tag")]
    Empty,
    #[error("the body is longer than {MAX_ANNOTATION_BODY} bytes")]
    BodyTooLong,
    #[error("at most {MAX_TAGS} tags")]
    TooManyTags,
    #[error("tag names are 1 to {MAX_TAG_CHARS} letters, digits or - _ . : / + # @")]
    BadTag,
    #[error("colours are #rgb or #rrggbb")]
    BadColor,
    #[error("nothing to change")]
    EmptyPatch,
}

/// A tag name as stored: trimmed, lowercase, 1 to [`MAX_TAG_CHARS`] characters, each a
/// letter, a digit or one of `-_.:/+#@`.
pub fn normalize_tag(name: &str) -> Result<String, AnnotationRequestError> {
    let n = name.trim().to_lowercase();
    let len = n.chars().count();
    let ok = (1..=MAX_TAG_CHARS).contains(&len) && n.chars().all(|c| c.is_alphanumeric() || "-_.:/+#@".contains(c));
    if ok { Ok(n) } else { Err(AnnotationRequestError::BadTag) }
}

/// A tag colour as stored: `#rrggbb`, lowercase (`#rgb` is expanded).
pub fn normalize_color(color: &str) -> Result<String, AnnotationRequestError> {
    let hex = color
        .trim()
        .strip_prefix('#')
        .filter(|h| h.chars().all(|c| c.is_ascii_hexdigit()))
        .ok_or(AnnotationRequestError::BadColor)?
        .to_ascii_lowercase();
    match hex.len() {
        6 => Ok(format!("#{hex}")),
        3 => Ok(hex.chars().fold(String::from("#"), |mut out, c| {
            out.push(c);
            out.push(c);
            out
        })),
        _ => Err(AnnotationRequestError::BadColor),
    }
}

/// A `tag=` filter: comma-separated names, each normalised ([`normalize_tag`]), sorted and
/// without duplicates; empty items are skipped. At most [`MAX_TAGS`] different names.
pub fn parse_tag_list(list: &str) -> Result<Vec<String>, AnnotationRequestError> {
    let names: Vec<String> = list
        .split(',')
        .filter(|t| !t.trim().is_empty())
        .map(str::to_owned)
        .collect();
    normalize_tags(&names)
}

/// Normalised, sorted and de-duplicated tag names (at most [`MAX_TAGS`] different ones).
pub fn normalize_tags(tags: &[String]) -> Result<Vec<String>, AnnotationRequestError> {
    let mut out = tags.iter().map(|t| normalize_tag(t)).collect::<Result<Vec<_>, _>>()?;
    out.sort();
    out.dedup();
    if out.len() > MAX_TAGS {
        return Err(AnnotationRequestError::TooManyTags);
    }
    Ok(out)
}

fn normalize_body(body: &str) -> Result<String, AnnotationRequestError> {
    let body = body.trim_end();
    if body.len() > MAX_ANNOTATION_BODY {
        return Err(AnnotationRequestError::BodyTooLong);
    }
    Ok(body.to_owned())
}

/// A validated [`AnnotationCreate`].
#[derive(Debug, Clone, PartialEq)]
pub struct NewAnnotation {
    /// Lowercase hyphenated.
    pub session_uuid: String,
    pub cmd_seq: Option<u64>,
    pub t_from: Option<Seconds>,
    pub t_to: Option<Seconds>,
    /// Trailing whitespace removed.
    pub body: String,
    /// See [`normalize_tags`].
    pub tags: Vec<String>,
}

impl NewAnnotation {
    /// The meta.db row for `author`, created at `now` (`uuid` a fresh UUIDv7).
    pub fn into_row(self, uuid: String, author: &str, now: Seconds) -> im::Annotation {
        im::Annotation {
            uuid,
            session_uuid: self.session_uuid,
            cmd_seq: self.cmd_seq,
            t_from: self.t_from,
            t_to: self.t_to,
            author: author.to_owned(),
            body: self.body,
            tags: self.tags,
            created_at: now,
            updated_at: now,
            deleted_at: None,
        }
    }
}

impl AnnotationCreate {
    pub fn validate(&self) -> Result<NewAnnotation, AnnotationRequestError> {
        let session_uuid = uuid::Uuid::try_parse(self.session_uuid.trim())
            .map_err(|_| AnnotationRequestError::BadUuid)?
            .hyphenated()
            .to_string();
        match (self.cmd_seq, self.t_from, self.t_to) {
            (Some(_), None, None) => {}
            (None, Some(a), Some(b)) => {
                if !(a.is_finite() && b.is_finite() && 0.0 <= a && a <= b) {
                    return Err(AnnotationRequestError::BadRange);
                }
            }
            (None, None, None) => return Err(AnnotationRequestError::NoTarget),
            (Some(_), _, _) => return Err(AnnotationRequestError::Ambiguous),
            _ => return Err(AnnotationRequestError::HalfRange),
        }
        let body = normalize_body(self.body.as_deref().unwrap_or_default())?;
        let tags = normalize_tags(self.tags.as_deref().unwrap_or_default())?;
        if body.is_empty() && tags.is_empty() {
            return Err(AnnotationRequestError::Empty);
        }
        Ok(NewAnnotation {
            session_uuid,
            cmd_seq: self.cmd_seq,
            t_from: self.t_from,
            t_to: self.t_to,
            body,
            tags,
        })
    }
}

/// A validated [`AnnotationPatch`].
#[derive(Debug, Clone, PartialEq, Default)]
pub struct AnnotationChange {
    pub body: Option<String>,
    pub tags: Option<Vec<String>>,
    pub add_tags: Vec<String>,
}

impl AnnotationChange {
    /// Changes only the author may make (`body`, `tags`).
    pub fn needs_author(&self) -> bool {
        self.body.is_some() || self.tags.is_some()
    }

    /// Applies the change to `row` at `now`. The result still needs a body or a tag, and
    /// at most [`MAX_TAGS`] tags.
    pub fn apply(&self, row: &mut im::Annotation, now: Seconds) -> Result<(), AnnotationRequestError> {
        let mut tags = self.tags.clone().unwrap_or_else(|| row.tags.clone());
        tags.extend(self.add_tags.iter().cloned());
        tags.sort();
        tags.dedup();
        if tags.len() > MAX_TAGS {
            return Err(AnnotationRequestError::TooManyTags);
        }
        let body = self.body.clone().unwrap_or_else(|| row.body.clone());
        if body.is_empty() && tags.is_empty() {
            return Err(AnnotationRequestError::Empty);
        }
        row.body = body;
        row.tags = tags;
        row.updated_at = now;
        Ok(())
    }
}

impl AnnotationPatch {
    pub fn validate(&self) -> Result<AnnotationChange, AnnotationRequestError> {
        if self.body.is_none() && self.tags.is_none() && self.add_tags.as_ref().is_none_or(Vec::is_empty) {
            return Err(AnnotationRequestError::EmptyPatch);
        }
        Ok(AnnotationChange {
            body: self.body.as_deref().map(normalize_body).transpose()?,
            tags: self.tags.as_deref().map(normalize_tags).transpose()?,
            add_tags: normalize_tags(self.add_tags.as_deref().unwrap_or_default())?,
        })
    }
}

impl TagRequest {
    /// The normalised name and colour.
    pub fn validate(&self) -> Result<(String, Option<String>), AnnotationRequestError> {
        Ok((
            normalize_tag(&self.name)?,
            self.color.as_deref().map(normalize_color).transpose()?,
        ))
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
        TmuxAttach::decl(&cfg),
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
        TreeAttachedShell::decl(&cfg),
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
        LiveInit::decl(&cfg),
        LiveEvent::decl(&cfg),
        LiveStreamState::decl(&cfg),
        LiveOffline::decl(&cfg),
        LiveMessage::decl(&cfg),
        RecStatus::decl(&cfg),
        LiveSession::decl(&cfg),
        BusHello::decl(&cfg),
        SessionEnd::decl(&cfg),
        IndexProgress::decl(&cfg),
        ChangeOp::decl(&cfg),
        AnnotationEvent::decl(&cfg),
        BusEvent::decl(&cfg),
        TimelineGroup::decl(&cfg),
        TimelineRowGroupKind::decl(&cfg),
        TimelineRowGroup::decl(&cfg),
        Timeline::decl(&cfg),
        TimelineLane::decl(&cfg),
        TimelineRow::decl(&cfg),
        TimelineSession::decl(&cfg),
        TimelineBlock::decl(&cfg),
        TimelineMarkKind::decl(&cfg),
        TimelineMark::decl(&cfg),
        DensityBin::decl(&cfg),
        TimeSpan::decl(&cfg),
        TimelineAnnotation::decl(&cfg),
        Annotation::decl(&cfg),
        AnnotationPage::decl(&cfg),
        AnnotationContext::decl(&cfg),
        AnnotationCommand::decl(&cfg),
        AnnotationCreate::decl(&cfg),
        AnnotationPatch::decl(&cfg),
        Tag::decl(&cfg),
        TagList::decl(&cfg),
        Author::decl(&cfg),
        AuthorList::decl(&cfg),
        TagRequest::decl(&cfg),
        TimelineParams::decl(&cfg),
        SortOrder::decl(&cfg),
        AnnotationParams::decl(&cfg),
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
    fn m4_wire_shapes() {
        let ts = typescript();
        assert!(ts.contains("export type LiveEvent = [number, string, string];"), "{ts}");
        assert!(
            ts.contains("export type LiveMessage = LiveInit | LiveEvent | LiveOffline;"),
            "{ts}"
        );
        assert!(
            ts.contains("\"type\": \"session-start\", \"data\": LiveSession"),
            "{ts}"
        );
        assert!(ts.contains("add_tags?: Array<string>"), "{ts}");

        assert_eq!(
            serde_json::to_value(LiveMessage::Event(LiveEvent(1.5, "o".into(), "x".into()))).unwrap(),
            json!([1.5, "o", "x"])
        );
        assert_eq!(
            serde_json::to_value(LiveMessage::Offline(LiveOffline {
                state: LiveStreamState::Offline
            }))
            .unwrap(),
            json!({"state": "offline"})
        );
        let init = LiveMessage::Init(LiveInit {
            cols: 80,
            rows: 24,
            time: 3.0,
            init: "\u{1b}[H".into(),
        });
        assert_eq!(
            serde_json::to_value(&init).unwrap(),
            json!({"cols": 80, "rows": 24, "time": 3.0, "init": "\u{1b}[H"})
        );
        let back: LiveMessage = serde_json::from_value(json!([2.0, "r", "100x30"])).unwrap();
        assert_eq!(back, LiveMessage::Event(LiveEvent(2.0, "r".into(), "100x30".into())));

        let end = BusEvent::SessionEnd(SessionEnd {
            session: "u".into(),
            state: SessionState::Crashed,
            ended_at: None,
        });
        assert_eq!(end.name(), "session-end");
        assert_eq!(end.data_json(), r#"{"session":"u","state":"crashed","ended_at":null}"#);
        let v = serde_json::to_value(&end).unwrap();
        assert_eq!(v["type"], json!(end.name()));
        assert_eq!(
            v["data"],
            serde_json::from_str::<serde_json::Value>(&end.data_json()).unwrap()
        );
        // NAMES matches the serde names of every variant.
        let samples = [
            BusEvent::Hello(BusHello {
                version: "v".into(),
                server_started_at: 0.0,
                live: vec![],
            }),
            BusEvent::ImportProgress(LegacyProgress::default()),
            BusEvent::Tag(Tag {
                name: "ops".into(),
                color: None,
                count: 0,
            }),
            end,
        ];
        for e in samples {
            assert_eq!(serde_json::to_value(&e).unwrap()["type"], json!(e.name()));
        }
        for (i, n) in BusEvent::NAMES.iter().enumerate() {
            assert!(ts.contains(&format!("\"type\": \"{n}\"")), "{n}");
            assert!(!BusEvent::NAMES[..i].contains(n));
        }
        assert_eq!(
            serde_json::to_value(TimelineMarkKind::Forgotten).unwrap(),
            json!("forgotten")
        );
        assert_eq!(RecStatus::from(cinelog_core::RecState::Live), RecStatus::Recording);
        let p: TimelineParams = serde_json::from_value(json!({"group": "pane", "from": 1.0})).unwrap();
        assert_eq!(p.group, Some(TimelineGroup::Pane));
    }

    #[test]
    fn state_reasons() {
        let s = |r| split_state_reason(r);
        assert_eq!(s(None), (None, None));
        assert_eq!(s(Some(" ")), (None, None));
        assert_eq!(s(Some("nested")), (Some("nested".into()), None));
        assert_eq!(
            s(Some("multiplexer: tmux: client")),
            (Some("multiplexer".into()), Some("tmux: client".into()))
        );
        assert_eq!(s(Some("user: lunch")), (Some("user".into()), Some("lunch".into())));
    }

    #[test]
    fn tags_and_colors() {
        use AnnotationRequestError as E;
        assert_eq!(normalize_tag("  Incident-42 ").as_deref(), Ok("incident-42"));
        assert_eq!(
            normalize_tag("ops/db:prod+1#@x_y.z").as_deref(),
            Ok("ops/db:prod+1#@x_y.z")
        );
        assert_eq!(normalize_tag("Ünïcode").as_deref(), Ok("ünïcode"));
        for bad in [
            "",
            "  ",
            "two words",
            "a,b",
            "<b>",
            "x\u{0}",
            &"x".repeat(MAX_TAG_CHARS + 1),
        ] {
            assert_eq!(normalize_tag(bad), Err(E::BadTag), "{bad:?}");
        }
        assert_eq!(normalize_color("#FF8800").as_deref(), Ok("#ff8800"));
        assert_eq!(normalize_color(" #f80").as_deref(), Ok("#ff8800"));
        for bad in ["ff8800", "#ff88", "#gg0000", "red", "#ff8800;x", ""] {
            assert_eq!(normalize_color(bad), Err(E::BadColor), "{bad:?}");
        }
        let many: Vec<String> = (0..=MAX_TAGS).map(|i| format!("t{i}")).collect();
        assert_eq!(normalize_tags(&many), Err(E::TooManyTags));
        assert_eq!(
            normalize_tags(&["b".into(), "A".into(), "a".into()]),
            Ok(vec!["a".into(), "b".into()])
        );
        assert_eq!(parse_tag_list(" Ops,,db , ops,"), Ok(vec!["db".into(), "ops".into()]));
        assert_eq!(parse_tag_list(""), Ok(vec![]));
        assert_eq!(parse_tag_list("ops,two words"), Err(E::BadTag));
        assert_eq!(parse_tag_list(&many.join(",")), Err(E::TooManyTags));
        let t = TagRequest {
            name: "Ops".into(),
            color: Some("#ABC".into()),
        };
        assert_eq!(t.validate(), Ok(("ops".into(), Some("#aabbcc".into()))));
    }

    #[test]
    fn annotation_requests() {
        use AnnotationRequestError as E;
        let u = "0193A1B2-0000-7000-8000-000000000001";
        let req = |cmd_seq, t_from, t_to| AnnotationCreate {
            session_uuid: u.into(),
            cmd_seq,
            t_from,
            t_to,
            body: Some("see https://x.example/a  \n".into()),
            tags: Some(vec!["Ops".into()]),
        };
        let ok = req(Some(3), None, None).validate().unwrap();
        assert_eq!(ok.session_uuid, u.to_lowercase());
        assert_eq!(ok.body, "see https://x.example/a");
        assert_eq!(ok.tags, ["ops"]);
        assert!(req(None, Some(1.0), Some(1.0)).validate().is_ok(), "a point span");
        assert_eq!(req(None, None, None).validate(), Err(E::NoTarget));
        assert_eq!(req(Some(1), Some(1.0), Some(2.0)).validate(), Err(E::Ambiguous));
        assert_eq!(req(None, Some(1.0), None).validate(), Err(E::HalfRange));
        assert_eq!(req(None, Some(2.0), Some(1.0)).validate(), Err(E::BadRange));
        assert_eq!(req(None, Some(-1.0), Some(1.0)).validate(), Err(E::BadRange));
        assert_eq!(req(None, Some(f64::INFINITY), Some(1.0)).validate(), Err(E::BadRange));
        let empty = AnnotationCreate {
            body: Some("  ".into()),
            tags: None,
            ..req(Some(1), None, None)
        };
        assert_eq!(empty.validate(), Err(E::Empty));
        let long = AnnotationCreate {
            body: Some("x".repeat(MAX_ANNOTATION_BODY + 1)),
            ..req(Some(1), None, None)
        };
        assert_eq!(long.validate(), Err(E::BodyTooLong));
        let bad = AnnotationCreate {
            session_uuid: "../x".into(),
            ..req(Some(1), None, None)
        };
        assert_eq!(bad.validate(), Err(E::BadUuid));

        let mut row = ok.into_row("a1".into(), "alice", 10.0);
        assert_eq!(
            (row.author.as_str(), row.created_at, row.updated_at),
            ("alice", 10.0, 10.0)
        );

        assert_eq!(AnnotationPatch::default().validate(), Err(E::EmptyPatch));
        let add = AnnotationPatch {
            add_tags: Some(vec!["Later".into(), "ops".into()]),
            ..AnnotationPatch::default()
        }
        .validate()
        .unwrap();
        assert!(!add.needs_author());
        add.apply(&mut row, 11.0).unwrap();
        assert_eq!(row.tags, ["later", "ops"]);
        assert_eq!(row.updated_at, 11.0);
        let edit = AnnotationPatch {
            body: Some(String::new()),
            tags: Some(vec![]),
            ..AnnotationPatch::default()
        }
        .validate()
        .unwrap();
        assert!(edit.needs_author());
        assert_eq!(edit.apply(&mut row, 12.0), Err(E::Empty), "nothing would be left");
        assert_eq!(row.updated_at, 11.0, "a failed change leaves the row alone");
        let p: AnnotationPatch = serde_json::from_value(json!({"add_tags": ["x"]})).unwrap();
        assert_eq!(p.add_tags, Some(vec!["x".to_owned()]));
    }

    #[test]
    fn annotation_conversion_sorts_tags() {
        let a = im::Annotation {
            uuid: "a".into(),
            session_uuid: "s".into(),
            cmd_seq: None,
            t_from: Some(1.0),
            t_to: Some(2.0),
            author: "bob".into(),
            body: "b".into(),
            tags: vec!["z".into(), "a".into(), "z".into()],
            created_at: 1.0,
            updated_at: 2.0,
            deleted_at: None,
        };
        let w = Annotation::from(&a);
        assert_eq!(w.session, "s");
        assert_eq!(w.tags, ["a", "z"]);
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
