# cinelog web API, version 1 (M3, M4)

The HTTPS interface of `cinelog serve` (one server per scope: PERSONAL per user, TEAM for
root shells). The wire types live in `crates/cinelog-web/src/api/types.rs` and are generated
to `web/src/lib/api/types.ts` (`CINELOG_WRITE_TS=1 cargo test -p cinelog-web --lib
api::types`; CI fails when the TS file is stale). Type names below are those types.

Frozen for M3 by the contracts work package (M3-0) and extended for M4 (M4-0): live viewing,
the UI bus, the timeline, annotations and tags, the export and live UI (§9). Changes are
additive and go through the contracts owner.

---

## 1. Conventions

- **Transport:** HTTPS only (rustls, ring), HTTP/1.1 and HTTP/2 (ALPN `h2`, `http/1.1`).
  The listener binds `server.bind` (default `0.0.0.0:auto` and `[::]:auto`, the IPv6 socket
  is v6-only). `auto` is 10000 (TEAM) or `10000 + uid` (PERSONAL; hashed into 40000–59999
  above 65535); when taken, the next 16 ports are tried. The chosen port is persisted in
  meta.db `settings.port` and written to `server.json` with the URL and fingerprint.
- **Certificate:** self-signed (or `server.tls_cert`/`tls_key`), valid for exactly 825 days
  (notAfter − notBefore, Apple's limit; notBefore is yesterday to allow for clock skew).
  The SANs are the host names, `server.hostnames` and the *stable* local IPs only: IPv6
  temporary, deprecated and DAD-failed addresses and every address on a virtual interface
  (`docker*`, `br-*`, `veth*`, `virbr*`, `vnet*`, `vmnet*`, `vboxnet*`, `lxcbr*`, `lxdbr*`,
  `incusbr*`, `podman*`, `cni*`, `flannel*`, `cali*`, `kube*`, `tun*`, `tap*`, `wg*`,
  `tailscale*`, `zt*`, `ppp*`) are left out, so the fingerprint does not change on ordinary
  restarts. The same stable set drives auto-regeneration (`server.tls_auto_regen`). Those
  addresses are still accepted as `Host`; to put one in the certificate (for example a
  WireGuard address), list it in `server.hostnames`. Without a readable
  `/proc/net/if_inet6` no global IPv6 address goes into the certificate.
- **JSON:** UTF-8, `Content-Type: application/json`. Field names are snake_case. Every
  field of a response is present; absent values are `null`. Integers are JSON numbers
  (all below 2^53).
- **Times:** `ts`, `ts_*`, `*_at` are UTC Unix epoch seconds (float, µs precision). `t`,
  `t_*` are session-relative cast seconds (rotation part offsets included).
- **Ids, never paths:** sessions are addressed by uuid (lowercase hyphenated; any case is
  accepted), commands by `(uuid, seq)`. The API never accepts a file system path and has no
  directory listing. The only path it returns is `ImportFile.file`, relative to the logs
  root, for display.
- **Untrusted text:** commands, output, snippets, marker labels and any annotation text
  are terminal data. Clients render them as text nodes only. The SPA never uses `{@html}`
  (`svelte/no-at-html-tags` is an error in `web/eslint.config.js`).
- **Pagination:** list endpoints return `next` (an opaque cursor string, `null` on the last
  page); pass it back as `cursor=`. Do not parse cursors.
- **Query parameters** are the fields of the `*Params` types. Booleans are `true`/`false`.
  Unknown parameters are ignored; malformed values are 400.

## 2. Errors

Every non-2xx response with a body is an `ApiError` `{error, message}`. `message` is for
humans and never contains tokens, terminal content, SQL or paths.

| `error` | Status | When |
|---|---|---|
| `bad_request` | 400 | malformed parameter or body; Host outside the allowlist |
| `unauthorized` | 401 | no, expired, revoked or rebound session cookie; wrong token |
| `forbidden` | 403 | `Origin` mismatch on a mutating request; changing another principal's annotation (§9.4) |
| `not_found` | 404 | unknown route, session, command or segment; forgotten session |
| `method_not_allowed` | 405 | |
| `conflict` | 409 | the request conflicts with the current state |
| `payload_too_large` | 413 | `/cast` of a cast above `index.playback_segment_threshold_mb` |
| `unsupported_media_type` | 415 | a mutating request whose body is not `application/json` |
| `too_many_requests` | 429 | login rate limit; `Retry-After` header in seconds |
| `unavailable` | 503 | no index yet (first start, rebuild) or a component is unavailable |
| `internal` | 500 | anything else (details only in the server log) |

## 3. Request checks (every request, in this order)

1. **Security headers** on every response, errors and assets included:
   `Content-Security-Policy: default-src 'self'; script-src 'self' 'wasm-unsafe-eval';
   style-src 'self' 'unsafe-inline'; img-src 'self' data:; font-src 'self' data:;
   connect-src 'self'; worker-src 'self'; object-src 'none'; base-uri 'none';
   form-action 'self'; frame-ancestors 'none'`, `X-Frame-Options: DENY`,
   `Referrer-Policy: no-referrer`, `X-Content-Type-Options: nosniff`,
   `Cross-Origin-Opener-Policy: same-origin`; `Cache-Control: no-store` on `/api/**`,
   `/healthz` and `/link/**`. (`'wasm-unsafe-eval'`: asciinema-player compiles its VT from
   WebAssembly; `'unsafe-inline'` styles: the player sets inline styles.)
2. **Host allowlist:** the host is taken from the request URI authority (HTTP/2
   `:authority`) and else from `Host`, port stripped, lowercase. Allowed: `server.hostnames`,
   the hostname, the FQDN, the current local IPs (refreshed every 60 s), `localhost`,
   `127.0.0.1`, `::1`. Otherwise 400.
3. **Origin:** for every method except GET, HEAD and OPTIONS, `Origin` must be exactly
   `https://<authority of this request>` (scheme, host and port), else 403; a request with
   a body must have `Content-Type: application/json`, else 415.
4. **Session** (all `/api/**` routes except `GET`/`POST /api/login`): see §4.

Public routes: `/healthz`, `/link/{uuid}`, `GET`/`POST /api/login` and the SPA assets
(which contain no data).

## 4. Auth

- **Tokens** (§7.2 of the plan): 32 random bytes shown once as unpadded base32 (52
  characters). Files store only `sha256(raw bytes)` (`crates/cinelog-web/src/tokens.rs`):
  PERSONAL `$XDG_CONFIG_HOME/cinelog/auth.toml` (one token, named after the user), TEAM
  `/etc/cinelog/tokens.toml` (one named token per member). Both 0600. The token name is the
  **principal** (author of forgets and, in M4, annotations). `cinelog token new|list|revoke`
  manages them; revoking deletes every auth session of that principal.
- **Login:** `POST /api/login` with `LoginRequest {token}`. Case and surrounding whitespace
  of the token do not matter. Constant-time comparison. At most 5 attempts per minute per
  IPv4 address or IPv6 /64 (429 with `Retry-After`); the limiter keeps at most 4096 clients
  and drops the least recently active ones first. Success: **204** and
  `Set-Cookie: __Host-cl-<port>=<session id>; HttpOnly; Secure; SameSite=Strict; Path=/;
  Max-Age=<absolute TTL>`. The session id is 32 random bytes (base64url); meta.db
  `auth_sessions` stores only its sha256, the principal, the client IP and the sha256 of the
  `User-Agent`. Wrong token: 401.
- **Sessions:** idle TTL `server.session_idle_hours` (12 h), absolute TTL
  `server.session_max_days` (7 d). A session is bound to the client IP and the sha256 of
  the `User-Agent` it logged in with:
  - another `User-Agent`, or another address of the **same** family (including another
    address in the same IPv6 /64), gets 401 and the session is deleted;
  - all loopback addresses (`127.0.0.0/8`, `::1`) count as one client, so `localhost`
    resolving to both does not log the user out;
  - a request from the **other** family (IPv4 vs IPv6) with the same `User-Agent` gets 401
    but the session is kept: a dual-stack browser whose happy-eyeballs reconnect uses the
    other family sees one failed request, not a logout;
  - a session whose principal no longer has a token (revoked) gets 401 and is deleted.

  IPv6 clients with temporary (privacy) source addresses log in again when their address
  rotates (about daily with `use_tempaddr=2`). Comparing by /64 was rejected on purpose: the
  server host often shares the victim's /64, so it would let the cross-port cookie replay
  of plan §7.2 / Q6 succeed. Tokens and cookie values never appear in URLs or logs.
  `document.cookie` is empty (HttpOnly).
- `GET /api/login` (public) → `LoginInfo {version, scope, fingerprint}` for the login page,
  which shows the fingerprint so the user can compare it with what `install` printed.
- `POST /api/logout` → 204, deletes the session, clears the cookie.
- `GET /api/me` → `Me {principal, scope, host, version, protocol, fingerprint, expires_at}`.

## 5. Endpoints

All paths below `/api` need a session (§4).

| Method and path | Query / body | Response |
|---|---|---|
| `GET /healthz` | | `Health {version}` (public) |
| `GET /link/{uuid}` | | 303 to `/s/{uuid}` (public; 404 for a non-uuid) |
| `GET /api/login`, `POST /api/login`, `POST /api/logout`, `GET /api/me` | | §4 |
| `GET /api/status` | | `Status` |
| `GET /api/sessions` | `SessionParams` | `SessionPage` |
| `GET /api/tree` | `TreeParams` | `Tree` |
| `GET /api/sessions/{uuid}` | | `SessionDetail` |
| `GET /api/sessions/{uuid}/commands` | | `CommandList` |
| `GET /api/search` | `SearchParams` | `SearchResponse` |
| `GET /api/show` | `ShowParams` | `Entry` |
| `GET /api/import` | | `ImportReport` |
| `POST /api/forget` | `ForgetRequest` | `Forgotten` |
| `GET /api/sessions/{uuid}/play` | | `PlayInfo` |
| `GET /api/sessions/{uuid}/cast` | | asciicast v3 stream |
| `GET /api/sessions/{uuid}/play/index.json` | | `PlayIndex` |
| `GET /api/sessions/{uuid}/play/seg/{n}` | | `PlaySegment` |
| `GET /api/sessions/{uuid}/export` | `ExportParams` | file download |
| `GET /api/sessions/{uuid}/live` | | `text/event-stream` (§9.1) |
| `GET /api/events` | | `text/event-stream` (§9.2) |
| `GET /api/timeline` | `TimelineParams` | `Timeline` (§9.3) |
| `GET /api/annotations` | `AnnotationParams` | `AnnotationPage` (§9.4) |
| `POST /api/annotations` | `AnnotationCreate` | 201 `Annotation` |
| `PATCH /api/annotations/{uuid}` | `AnnotationPatch` | `Annotation` |
| `DELETE /api/annotations/{uuid}` | | 204 |
| `GET /api/tags` | | `TagList` |
| `POST /api/tags` | `TagRequest` | `Tag` |

Every other non-`/api` path serves the SPA: an existing asset, else `index.html` for paths
without a file extension (client routes `/login`, `/`, `/s/{uuid}?t=|cmd=`, `/timeline`,
`/live/{uuid}`), else 404.
Assets under `/assets/` are content-hashed and `Cache-Control: public, max-age=31536000,
immutable`; everything else `no-cache`, with an `ETag`.

### 5.1 Status and import
- `Status`: version, marker protocol, scope, host, server start, port and fingerprint, then
  the indexer view of `cinelog status`: schema version, index.db size, session counts by
  state, command and output row counts, lag per live session, backlog bytes, legacy import
  progress and the last indexer error.
- `ImportReport`: `progress` plus one `ImportFile` per legacy file in path order (status,
  session uuid, banners, log lines, aligned commands, mean confidence, error). Aggregates
  only, never cast content.

### 5.2 Sessions
- `GET /api/sessions`: newest `started_at` first; filters `live` (`true` = state `live`,
  `false` = everything else), `host`, `user`, `from`/`to` (`started_at` bounds, inclusive),
  `pane`, `legacy`, `hidden` (hidden sessions are excluded unless `hidden=true`); `limit`
  1–500, default 100. The sidebar takes live sessions from `/api/events` (§9.2) and polls
  `?live=true` every 5 s only while that stream is down.
- `Session.duration`: cast seconds as far as indexed; `commands`: indexed commands.
- `GET /api/sessions/{uuid}` → `SessionDetail`: the session, its parent (`parent_uuid`) and
  nested children (oldest first), recorder markers (`pause`, `resume`, `nested`,
  `multiplexer`, `ctx`, `gap`, `rotate`, `error`, `clock`, …; `data` is the marker JSON
  without the nonce; ignore unknown kinds), parts, forgotten ranges and `play_mode`. A
  forgotten session is 404.
- `GET /api/sessions/{uuid}/commands` → `CommandList`: every command in `t_start` order
  (the chapters). `expanded` is `null` when equal to `typed`. `approx`/`confidence` mark
  legacy boundaries.

### 5.3 Tree
`GET /api/tree` → host › tmux server › tmux session › window › pane › sessions, plus
`shells` grouped by `holder_comm` (`null` group last: unknown holders, legacy casts).
Hosts by name; servers and tmux sessions by most recent activity; windows by index; sessions
newest first. A session whose `parent_uuid` is in the same response appears only under its
parent's `children`. Filters: `from`/`to` (sessions active in the window: started before
`to`, not ended before `from`), `live`, `hidden`.

