// Generated from crates/cinelog-web/src/api/types.rs by ts-rs. Do not edit.
// Regenerate: CINELOG_WRITE_TS=1 cargo test -p cinelog-web --lib api::types

export type Scope = "team" | "personal";

export type SessionState = "live" | "ended" | "crashed" | "degraded" | "error";

export type ChunkKind = "primary" | "alt" | "gap" | "provisional";

export type HitKind = "command" | "output";

export type SearchIn = "cmd" | "out" | "both";

export type ExportFormat = "cast" | "txt" | "html";

export type PlayMode = "cast" | "segmented";

export type LegacyStatus = "imported" | "hidden" | "skipped" | "failed";

export type ErrorCode = "bad_request" | "unauthorized" | "forbidden" | "not_found" | "method_not_allowed" | "conflict" | "payload_too_large" | "unsupported_media_type" | "too_many_requests" | "unavailable" | "internal";

export type ApiError = { error: ErrorCode, message: string, };

export type Health = { version: string, };

export type LoginInfo = { version: string, scope: Scope, 
/**
 * SHA-256 of the certificate's DER, uppercase hex pairs joined by `:` (the format
 * `cinelog install`, `doctor` and `tls show` print).
 */
fingerprint: string, };

export type LoginRequest = { 
/**
 * The token as printed by `cinelog token new` / `install` (base32, case-insensitive,
 * surrounding whitespace ignored).
 */
token: string, };

export type Me = { 
/**
 * Token name (TEAM: the member; PERSONAL: the user). Author of annotations and forgets.
 */
principal: string, scope: Scope, host: string, version: string, 
/**
 * Marker protocol version (1).
 */
protocol: number, fingerprint: string, 
/**
 * When the session ends without activity (idle TTL, capped by the absolute TTL).
 */
expires_at: number, };

export type TmuxLocation = { 
/**
 * `<server_pid>:<server_starttime>`: one tmux server instance.
 */
server: string | null, 
/**
 * `$3`.
 */
session_id: string | null, 
/**
 * Session name at the last `ctx` marker.
 */
session: string | null, 
/**
 * Window index.
 */
window: string | null, window_name: string | null, 
/**
 * Pane id (`%7`).
 */
pane: string | null, };

export type Session = { uuid: string, host: string, scope: Scope, user: string, euid: number | null, sudo_user: string | null, claimed_user: string | null, 
/**
 * `ancestor`, `loginuid`, `inherited`, `none` or `legacy`.
 */
attribution: string | null, login_via: string | null, tty: string | null, 
/**
 * PTY master holder: `tmux: server`, `sshd-session`, `terminator`, `sudo`, …
 */
holder_comm: string | null, tmux: TmuxLocation, 
/**
 * Outer session of a nested recording (sudo inside a user shell).
 */
parent_uuid: string | null, parts: number, compressed: boolean, cast_bytes: number | null, started_at: number, ended_at: number | null, 
/**
 * Cast length in seconds as far as indexed (`null` before the first index pass).
 */
duration: number | null, exit_status: number | null, 
/**
 * IANA zone of the recording host at start.
 */
tz: string | null, cols: number, rows: number, term: string | null, legacy: boolean, hidden: boolean, state: SessionState, commands: number, };

export type SessionPage = { sessions: Array<Session>, 
/**
 * Pass as `cursor=` for the next page; `null` on the last page.
 */
next: string | null, };

export type SessionRef = { uuid: string, user: string, scope: Scope, state: SessionState, started_at: number, ended_at: number | null, holder_comm: string | null, };

export type Marker = { t: number, ts: number, kind: string, 
/**
 * The marker's JSON fields (never contains the nonce).
 */
data: Record<string, unknown>, };

export type CastPart = { part: number, compressed: boolean, bytes: number | null, 
/**
 * Session time at which this part starts.
 */
t_offset: number, };

export type SessionDetail = { session: Session, parent: SessionRef | null, 
/**
 * Nested sessions started from this one, oldest first.
 */
children: Array<SessionRef>, 
/**
 * Oldest first.
 */
markers: Array<Marker>, parts: Array<CastPart>, 
/**
 * The `meta.forgotten` rows of this session.
 */
forgotten: Array<Forgotten>, 
/**
 * What playback and export mask, by start: forgotten ranges, and each forgotten
 * command from the end of the command before it to its own end. Known after the
 * command has left index.db, so clients draw these rather than deriving them.
 */
masked: Array<MaskedSpan>, 
/**
 * How [`PlayInfo`] will load this session.
 */
play_mode: PlayMode, };

