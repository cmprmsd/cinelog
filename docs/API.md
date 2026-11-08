# cinelog web API, version 1 (M3)

The HTTPS interface of `cinelog serve` (one server per scope: PERSONAL per user, TEAM for
root shells). The wire types live in `crates/cinelog-web/src/api/types.rs` and are generated
to `web/src/lib/api/types.ts` (`CINELOG_WRITE_TS=1 cargo test -p cinelog-web --lib
api::types`; CI fails when the TS file is stale). Type names below are those types.

Frozen for M3 by the contracts work package (M3-0). Changes are additive and go through it.
Live SSE, `/events`, the timeline and annotations are M4 and are only reserved here (§9).

---

## 1. Conventions

- **Transport:** HTTPS only (rustls, ring), HTTP/1.1 and HTTP/2 (ALPN `h2`, `http/1.1`).
  The listener binds `server.bind` (default `0.0.0.0:auto` and `[::]:auto`, the IPv6 socket
  is v6-only). `auto` is 10000 (TEAM) or `10000 + uid` (PERSONAL; hashed into 40000–59999
  above 65535); when taken, the next 16 ports are tried. The chosen port is persisted in
  meta.db `settings.port` and written to `server.json` with the URL and fingerprint.
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
| `forbidden` | 403 | `Origin` mismatch on a mutating request |
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
  client IP (429 with `Retry-After`). Success: **204** and
  `Set-Cookie: __Host-cl-<port>=<session id>; HttpOnly; Secure; SameSite=Strict; Path=/;
  Max-Age=<absolute TTL>`. The session id is 32 random bytes (base64url); meta.db
  `auth_sessions` stores only its sha256, the principal, the client IP and the sha256 of the
  `User-Agent`. Wrong token: 401.
- **Sessions:** idle TTL `server.session_idle_hours` (12 h), absolute TTL
  `server.session_max_days` (7 d). A cookie presented from another client IP or with another
  `User-Agent` is rejected with 401 and its session is deleted. Tokens and cookie values
  never appear in URLs or logs. `document.cookie` is empty (HttpOnly).
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

Every other non-`/api` path serves the SPA: an existing asset, else `index.html` for paths
without a file extension (client routes `/login`, `/`, `/s/{uuid}?t=|cmd=`), else 404.
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
  1–500, default 100. In M3 the sidebar polls `?live=true` every 5 s (M4 replaces this with
  `/events`).
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
`--fingerprint` the first certificate seen is trusted and its fingerprint printed (TOFU).
`search --team` then talks HTTPS to that URL with a client that accepts exactly the pinned
fingerprint: `POST /api/login` (cookie kept in memory), `GET /api/search`, `GET /api/show`
for the fzf preview. As root, `cinelog search` reads the team DB directly instead.

## 9. Reserved for M4

`GET /api/sessions/{uuid}/live` (SSE, player `eventsource` driver: first
`{cols, rows, time, init}`, then `[t, code, data]`, then event `done` or `{state:'offline'}`),
`GET /api/events` (SSE UI bus: `session-start`, `session-end`, `index-progress`,
`annotation`, `import-progress`), `GET /api/timeline?from=&to=&group=user|pane`,
`GET/POST /api/annotations`, `PATCH/DELETE /api/annotations/{uuid}`, `GET/POST /api/tags`.
These paths return 404 in M3.

## 10. Implementation map (M3 work packages)

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

`cinelog serve` (without `--no-http`) calls `cinelog_web::spawn(ctx.web_context())` after
opening the index writer; when the web server cannot start (TLS, ports) it logs the error and
keeps indexing. Tests that run `serve` and must not listen pass `--no-http`, or set
`server.bind = ["127.0.0.1:0"]`.