### 5.4 Search
`GET /api/search` (`SearchParams`):
- `q`: substring, Unicode case-insensitive unless `case=true`. Empty or missing: the newest
  commands (with the filters).
- `in`: `cmd` (typed, expanded, cwd; default), `out` (output chunks) or `both`.
- Filters: `user`, `host`, `from`/`to` (`ts`, inclusive), `exit` (`<n>`, `ok`, `fail`;
  `null` exits never match `fail`), `cwd` (exact), `pane`, `session`, `hidden`.
- `limit` 1–500, default 50; `cursor`.
- Order: newest `ts` first. `partial`: a term under 3 characters ran the 500 ms
  newest-first scan and stopped early. `capped`: more than 5000 candidates, refine.
- `SearchHit.snippet` is a list of `SnippetPart {text, hit}`: for command hits, `typed`
  with every match marked; for output hits, the text around the first match. The server
  builds it from SQLite `snippet()` with the private-use sentinels U+E000/U+E001
  (`api::types::split_snippet`); sentinels never reach the client. Render each part as a
  text node, inside `<mark>` when `hit`.
- `SearchHit.t` is the seek position (command start, or the output chunk's first write);
  `id` (`c:<n>` / `o:<n>`) is a stable list key; results of one command can be grouped by
  `(session, cmd_seq)`.

### 5.5 Show
`GET /api/show?entry=<uuid>[:<seq>|@<t>]&max_bytes=` → `Entry`: the session, the command
(the addressed one, or the one running at `@<t>`), and its rendered output text up to
`max_bytes` (default 64 KiB, max 1 MiB; `output_truncated`). This is `cinelog show` over
HTTP and the fzf preview of `cinelog search --team`.

### 5.6 Forget
`POST /api/forget` with `ForgetRequest`: `session_uuid` alone (the whole session), with
`cmd_seq` (one command), or with `t_from` and `t_to` (a range in session seconds,
inclusive). Any other combination is 400. Response: 200 `Forgotten {target, by, at}` (`by`
is the principal; repeating a forget returns the existing row). Effects (§7.6 of the plan):
the request is recorded in meta.db `forgotten`; queries exclude it at once; the serve
daemon deletes the rows from index.db; every cast, segment, show and export response masks
the range (its `o` events are replaced by one `o` event `[redacted by <by>]`, its `i`
events dropped). Casts on disk are never rewritten. There is no redaction at index time
(user decision §15.7).

A command forget (`cmd_seq`, API or `cinelog forget <uuid>:<seq>`) removes the command and
its output from index.db (the normal command purge). Playback and export mask more: from
the end of the command before it (the session start for the first one) to its own end, so
the prompt and the echoed command line are hidden too. That span is playback-only and is
recorded in meta.db `settings` under `forget_span/<uuid>/<seq>` = `"<from> <to>"` (session
seconds, `-` for an open end), not as a `range` row, so the daemon's purge never blanks
other output; `SessionDetail.forgotten` lists only the `command` row, and
`SessionDetail.masked` (`MaskedSpan {t_from, t_to, by}`) lists every span playback masks, also
after the command has left index.db (the SPA draws its rail bands from it). The CLI records the
span before it purges; the server records it the first time it sees a forgotten command
still in index.db, or recovers it once from a cinelog cast's `cmd`/`done` markers. The
shell echoes the command line before the command's `cmd` marker, so in index.db it belongs
to the output before the command (unattributed, or the enclosing command's). A command forget
removes that echo too: in the output chunks that start within the span above (from the end of
the command before it to its start), every line showing a line of the typed command becomes
empty (line ordinals stay). Queries strip it at once, the purge rewrites the chunks, and a
rebuild strips it on the way in. Other output in that span stays searchable.

## 6. Playback

1. The client calls `GET /api/sessions/{uuid}/play` → `PlayInfo {mode, url, ready,
   progress, duration, cols, rows}`. `mode` is `cast` when every part together is below
   `index.playback_segment_threshold_mb` (8), else `segmented`. A large cast without
   segments in index.db (not indexed yet) gets them built on demand: `ready=false` with
   `progress` 0…1 until done; poll every ~500 ms (target: under 6 s for 517 MB the first
   time, under 1 s afterwards).
2. It then creates the player with `url`:
   - `cast`: `AsciinemaPlayer.create({url}, el, opts)`;
   - `segmented`: `AsciinemaPlayer.create({url, format: 'segmented'}, el, opts)`; the
     player does not accept `idleTimeLimit` or `markers` options in this mode.
3. Chapters for the command strip come from `/commands`; the player's own markers come from
   the stream (`cast`) or the index (`segmented`).

### 6.1 `GET /api/sessions/{uuid}/cast`
`Content-Type: application/x-asciicast`, streamed. One asciicast v3 document over all
parts: the header of part 1 (`version` first, `term`, `timestamp`), then the events with
interval times. Rewritten for the player: each cinelog `cmd` marker becomes
`[dt, "m", "$ <typed>"]`; every other `m` event is dropped; `o`, `i`, `r`, `x` are kept;
forgotten ranges are masked (§5.6). 413 when the cast is above the threshold.

### 6.2 `GET /api/sessions/{uuid}/play/index.json` → `PlayIndex`
asciinema-player 3.18 segmented index. The player rejects an index that breaks these rules:
- `version` = 1; `duration` ≥ 0; `term.cols`/`term.rows` positive integers; `term.theme`
  `{fg, bg, palette}` or `null`;
- `segments` non-empty; `segments[0].start` = 0; starts strictly increasing and below
  `duration`; `url` non-empty, relative to the index URL (`seg/<n>`);
- `markers` `[t, label]` with string labels (`$ <typed>`), ascending, each `t ≤ duration`.

### 6.3 `GET /api/sessions/{uuid}/play/seg/{n}` → `PlaySegment`
`{snapshot: {cols, rows, init}, events: [[t, code, data], …]}`:
- `init` is the avt dump of the terminal at the segment start (fed before the events);
- `events` non-empty, **absolute** session seconds, ascending; segment `n > 0` starts with
  an event at exactly its `start`; every event `t` is `≥ start` and `< next start` (last
  segment: `≤ duration`), and the last segment's last event is at exactly `duration`;
- codes `o`, `i`, `r` (`"COLSxROWS"`) and `m` (string label); the same rewrite and masking
  as `/cast`.

Segments come from index.db `segments` (cut about every 2 MiB or 60 s, with byte offset and
avt dump), so a request reads one segment's bytes from the cast (zstd parts via
`zst_frames`). Memory per request is bounded by one avt screen plus buffers.

## 7. Export

`GET /api/sessions/{uuid}/export?format=cast|txt|html` with either `cmd=<seq>` (one command,
`t_start`…`t_end`) or `from`/`to` (session seconds; default the whole session). Streamed with
`Content-Disposition: attachment; filename="cinelog-<uuid8>-<from>-<to>.<ext>"`:
- `cast` (`application/x-asciicast`): a standalone v3 slice: header, a `t=0` `o` event with
  the avt dump at `from`, then the events up to `to` (intervals; label rewrite and masking
  as in §6.1). It plays in asciinema-player on its own.
- `txt` (`text/plain; charset=utf-8`): the rendered terminal rows of the range.
- `html` (`text/html; charset=utf-8`): one self-contained document. Every character from
  the cast is escaped; SGR colours and attributes become inline-styled spans (whitelist);
  no script; its own `<meta http-equiv="Content-Security-Policy" content="default-src
  'none'; style-src 'unsafe-inline'">`.

The SPA downloads through `fetch` plus a blob URL, so the cookie applies.

## 8. Team search client (`cinelog search --team`)

`cinelog token team-login [--url URL] [--fingerprint SHA256]` reads the member's team token
(from the terminal without echo, or stdin), logs in once to check it, and stores
`$XDG_CONFIG_HOME/cinelog/team-token` (0600, TOML: `url`, `token`, `fingerprint`). Without
`--fingerprint` it first asks the server without a token (`GET /api/login`), shows the
certificate's fingerprint and asks for confirmation at the terminal (y/N on `/dev/tty`);
only then is the token read and sent, to that pinned certificate.
`search --team` then talks HTTPS to that URL with a client that accepts exactly the pinned
fingerprint: `POST /api/login` (cookie kept in memory), `GET /api/search`, `GET /api/show`
for the fzf preview (`cinelog show --team {1}`, a hidden flag). Result URLs (`--json`,
Enter) point at the team server. As root, `cinelog search` reads the team DB directly
instead.

## 9. M4: live view, UI bus, timeline, annotations, export UI

### 9.0 Server-sent events (both streams)
- `Content-Type: text/event-stream`, `Cache-Control: no-store`, `X-Accel-Buffering: no`. The
  session cookie authenticates them like every `/api` route (EventSource sends same-origin
  cookies); errors before the stream starts are normal `ApiError` responses (401, 404, 503).
- Every `data:` is one line of JSON. No `id:` fields and no `Last-Event-ID` handling: each
  connection starts from a full state (`LiveInit`, `hello`).
- A comment line (`: keep-alive`) every 15 s keeps proxies and the browser from timing out.
- At most 64 streams of each kind per server (`unavailable` beyond). Streams end when the
  server shuts down (`bus::Bus::closed`).

### 9.1 Live view: `GET /api/sessions/{uuid}/live`
The asciinema-player 3.18 `eventsource` driver
(`AsciinemaPlayer.create({driver: 'eventsource', url}, el, opts)`, `url` from `api.liveUrl`)
reads unnamed (`message`) events whose data is a `LiveMessage`:

1. **`LiveInit {cols, rows, time, init}`**, always first: the terminal at the join. The
   server starts from the newest playback checkpoint at or before the end of the cast on disk
   (index.db `segments` row or `index_state`, else the cast start), feeds the masked events up
   to the current end through avt and sends its `dump()`; `time` is the session time of the
   last event read. A late joiner therefore sees the current screen at once; a join reads at
   most one segment's bytes (about 2 MiB).
2. **`LiveEvent [t, code, data]`**, absolute session seconds, ascending: everything appended
   to the cast from then on (polled at least every 100 ms), with the playback rewrite of
   §6.1: `cmd` markers become `m` `$ <typed>`, other `m` dropped, `o` `i` `r` kept, `x`
   ends the stream; forgotten ranges are masked (meta.db re-read at least every 2 s and on
   every bus `forgotten`). Rotation parts are followed (`rotate` marker → next part), `t`
   stays continuous.
3. **`LiveOffline {state: "offline"}`** when the recorder died without writing `x` (the
   session becomes `crashed`, plan §6.4: registry pid/starttime/boot_id dead), then event
   `done`.
4. **event `done`** (empty data) after `x` or when the session leaves `live` normally. The
   player shows the end; the stream closes.

A session that is not `live` at connect (ended, crashed, legacy) gets its final screen as
`LiveInit`, then (`crashed`) `LiveOffline`, then `done`. Unknown or forgotten sessions: 404.
While the recorder is paused nothing is appended, the stream just stays open; the pause shows
from the bus (§9.2). The view is read-only: there is no input path anywhere in the API. The
browser reconnects the EventSource by itself after network errors; every connection gets a
fresh `LiveInit`.

### 9.2 UI bus: `GET /api/events`
Named events: the SSE `event:` is `BusEvent.type`, `data:` the JSON of `BusEvent.data`
(`BusEvent::name`, `BusEvent::data_json`; the SPA rebuilds the `BusEvent` object). Unknown
event names must be ignored.

| `type` | `data` | Sent |
|---|---|---|
| `hello` | `BusHello {version, server_started_at, live: [LiveSession]}` | first on every connection; a full resync of the live sessions |
| `session-start` | `LiveSession` | within 1 s of a recorder registry entry appearing |
| `session-update` | `LiveSession` | recorder status changed (pause, resume, degraded; registry polled every ≤ 500 ms) or index.db metadata of a live session changed (tmux names) |
| `session-end` | `SessionEnd {session, state, ended_at}` | within 1 s of the registry entry going away or the state leaving `live`; `state` `ended` when the cast has its `x`, else `crashed` |
| `index-progress` | `IndexProgress` | at most every 2 s while backlog, counts, lag or error change |
| `import-progress` | `LegacyProgress` | at most every 2 s while a legacy import runs |
| `annotation` | `AnnotationEvent {op, annotation}` | after every annotation create, change or delete (§9.4) |
| `tag` | `Tag` | after a tag is created or its colour changes |
| `forgotten` | `Forgotten` | after `POST /api/forget`, and within 2 s for rows `cinelog forget` adds to meta.db |

`LiveSession {session, status, reason, detail, since}`: `status` `recording`, `paused` or
`degraded` (the registry state); `reason`/`detail` split the registry `state_reason` at the
first `": "` (`api::types::split_state_reason`): `multiplexer` / `tmux: client`, `nested` /
`<child uuid>`, `user` / `<--reason text>`, or the degraded error. A session not yet in
index.db is sent with what the registry and the cast header give (`commands: 0`, `duration:
null`) and updated once indexed. A subscriber that falls more than 1024 events behind gets a
fresh `hello` and should refetch what it shows (annotations, timeline).

Client: `web/src/lib/api/events.svelte.ts` (`bus.subscribe(handler) → unsubscribe`,
`bus.status`). One EventSource for the app, opened while anything is subscribed; a refused
stream (HTTP error) is retried after 1 s, doubling up to 30 s, and checks `/api/me` so an
expired session signs out; while `status !== 'open'` callers fall back to polling.

### 9.3 Timeline: `GET /api/timeline`
`TimelineParams {from, to, group, user, host, hidden}` → `Timeline`. Plan §8.3, §15.2.
- **Window:** ts, default the last 24 h; `from < to`, at most 31 days (else 400). Sessions
  active in the window (started before `to`, not ended before `from`); hidden sessions only
  with `hidden=true`; forgotten sessions never.
- **Grouping:** the *person* of a session is `sudo_user`, else `claimed_user`, else `user`
  (TEAM root shells belong to the member). `group=user` (default): one lane per person
  (`key` `user:<name>`, by label), one row per pane (`pane:<host>/<tmux server>/<pane>`,
  label `<tmux session>:<window>.<pane>`) or non-tmux shell (`tty:<host>/<tty>`, else
  `session:<uuid>`; label `<holder_comm> <tty>`), rows by first activity. `group=pane`: one
  lane per pane or shell (key as the row key above), one row per person in it
  (`<lane key>|user:<name>`). Nested sessions (`parent_uuid` in the response) are drawn in
  their parent's row; `TimelineRow.sessions` lists every session of the row.
- **Blocks:** one `TimelineBlock` per command overlapping the window, by `ts_start`;
  `typed` is the first line cut to 200 characters; `ts_end: null` means running (draw to
  now) or never closed (draw to the session end). Forgotten commands are left out.
  Colours (client): exit 0 green, non-zero red, unknown grey, `approx` hatched.
- **Marks** (always sent, also when aggregated): `pause` spans from `pause`/`resume`
  markers (`reason`, `detail` from the marker; `ts_end: null` while still paused), `skip`
  points (leading-space commands), `nested` points (`child` = the nested session), `gap`
  spans (dropped output) and `forgotten` spans (`SessionDetail.masked`; `detail` = who).
  Every pause or skip is visible (plan §15.2).
- **Idle:** spans of at least 600 s inside the window without any block in any row
  (`idle`), drawn collapsed as a break mark.
- **Aggregation:** more than 20 000 commands in the window: `aggregated = true`, every row's
  `blocks` is empty and `density` holds the non-empty bins; `bin_secs` is the smallest of 60,
  300, 900, 1800, 3600, 10800, 21600, 86400 that gives at most 2000 bins over the window.
  `commands` is the total before aggregation.
- **Annotations:** the non-deleted annotations of the window's sessions
  (`MetaStore::find_annotations` with `sessions`), as `TimelineAnnotation` in wall time: a
  command annotation spans its command (`ts_start`…`ts_end`, or `ts_start` when open), a
  span annotation `t_from`…`t_to` mapped to wall time like output chunks (`started_at + t`,
  corrected by `clock` markers); `row` is the session's row key, `color` the first tag
  colour that is set.
- **Budget:** one week of team data in under 500 ms server time plus rendering.
- **SPA:** `/timeline?from=&to=&group=` (window and grouping in the URL). Brush to zoom,
  `+`/`-`, a date picker; hover shows `typed`, cwd and duration; a click on a block opens
  that command's commands and player (`SessionView` with `target {cmd}`, or
  `/s/<uuid>?cmd=<seq>`). It may refetch on bus `session-start`/`session-end`/`annotation`.