export type Command = { seq: number, parent_seq: number | null, 
/**
 * Nesting depth (zsh inside zsh); 0 at the top.
 */
depth: number, t_start: number, 
/**
 * `null` while running, or when the shell never reported it.
 */
t_end: number | null, ts_start: number, ts_end: number | null, exit: number | null, pipestatus: Array<number>, cwd: string | null, user: string | null, 
/**
 * The command as typed (`$1` of preexec).
 */
typed: string, 
/**
 * The expanded command (`$3`), when it differs.
 */
expanded: string | null, 
/**
 * `typed` was cut at 16 KiB.
 */
typed_trunc: boolean, 
/**
 * Boundaries reconstructed by the legacy importer.
 */
approx: boolean, confidence: number | null, out_bytes: number, out_truncated: boolean, };

export type CommandList = { session: string, commands: Array<Command>, };

export type Entry = { session: Session, 
/**
 * The addressed command, or the command running at `@<t>`; `null` for gap output and
 * session entries.
 */
command: Command | null, 
/**
 * Rendered output text (rows joined by `\n`), forgotten ranges removed.
 */
output: string, 
/**
 * More output exists than `max_bytes`.
 */
output_truncated: boolean, };

export type Tree = { 
/**
 * By host name.
 */
hosts: Array<TreeHost>, };

export type TreeHost = { host: string, 
/**
 * Most recent activity first.
 */
tmux_servers: Array<TreeTmuxServer>, 
/**
 * Non-tmux shells by `holder_comm` (`null` group last: unknown holder, legacy casts).
 */
shells: Array<TreeShellGroup>, };

export type TreeTmuxServer = { 
/**
 * `<server_pid>:<server_starttime>`.
 */
server: string, sessions: Array<TreeTmuxSession>, };

export type TreeTmuxSession = { session_id: string | null, name: string | null, 
/**
 * By window index.
 */
windows: Array<TreeWindow>, };

export type TreeWindow = { window: string | null, name: string | null, panes: Array<TreePane>, };

export type TreePane = { pane: string | null, 
/**
 * Newest first.
 */
sessions: Array<TreeNode>, };

export type TreeShellGroup = { holder_comm: string | null, 
/**
 * Newest first.
 */
sessions: Array<TreeNode>, };

export type TreeNode = { uuid: string, user: string, state: SessionState, started_at: number, ended_at: number | null, legacy: boolean, hidden: boolean, commands: number, 
/**
 * Oldest first.
 */
children: Array<TreeNode>, };

export type SnippetPart = { text: string, hit: boolean, };

export type SearchHit = { 
/**
 * Stable key within one result list: `c:<commands.id>` or `o:<outputs.id>`.
 */
id: string, kind: HitKind, 
/**
 * Session uuid.
 */
session: string, 
/**
 * The matched command, or the command owning the matched output; `null` for gap output.
 */
cmd_seq: number | null, 
/**
 * Seek position: the command start, or the output chunk's first write.
 */
t: number, ts: number, user: string, host: string, pane: string | null, tmux_session: string | null, exit: number | null, cwd: string | null, typed: string | null, 
/**
 * Command hits: `typed` with the matches marked. Output hits: text around the first
 * match. Never empty.
 */
snippet: Array<SnippetPart>, chunk_kind: ChunkKind | null, approx: boolean, confidence: number | null, legacy: boolean, };

export type SearchResponse = { hits: Array<SearchHit>, next: string | null, 
/**
 * Terms under 3 characters run a 500 ms newest-first scan; `true` when it stopped
 * early (older matches may exist).
 */
partial: boolean, 
/**
 * More than 5000 candidates: refine the query.
 */
capped: boolean, };

export type SessionCounts = { live: number, ended: number, crashed: number, degraded: number, error: number, legacy: number, hidden: number, };

export type LiveLag = { session: string, indexed_bytes: number, file_bytes: number | null, updated_at: number | null, };

export type LegacyProgress = { files: number, imported: number, hidden: number, skipped: number, failed: number, banners: number, log_lines: number, aligned: number, };