### 9.4 Annotations and tags
Stored in meta.db (authoritative, keyed by session uuid and command seq, so they survive
`index --rebuild`; part of the nightly backup and `cinelog meta export|import`).
`cinelog_index::MetaStore` has `put_annotation`, `annotation(uuid)`,
`find_annotations(&AnnotationQuery)`, `annotations(session)`, `tags()`, `put_tag(name, color)`.

- **Target:** exactly one of `cmd_seq` (a command) or `t_from`+`t_to` (session seconds,
  `0 ≤ t_from ≤ t_to`). A body or at least one tag is required.
- **Body:** plain text, at most 16 KiB (`MAX_ANNOTATION_BODY`), trailing whitespace removed.
  Rendered as text only; `http://` and `https://` URLs become links (`rel="noopener
  noreferrer"`, `target="_blank"`), nothing else is interpreted (no Markdown to HTML, no
  `{@html}`).
- **Tags:** normalised by `normalize_tag`: trimmed, lowercase, 1–40 letters, digits or
  `- _ . : / + # @`; at most 16 per annotation, stored sorted. Colours `#rrggbb`
  (`normalize_color` also takes `#rgb`); `null` means the client derives one from the name.
- `GET /api/annotations` (`AnnotationParams`): filters `session`, `cmd_seq`, `tag`, `author`;
  soft-deleted ones only with `deleted=true`; oldest `created_at` first (uuid breaks ties);
  `limit` 1–500, default 200; `next` cursor.
- `POST /api/annotations` (`AnnotationCreate`, validated by `AnnotationCreate::validate`):
  `author` is the principal, `uuid` a new UUIDv7. 404 when the session is unknown or
  forgotten or `cmd_seq` is not one of its commands. 201 with the `Annotation`.
- `PATCH /api/annotations/{uuid}` (`AnnotationPatch`, `AnnotationPatch::validate`): `body`
  and `tags` (replace) only by the author, else 403; `add_tags` by any principal (TEAM: any
  member can add tags). 404 for unknown or deleted annotations. Applied with
  `AnnotationChange::apply` (the result still needs a body or a tag). 200 with the result.
- `DELETE /api/annotations/{uuid}`: author only (403), soft delete (`deleted_at` and
  `updated_at` = now), 204; 404 when unknown or already deleted.
- `GET /api/tags` → `TagList` (by name, `count` = non-deleted annotations).
  `POST /api/tags` (`TagRequest`, `TagRequest::validate`) creates the tag or sets its colour;
  any principal; 200 with the `Tag`.
- **Sync:** every successful create, change or delete publishes `annotation` (`op`
  `created`, `updated` or `deleted`, the full row) and every tag change `tag`, after the
  commit. Open views apply them, so a second browser shows the change within 2 s. Last write
  wins; there is no version check.