export type Status = { version: string, protocol: number, scope: Scope, host: string, server_started_at: number, port: number, fingerprint: string, schema_version: number, index_bytes: number, sessions: SessionCounts, commands: number, outputs: number, live: Array<LiveLag>, backlog_bytes: number, legacy: LegacyProgress, 
/**
 * Last indexer error (one line, no cast content).
 */
last_error: string | null, };

export type ImportFile = { 
/**
 * Relative to the scope's logs root. Display only; the API never accepts paths.
 */
file: string, status: LegacyStatus, session: string | null, banners: number, log_lines: number, aligned: number, mean_conf: number | null, error: string | null, };

export type ImportReport = { progress: LegacyProgress, files: Array<ImportFile>, };

export type ForgetRequest = { session_uuid: string, cmd_seq?: number, t_from?: number, t_to?: number, };

export type ForgetTarget = { "kind": "session", uuid: string, } | { "kind": "command", uuid: string, seq: number, } | { "kind": "range", uuid: string, t_from: number, t_to: number, };

export type Forgotten = { target: ForgetTarget, 
/**
 * Principal who forgot it.
 */
by: string, at: number, };

export type MaskedSpan = { t_from: number, 
/**
 * `null`: a forgotten command that has not ended (masked to the end).
 */
t_to: number | null, 
/**
 * Principal who forgot it.
 */
by: string, };

export type PlayInfo = { mode: PlayMode, 
/**
 * Player source URL: `/api/sessions/{uuid}/cast` or `/api/sessions/{uuid}/play/index.json`.
 */
url: string, 
/**
 * `false` while segments are being built; poll again after about 500 ms.
 */
ready: boolean, 
/**
 * Build progress 0…1 while `!ready`.
 */
progress: number | null, duration: number | null, cols: number, rows: number, };

export type PlayTerm = { cols: number, rows: number, theme: PlayTheme | null, };

export type PlayTheme = { fg: string, bg: string, 
/**
 * 8 or 16 colours joined by `:`.
 */
palette: string, };

export type PlayIndex = { 
/**
 * Always 1.
 */
version: number, duration: number, term: PlayTerm, 
/**
 * Chapters `[t, "$ <typed>"]`, ascending.
 */
markers: Array<[number, string]>, segments: Array<PlaySegmentRef>, };

export type PlaySegmentRef = { start: number, 
/**
 * Relative to the index URL: `seg/<n>`.
 */
url: string, };

export type PlaySnapshot = { cols: number, rows: number, init: string, };

export type PlaySegment = { snapshot: PlaySnapshot, events: Array<[number, string, string]>, };

export type SearchParams = { q?: string, in?: SearchIn, 
/**
 * Case-sensitive (default: Unicode case-insensitive).
 */
case?: boolean, user?: string, host?: string, 
/**
 * `ts` lower bound, inclusive.
 */
from?: number, 
/**
 * `ts` upper bound, inclusive.
 */
to?: number, 
/**
 * A number, `ok` (0) or `fail` (non-zero).
 */
exit?: string, 
/**
 * Exact command cwd.
 */
cwd?: string, 
/**
 * tmux pane id (`%3`).
 */
pane?: string, 
/**
 * Only this session (uuid).
 */
session?: string, 
/**
 * Include hidden (`TERM=dumb` legacy) sessions.
 */
hidden?: boolean, 
/**
 * 1…500, default 50.
 */
limit?: number, cursor?: string, };

export type ShowParams = { 
/**
 * `<uuid>`, `<uuid>:<seq>` or `<uuid>@<t>` (the `EntryRef` forms of `cinelog show`).
 */
entry: string, 
/**
 * Output bytes, default 65536, at most 1048576.
 */
max_bytes?: number, };

export type SessionParams = { 
/**
 * `true`: only `state = live`; `false`: only finished sessions.
 */
live?: boolean, host?: string, user?: string, 
/**
 * `started_at` bounds, inclusive.
 */
from?: number, to?: number, pane?: string, legacy?: boolean, hidden?: boolean, 
/**
 * 1…500, default 100.
 */
limit?: number, cursor?: string, };

export type TreeParams = { 
/**
 * Sessions active in `[from, to]` (started before `to`, not ended before `from`).
 */
from?: number, to?: number, live?: boolean, hidden?: boolean, };

export type ExportParams = { format: ExportFormat, from?: number, to?: number, cmd?: number, };