- **SPA:** the annotations panel attaches to the current command or the rail selection (`a`),
  lists the session's annotations with author and time, offers tag autocomplete from
  `/api/tags`, shows edit and delete only to the author, and gives the rail its spans.

### 9.5 Export UI
The export endpoint is §7. The session view's export menu (`web/src/lib/export/`) offers
three sources × three formats: the current command (`cmd=<seq>`), the rail selection
(`from`/`to`) and the whole session; `.cast` (standalone slice: plays in asciinema-player on
its own), `.txt` (rendered rows) and `.html` (escaped, script-free, own CSP). Downloads go
through `fetch` plus a blob URL (`lib/download.ts`), so the cookie applies; the file name
comes from `Content-Disposition`. `e` opens the menu.

### 9.6 SPA wiring (contracts; the packages fill in the components)
- **Routes** (`lib/route.ts`): `/timeline` → `routes/timeline/TimelinePage.svelte` (no
  props); `/live/{uuid}` → `lib/live/LivePage.svelte` (`{uuid}`; root element keeps
  `data-testid="live-page"` and `data-uuid`). The header has Search and Timeline tabs.
- **Keys** (`lib/keys.ts`): `t` timeline, `l` recording ↔ live view of the same session,
  `a` annotate (`AnnotationsPanel.annotate()`), `e` export (`ExportMenu.open()`).
- **Sidebar:** live sessions from the bus (status badge when paused or degraded), each
  linking to `/live/{uuid}`.
- **Session view** (`lib/components/SessionView.svelte`) mounts
  `<ExportMenu {uuid} command selection onnotice>` (exports `open()`, `close()`),
  `<AnnotationsPanel session commands current currentTime selection onseek bind:spans>`
  (exports `annotate(): boolean`) and passes `bind:selection` and `annotations={spans}` to
  `Rail`; a live session shows "Watch live". `selection` is a `TimeRange` (`lib/spans.ts`,
  session seconds), set by dragging on the rail and cleared with `Esc`; `AnnotationSpan`
  is what the rail draws. A bus `forgotten` for the open session reloads it.
- **Signed-in principal:** `lib/principal.svelte.ts` (`principal.me`), set by `App`.
- **API client** (`lib/api/client.ts`): `liveUrl`, `timeline`, `annotations`,
  `createAnnotation`, `updateAnnotation`, `deleteAnnotation`, `tags`, `putTag`.

### 9.7 E2E mock
`web/e2e/mock/server.ts` serves `/api/events` (a `hello` with the live fixtures, and every
event a handler or `forget` publishes) and hands every other request, before the M3 method
check, to the M4 handlers in `web/e2e/mock/m4/` (`MockHandler`, `MockContext` in
`context.ts`): `live.ts` (M4-D), `timeline.ts` (M4-B), `annotations.ts` (M4-C). Each work
package writes its own specs (`e2e/live.spec.ts`, `e2e/export.spec.ts` M4-D,
`e2e/timeline.spec.ts` M4-B, `e2e/annotations.spec.ts` M4-C). The Playwright config does not
override the User-Agent (Chromium drops the override on some navigations while an
EventSource is open, and sessions are bound to the User-Agent).

## 10. Implementation map

### 10.0 M3 work packages

| Path | Owner |
|---|---|
| `docs/API.md`, `crates/cinelog-web/src/api/types.rs` → `web/src/lib/api/types.ts`, `src/{lib,state,error,server,assets,tokens}.rs`, CLI surface (`token`, `tls`, `search --team`), `crates/cinelog/src/tls.rs`, serve wiring | M3-0 contracts |
| `crates/cinelog-web/src/{tls,auth,hosts,headers}.rs`, `build.rs` | M3-A server core |
| `crates/cinelog-web/src/{api,play,export}/**` (except `api/types.rs`) | M3-B API and playback |
| `web/**` (except the generated types), `web/e2e/**` | M3-C frontend tab 1 |
| `packaging/**`, `crates/cinelog/src/install/team.rs`, `crates/cinelog/src/token.rs`, `crates/cinelog/src/search/team.rs` | M3-D team ops |

Entry points with frozen signatures (stand-ins until each package lands):

- M3-A: `tls::{install_provider, load_or_create, regenerate, inspect}`;
  `auth::{Auth::new(&WebContext, port), routes, session_routes, require_session,
  revoke_principal(&Path, &str) -> Result<u64>}`; `hosts::{HostAllowlist::{new, allows,
  refresh, start_refresh, cert_names}, request_host, check_host, check_origin}`;
  `headers::security_headers` (complete in the contracts commit).
- M3-B: `api::routes`, `play::{PlayState::new(&WebContext), routes}`, `export::routes`, all
  `Router<AppState>` merged under `/api` behind `auth::require_session`. Handlers extract
  `state::Principal` where an author is needed.
- M3-D: `token::{run(&TokenArgs), ensure_personal(&Ctx) -> Result<Option<NewToken>>}`
  (`install` prints the new personal token once), `search::team::store(&Ctx) ->
  Result<Box<dyn Store>>`.
- Contracts (complete): `server::{router, spawn, bind_all, BindSpec}`, `ServerHandle`,
  `assets::spa`, `tokens::{TokenFile, TokenEntry, NewToken, generate, hash, valid_name}`,
  `tls::{fingerprint, TlsOptions::from_context}`, `cinelog tls show|regen`.

### 10.1 M4 work packages

| Path | Owner |
|---|---|
| `docs/API.md` §9, `api/types.rs` M4 types, `src/bus.rs`, route tables (`api/mod.rs`, `server.rs`), `state.rs`; `cinelog-index` `MetaStore` additions (`annotation`, `find_annotations`, `tags`, `put_tag`; `AnnotationQuery`, `TagInfo`); SPA wiring of §9.6 (`App.svelte`, `Shell.svelte`, `Sidebar.svelte`, `SessionView.svelte`, `lib/{route,keys,spans,principal.svelte}.ts`, `lib/api/{client,events.svelte}.ts`); `e2e/mock/server.ts`, `e2e/mock/m4/context.ts`, `playwright.config.ts` | M4-0 contracts |
| `crates/cinelog-web/src/live/**` | M4-A live SSE and the bus watcher |
| `crates/cinelog-web/src/api/timeline.rs`, `web/src/routes/timeline/**`, `web/e2e/mock/m4/timeline.ts`, `web/e2e/timeline.spec.ts` | M4-B timeline |
| `crates/cinelog-web/src/api/annotations.rs`, `web/src/lib/annotations/**`, `web/src/lib/components/Rail.svelte` (the drag selection), `web/e2e/mock/m4/annotations.ts`, `web/e2e/annotations.spec.ts` | M4-C annotations |
| `web/src/lib/{export,live}/**`, `web/e2e/mock/m4/live.ts`, `web/e2e/{live,export}.spec.ts` | M4-D export UI and live UI |

Entry points with frozen signatures (stand-ins until each package lands; Rust stand-ins
answer 503 `unavailable`):

- M4-A: `live::{LiveState::new(&WebContext), start(&AppState), routes() ->
  Router<AppState>}` (`/sessions/{uuid}/live`, `/events`). `start` runs once inside the
  server runtime. Publishes on `state.bus`; ends streams on `state.bus.closed()`. May reuse
  the `pub(crate)` playback internals (`play::{source, stream, plan, session}`, `api::db`).
- M4-B: `api::timeline::routes()` (`GET /timeline`); `TimelinePage.svelte` (no props).
- M4-C: `api::annotations::routes()` (`/annotations`, `/annotations/{uuid}`, `/tags`);
  `AnnotationsPanel.svelte` (props of §9.6, `annotate()`); the drag gesture of `Rail`
  (`selection` is already a bindable prop drawn as a band).
- M4-D: `ExportMenu.svelte` (props of §9.6, `open()`, `close()`; complete for the three
  sources), `LivePage.svelte` (`{uuid}`; the stand-in shows the recording).
- Contracts (complete): `bus::Bus::{new, publish, subscribe, subscribers, close, is_closed,
  closed}`, `BusEvent::{NAMES, name, data_json}`, `split_state_reason`, the validators
  (`AnnotationCreate::validate` → `NewAnnotation::into_row`, `AnnotationPatch::validate` →
  `AnnotationChange::{needs_author, apply}`, `TagRequest::validate`, `normalize_tag`,
  `normalize_tags`, `normalize_color`), `From<&im::Annotation> for Annotation`,
  `From<im::TagInfo> for Tag`, `From<RecState> for RecStatus`; the `forgotten` bus event
  of `POST /api/forget`; the SPA bus client and sidebar.

No new crates or npm packages are needed for M4 (SSE: `axum::response::sse`, broadcast:
`tokio::sync`, streams: `tokio-stream`/`async-stream`, all present); a package that needs
one asks the contracts owner.

`cinelog serve` (without `--no-http`) calls `cinelog_web::spawn(ctx.web_context())` after
opening the index writer; when the web server cannot start (TLS, ports) it logs the error and
keeps indexing. Tests that run `serve` and must not listen pass `--no-http`, or set
`server.bind = ["127.0.0.1:0"]`.
