# cinelog marker protocol, version 1

Status: frozen for cinelog 1.x. Changes are additive only (new optional fields, new
kinds). Rust types: `crates/cinelog-proto/src/types.rs`.

This document specifies:

1. the in-band markers the zsh plugin and cinelog processes write into the PTY output
   stream (§1–§4),
2. what the recorder writes into the cast (§5),
3. the interface between the shell plugin and the `cinelog` binary: environment, nonce fd,
   `rec --check`, the registry, `CINELOG_OPTS`, `pause`/`resume`/`link` (§6),
4. compatibility rules and the threat model (§7, §8).

The key words MUST, MUST NOT, SHOULD and MAY are used as in RFC 2119.

---

## 1. Wire format

A marker is an OSC sequence inside the output stream of the PTY that the recorder reads:

```
ESC ] 7777 ; cinelog ; <ver> ; <nonce> ; <enc> ; <payload> <ST>

  ESC ]    = 0x1b 0x5d
  <ver>    = "1"                              protocol version, decimal
  <nonce>  = 32 lowercase hex chars           session nonce (128 bit, per recording)
           | "-"                              recorder-to-recorder message (§4.5)
  <enc>    = "b" | "j"                        payload encoding (§1.1)
  <payload>= encoded JSON object              at most 65536 bytes (MAX_ENCODED_PAYLOAD)
  <ST>     = BEL (0x07) | ESC \ (0x1b 0x5c)
```

The fixed prefix is the byte string `ESC ] 7 7 7 7 ; c i n e l o g ;` (`WIRE_PREFIX`).

### 1.1 Encodings

| `enc` | Payload | Used when |
|---|---|---|
| `b` | base64, standard alphabet (`A–Z a–z 0–9 + /`), **no `=` padding**, of the UTF-8 JSON | JSON length ≤ 1536 bytes (`BASE64_MAX_JSON`) |
| `j` | the UTF-8 JSON itself, in which every byte 0x00–0x1f and 0x7f is written as `\u00XX` (lowercase or uppercase hex) and `"`/`\` are escaped as usual | JSON length > 1536 bytes |

Emitters MUST choose the encoding by that threshold (`Encoding::for_json_len`). Receivers
MUST accept both encodings for any length. Neither encoding contains a byte below 0x20 or
0x7f, so the payload cannot terminate or corrupt the OSC. Bytes 0x80–0xff (UTF-8) are
harmless: the parser works on bytes and terminals parse UTF-8 before C1 controls.

Rationale: pure-zsh base64 costs about 3.8 µs per byte (2 KiB = 8 ms) while escaping 16 KiB
of JSON costs about 1.2 ms.

### 1.2 Parsing rules (recorder)

- The scanner is streaming: a marker may be split across any number of reads. Its result
  MUST be identical for every split of the input (tested at every split offset).
- Only a possible partial prefix and an unterminated marker are held back; all other bytes
  pass through immediately. At end of stream held-back bytes are released unchanged.
- CAN (0x18) or SUB (0x1a) inside a marker aborts it; the held bytes and the CAN/SUB pass
  through unchanged (counted as `aborted`).
- A payload longer than 65536 bytes aborts the marker; its bytes pass through (`oversized`).
- A bad `<ver>`, `<nonce>` or `<enc>` field makes the sequence pass through (`malformed`);
  a version other than `1` passes through (`unsupported_version`).
- A well-formed marker whose session nonce does not match the recording's nonce passes
  through **unchanged**, is not recorded as an `m` event, and is counted (`bad_nonce`).
- Markers with nonce `-` are always accepted by the scanner; the recorder then decides
  (§4.5).
- The sum of these counters is the session's `bad_markers`.
- A sequence is released as soon as it can no longer be accepted, without waiting for a
  terminator: bad `<ver>`/`<nonce>`/`<enc>` syntax, or a C0/DEL byte other than BEL, ESC,
  CAN or SUB in the payload, is `malformed` at that byte; an ESC in the payload that is not
  followed by `\` is `malformed` and is rescanned (it may start the next marker); a
  version other than `1` is `unsupported_version` at the `;` after `<ver>`; a nonce
  mismatch is `bad_nonce` at the `;` after `<nonce>`. Output stays byte-identical in all
  cases. An unterminated marker at end of stream is released uncounted.
- Invalid UTF-8 inside a decoded payload (zsh hands over command lines as raw bytes) is
  replaced by U+FFFD before the JSON is parsed; it does not make the marker undecodable.

### 1.3 Other sequences the plugin emits

These are standard and pass through to the terminal unchanged:

| Sequence | When |
|---|---|
| `OSC 133 ; A ST` | at every prompt (precmd) |
| `OSC 133 ; C ST` | preexec, before the `cmd` marker |
| `OSC 133 ; D ; <exit> ST` | precmd, when a command ran |
| `OSC 7 ; file://<host><cwd> ST` | precmd (cwd percent-encoded) |
| `OSC 0 ; <title> ST` / `OSC 2` | title, when `title=1` |

tmux drops unknown OSC sequences, which is why pausing on multiplexers is process-based
(§4.4) and why pane recorders sit *inside* each pane.

---

## 2. Payloads

Every payload is a JSON object with `"v":1` and `"k":<kind>` as its first two keys,
followed by kind-specific fields. Times named `t` are `$EPOCHREALTIME` (seconds since the
epoch as a JSON number with µs fraction); they are a cross-check only, the recorder's
event position is authoritative.

### 2.1 Kinds

| `k` | Emitter | Nonce | When |
|---|---|---|---|
| `hello` | plugin | session | once per shell instance, at its first precmd |
| `cmd` | plugin preexec | session | each command whose line does not start with a space |
| `skip` | plugin preexec | session | each command whose line starts with a space (no text) |
| `done` | plugin precmd | session | after a `cmd` of the same shell |
| `user-pause` | `cinelog pause` | session | on request |
| `user-resume` | `cinelog resume` | session | on request |
| `nested-start` | inner recorder | `-` | right after the inner recorder started recording |
| `nested-end` | inner recorder | `-` | after its shell exited, before it exits |

### 2.2 Fields

Types: `u32`/`u64` non-negative integers, `i32` integer, `f64` number, `str` string.
Fields marked *req* MUST be present; all others MAY be omitted and default to empty/0.

**`hello`** — `sh` (u32, *req*): shell pid `$$`; `sst` (u64): shell starttime, field 22 of
`/proc/$$/stat`; `ppid` (u32); `user` (str); `uid` (u32); `euid` (u32); `host` (str); `tty`
(str, `$TTY`); `cwd` (str); `zsh` (str, `$ZSH_VERSION`); `tmux` (object, only inside tmux:
`socket`, `server_pid` (u32), `session_id` from `$TMUX`, `pane` = `$TMUX_PANE`); `t` (f64).

**`cmd`** — `sh` (u32, *req*); `n` (u64, *req*): shell-local counter, incremented before
each `cmd`, so the first command of every shell is `1`; `typed` (str, *req*): preexec `$1`;
`expanded` (str): preexec `$3`; `cwd` (str); `user` (str); `euid` (u32); `host` (str);
`pane` (str, `$TMUX_PANE`); `t` (f64); `trunc` (bool, present only when true): `typed` or
`expanded` was cut (§3). Text is the exact bytes zsh provides (no `echo` interpretation, no
exclusions).

**`skip`** — `sh` (u32, *req*); `t` (f64). Carries no command text. It produces a visible
"hidden command" marker in the timeline (user decision: every skip is visible).

**`done`** — `sh` (u32, *req*); `n` (u64, *req*): the `n` of the `cmd` it closes; `exit`
(i32, *req*): `$?`; `pipestatus` (array of i32); `cwd` (str, cwd after the command); `t`
(f64).

**`user-pause` / `user-resume`** — `reason` (str): free text from `--reason`; `pid` (u32):
pid of the requesting `cinelog` process.

**`nested-start` / `nested-end`** — `uuid` (str, *req*): session uuid of the inner
recording; `pid` (u32, *req*) and `starttime` (u64, *req*) of the inner recorder; `scope`
(`"team"`/`"personal"`, *req*); `user` (str); `sudo_user` (str); `url` (str, the inner
session's web URL when its server is known); `exit` (i32, `nested-end` only).

### 2.3 Example

```
cmd payload:  {"v":1,"k":"cmd","sh":4242,"n":3,"typed":"ls -la","expanded":"ls --color=tty -la","cwd":"/tmp","user":"alice","euid":1000,"host":"box","t":1731070000.123456}
on the wire:  ESC]7777;cinelog;1;3f0c…(32 hex)…;b;eyJ2IjoxLCJrIjoiY21kIiwic2giOjQyNDIs…BEL
```

---

## 3. Plugin emission rules

- Markers are emitted only by a shell that proved it is the recorded inner shell (§6.2) and
  therefore holds the nonce in the non-exported shell variable `_cl_nonce`.
- Hooks are registered with `add-zsh-hook`; the plugin never defines `preexec()` or
  `precmd()` directly. Output is written with `print -rn`, never `echo`.
- **preexec**: if `$1` starts with a space, emit `OSC 133;C` and a `skip` marker, nothing
  else about the command. Otherwise increment `_cl_n`, emit `OSC 133;C`, then the `cmd`
  marker. Then, when `banners=1`, print the right-aligned banner
  `printf '%*s\n' $COLUMNS "Begin [YYYY-MM-DD HH:MM:SS]"`, and set the title when `title=1`.
- **precmd**: if a `cmd` is open: `OSC 133;D;<exit>`, the `done` marker, and when
  `banners=1` the banner `End   [YYYY-MM-DD HH:MM:SS]` (three spaces, right-aligned).
  After a `skip`: only `OSC 133;D;<exit>` (no marker, no banner).
  At the first precmd of the shell, `hello` is emitted before anything else. Always:
  `OSC 133;A` and `OSC 7`.
- JSON is built in pure zsh: `\` → `\\`, `"` → `\"`, every byte 0x00–0x1f and 0x7f →
  `\u00XX`. Budget: p95 under 2 ms for commands of 200 bytes or less, under 5 ms for any
  command.
- **Size bound:** the encoded payload of every marker MUST be at most 65536 bytes, or the
  recorder drops it as `oversized`. Emitters cap `typed` and `expanded` at 16384 bytes each
  (`MAX_COMMAND_TEXT`), cut on UTF-8 boundaries. Escaping can still grow them (a C0 byte
  becomes 6 bytes), so if the encoded payload would exceed the bound they cut both further
  by escaped length until it fits, and set `trunc:true` whenever text was cut. Any rule
  with that result is valid; `Cmd::fit_to_wire` in `cinelog-proto` is the reference (a
  field whose escaped form fits in half of the remaining budget is kept whole, the other
  gets the rest), the plugin halves both caps until the escaped texts fit. Lengths are
  counted in bytes (`setopt nomultibyte`).

---

## 4. Recorder processing

### 4.1 Accepting markers

1. The scanner (§1.2) finds the marker at an exact byte offset of the output stream.
2. If accepted, the marker is **stripped** from the terminal output (default,
   `record.strip_markers = true`; with `false` the bytes stay in both terminal output and
   `o` events, with the nonce field of the copy in the `o` event scrubbed as in §5.1) and
   becomes an `m` event at exactly that offset: `o` data before it is
   flushed as its own `o` event first.
3. The payload is decoded (`enc`) and parsed (`WireMessage::from_json`). Undecodable
   payloads and payloads with a version other than 1 are dropped and counted in
   `bad_markers`. Unknown kinds are ignored (§7).
4. Session-nonce markers of kind `nested-*` and `-` markers of any other kind are dropped
   and counted.

### 4.2 Sequence numbers

The recorder assigns `seq` to every `cmd`: a counter per session (continuing across
rotation parts) starting at 1. Every shell's `n` restarts at 1 (subshells, nested zsh), so
`(sh, n)` is unique only together with the shell instance; `seq` is unique per session.
A `done` is matched to the most recent open `cmd` with the same `(sh, n)` and carries that
`seq`; an unmatched `done` is written with no `seq`.

### 4.3 While paused

Markers are still parsed (so the recorder can resume and link), but nothing except `r`
(resize) events, `pause`/`resume`/`nested`/`clock` markers and the `x` exit event is written.
No `o` and no `i` events are recorded while paused; passthrough to the terminal continues.
A session-nonce marker (`hello`/`cmd`/`done`/`skip`) that arrives during a multiplexer or
nested pause makes the recorder check the foreground at once, so the `done` after
`tmux attach` is recorded right after the `resume` instead of up to 250 ms later.

### 4.4 Pause triggers (process-based)

- poll `tcgetpgrp(master)` every 250 ms; after every `cmd`/`done` marker every 20 ms for
  500 ms; while the foreground pgrp differs from the shell's, rescan its members and their
  descendants every 1 s; always check the pgrp leader's comm (catches `exec tmux`).
- A member or descendant whose comm is in `record.pause_on_multiplexer` (default
  `tmux: client`, `screen`, `zellij`) pauses with reason `multiplexer`, preceded by a
  `multiplexer` marker naming the client.
- **Resume** on `nested-end`, on `user-resume`, or when the foreground pgrp returns to the
  shell with no multiplexer left (a crashed inner process cannot leave the recorder paused).
  If an alternate screen (`?1049h`, `?1047h`, `?47h`) was entered without the matching
  reset during the detection lag, the recorder writes `resume` with `alt_fixup:true`
  followed by a synthetic `o` event `ESC[?1049l ESC[0m`.

### 4.5 Nested recorders

A `nested-start` with nonce `-` is honoured only if (a) the foreground pgrp differs from
the shell's pgrp and (b) a descendant in that pgrp has comm `cinelog` and its pid and
starttime equal the payload's `pid`/`starttime`. Otherwise it is dropped and counted.
`record.nested` then decides:

| `nested` | Effect |
|---|---|
| `pause` (default) | write `nested` link marker (`mode:"pause"`), then `pause{reason:"nested"}` |
| `record` | write `nested` link marker (`mode:"record"`), keep recording. **Copies root/team content into the user-readable personal cast.** |
| `ignore` | keep recording, no marker |

Every recorder writes `nested-start` to its own host-side stdout after it started, and
`nested-end` before exiting; when no recorder reads that tty (terminal emulator, tmux) the
sequence is simply ignored by the terminal.

### 4.6 User pause

`user-pause` pauses with reason `user`, `user-resume` resumes with reason `user`. They are
allowed in both scopes: TEAM recording is a convenience, not an audit control, and every
pause leaves a visible `pause` marker.

The recorder confirms through its registry entry (§6.5): `user-pause` leads to
`state:"paused"` with `state_reason` `user` or `user: <reason>`; `user-resume` leads to
`state:"live"`; a resume from `degraded` leads to `state:"live"` with `part` incremented.
`cinelog pause`/`resume` poll the entry (same `pid`) for up to 1.5 s and print
`paused <uuid>` / `resumed <uuid>`, or a note on stderr when no confirmation arrived. If the
session is already paused for another reason, or degraded, the marker is still sent and the
CLI prints that state instead of claiming a confirmation; `resume` of a multiplexer or
nested pause says that it ends by itself. An overlong `--reason` (one that would exceed the
§1 payload bound) is an error and nothing is sent.

---

## 5. The cast

Casts are asciicast v3, one JSON value per line, written by `cinelog rec`:

```
{"version":3,"term":{"cols":80,"rows":24,"type":"xterm-256color"},"timestamp":1731070000,"env":{"SHELL":"/bin/zsh","TERM":"xterm-256color"}}
[0.000000, "m", "{\"cl\":1,\"k\":\"session\",\"uuid\":\"…\",…}"]
[0.012345, "o", "…"]
[0.510000, "m", "{\"cl\":1,\"k\":\"cmd\",\"seq\":4,\"sh\":4242,\"n\":3,\"typed\":\"ls\",…}"]
[3.000000, "x", "0"]
```

- `version` is always the first header key. Event times are intervals since the previous
  event with exactly six decimals. Codes: `o` output, `i` input, `m` marker, `r` resize
  (`"COLSxROWS"`), `x` exit status (`128+signal` when killed by a signal).
- Output is accumulated as UTF-8 across reads; invalid bytes become U+FFFD and are counted
  as `lossy_bytes`.
- Files: mode 0600 in 0700 directories, created with `O_EXCL`:
  - PERSONAL: `<paths.logs>/<YYYY-MM>/<host>_<YYYYmmddTHHMMSS.ffffff>_<id8>.cast`
  - TEAM: `/root/logs/<sudo_user>/<YYYY-MM>/…`
  - rotation parts: `…_<id8>_part<N>.cast` (N ≥ 2, same stem as part 1)
  - `<id8>` is the last 8 hex digits of the UUIDv7 session uuid (its random tail); the
    time is the local start time.

### 5.1 `m` events

The data string of every cinelog `m` event is compact JSON with `"cl":1` and `"k"` as the
first two keys. **The nonce never appears in a cast, export or DB.** `m` events without a
`cl` key are foreign labels and are ignored by cinelog readers.

Every `o`, `i` and `m` data string passes through `scrub_nonce` before it is written,
whatever `record.strip_markers` says: a nonce the shell prints (say, `cat` of the registry)
appears as `<cinelog-nonce>`. A nonce split across master reads is caught too: a trailing
run of at most 31 lowercase hex characters of `o` text is held back and written with its
own timestamp at the next non-`o` event or once it is 1 s old. Not caught: a nonce whose
halves arrive more than 1 s apart, or one interleaved with other bytes.

| `k` | Written | Fields |
|---|---|---|
| `session` | t=0 of every cast and every part | `uuid`, `part` (1…), `continues` (file name of the previous part), `start_epoch_us`, `boottime_us`, `tz` (IANA), `tz_offset_s`, `scope`, `host`, `user`, `uid`, `euid`, `sudo_user`, `claimed_user`, `attribution` (`ancestor`/`loginuid`/`inherited`/`none`/`legacy`), `login_via`, `tty`, `holder_comm`, `holder_pid`, `rec_pid`, `rec_starttime`, `boot_id`, `shell_pid`, `tmux` (`socket`, `server_pid`, `server_start`, `session_id`, `pane`), `parent_uuid`, `input` (bool), `rec_version`, `protocol` |
| `clock` | every 5 min, and at once when \|Δrealtime − Δboottime\| > 2 s | `epoch_us`, `boottime_us`, `reason` (`periodic`/`step`) |
| `hello` | as received | the `hello` fields |
| `cmd` | as received | `seq` + the `cmd` fields |
| `done` | as received | `seq` (if matched) + the `done` fields |
| `skip` | as received | the `skip` fields |
| `pause` | pause starts | `reason` (`multiplexer`/`nested`/`user`), `detail` |
| `resume` | pause ends | `reason` (`nested-end`/`foreground`/`user`), `alt_fixup` |
| `nested` | corroborated `nested-start` | `child_uuid`, `scope`, `user`, `sudo_user`, `url`, `pid`, `starttime`, `mode` (`pause`/`record`) |
| `multiplexer` | before `pause{multiplexer}` | `comm`, `pid`, `pgrp` |
| `ctx` | pane recorders: at start and on change (checked every 30 s) | `session_name`, `window_index`, `window_name`, `pane_index`, `pane_id` |
| `gap` | after the writer queue overflowed | `dropped_bytes`, `from_t`, `to_t` (cast seconds) |
| `rotate` | last event of a part | `part` (the ending part), `next` (file name), `reason` (`size`/`age`/`resume`) |
| `error` | writer I/O error; in the failing part if it still fits, else right after the `session` marker of the continuation part | `error` (`enospc`/`eio`/`edquot`/`write`), `detail`, `degraded` (bool) |
| `stats` | last part only, right before `x` | `bad_markers` (scanner counters + undecodable payloads + uncorroborated `nested-start`), `dropped_bytes` (queue overflow), `lossy_bytes` (invalid UTF-8), `parts` (parts opened) |

Wall time of any event: piecewise-linear mapping of its offset between `session` and
`clock` markers (intervals use `CLOCK_BOOTTIME`, so suspend time counts).

When serving casts, the server rewrites `cmd` labels to `$ <typed>` and drops all other
`m` events for players; files on disk are never rewritten.

---

## 6. Binary interface for the shell

### 6.1 Commands

| Command | Output / effect | Exit |
|---|---|---|
| `cinelog --version` | `cinelog <VERSION>` (e.g. `cinelog 1.0.0`) | 0 |
| `cinelog rec --protocol` | `1` | 0 |
| `cinelog rec --check` | one line, §6.3 | 0 (also for `skip`) |
| `cinelog rec -- <shell> [args…]` | records; on any init failure execs `<shell>` unrecorded (§6.4) | the shell's status |
| `cinelog link [--json]` | text: `session <uuid>`, `cast <path>` and, once a server exists, `url <url>`, one per line; JSON: `{"uuid","cast","url","state"}` | 1 if not in a recorded session |
| `cinelog pause [--reason TEXT]` / `cinelog resume` | writes a `user-pause`/`user-resume` marker to `/dev/tty` with the registry nonce (§6.5); `resume` also leaves degraded mode by opening the next part; prints `paused <uuid>`/`resumed <uuid>` once confirmed (§4.6) | 0 once the marker is written; 1 if not in a recorded session, `/dev/tty` is not writable or the reason is too long |
| `cinelog doctor [--json]` | diagnostics | 0 ok, 1 problems found |
| `cinelog doctor --skew` | the local version-skew warnings (plugin vs binary, team binary, this scope's running server), one line each starting with `cinelog: `; nothing when all agree. No network, no ANSI; the plugin prints it in the MOTD | 0 |
| `cinelog install [--team [--system-zshrc] [--upgrade] [--binary PATH] [--module DIR] [--allow-unverified]] [--prefix DIR] [--refresh] [--uninstall] [--no-migrate]` | install; `--prefix` (alias `--root`) writes every system path below DIR. TEAM: the binary is `--binary`, else `<module>/bin/cinelog`, else the running binary if it lies inside the module; it is verified against `shell/checksums/v<module VERSION>.sha256` unless `--allow-unverified`; `--upgrade` from the installed team binary itself is refused. `--module` defaults to the checkout found up to 4 levels above the running binary. `--refresh` is idempotent: it points `~/.local/bin/cinelog` at the module binary and restarts this user's running server (unit or detached) so it runs that binary | 0 ok |
| `cinelog completions zsh` | completion script (committed as `shell/completions/_cinelog`) | 0 |
| `cinelog serve [--scope personal\|team] [--no-http] [--ensure]` | indexer and (without `--no-http`) web server of one scope; holds `serve.lock` in the runtime dir for its lifetime; `--ensure` returns at once if the lock is held, else starts the user unit or a detached `serve` | 0 |
| `cinelog search [-a\|--out] [-s] [--user U] [--host H] [--since W] [--until W] [--exit N\|ok\|fail] [--cwd DIR] [--pane P] [--tag T[,T…]] [--json\|--print] [--limit N] [TERM…]` | fzf over `<uuid>:<seq>\t<ts>\t<user>@<host>\t[exit]\t<typed>\t┊ <snippet>` lines (hidden first field), preview `cinelog show {1}`; Enter prints the URL (when no server is known: the cast path and `cinelog show <id>`); without fzf a plain list | 0; 1 no match |
| `cinelog show <uuid>[:<seq>\|@<t>] [--json] [--max-bytes N]` | the command (or session) header and its output text | 0; 1 not found |
| `cinelog forget <uuid>[:<seq>] [--from S --to S]` | records the target in `meta.forgotten`; the writer deletes it from index.db; casts are never rewritten | 0 |
| `cinelog index --once\|--rebuild [CAST…]` | one-shot discovery and backfill (only while the writer lock is free); `--rebuild` drops index.db first, meta.db is kept | 0; 1 some casts failed |
| `cinelog status [--json]` | indexer lag per live session, backlog, legacy import progress, last error, server state; a newer index.db/meta.db (after a rollback) is read and reported as the error `… read-only, not indexing` | 0 |
| `cinelog compress --now\|--dry-run` | zstd-compresses finished non-legacy casts older than `retention.compress_after_days` | 0 |
| `cinelog archive-legacy --to DIR [--dry-run]` | writes a zstd copy (the compressor's frame format) of every legacy cast the index knows to `DIR/<path below the logs dir>.zst` (tmp file, fsync, rename; 0600 files, 0700 dirs); originals are only read (`O_NOATIME`), never changed; a copy at the cast's current size is skipped; `DIR` inside the logs dir is refused; prints aggregate counts only | 0; 1 some casts failed; 2 error (no index, `DIR` refused) |
| `cinelog import --report [--json]` | per legacy file: banners, log lines, aligned commands, mean confidence (aggregates only) | 0 |
| `cinelog meta export\|import <FILE>` | meta.db as JSON Lines (`-` = stdout, `.zst` compressed) | 0 |

Upgrade hints name the full path of the user's binary (`sudo ~/.zim/modules/cinelog/bin/cinelog
install --team --upgrade`): under sudo's `secure_path` a bare `cinelog` is usually the old team
binary.

### 6.2 Recorder → inner shell

The recorder starts the inner shell non-login (`record.inner_login = false`) with `argv[0]`
`zsh`, and these variables, which the plugin unsets (`unset -m 'CINELOG_*'`) immediately:

| Variable | Value |
|---|---|
| `CINELOG_NONCE_FD` | number of an inherited fd (read end of a pipe, not close-on-exec) |
| `CINELOG_REC_PID` | recorder pid; must equal the shell's `$PPID` |
| `CINELOG_TTY` | inner slave path; must equal the shell's `$TTY` |
| `CINELOG_SESSION` | session uuid |
| `CINELOG_OPTS` | shell options, §6.6 |

The recorder writes exactly one line, `<32 lowercase hex>\n`, into the pipe and closes its
write end before the shell starts. The plugin accepts the fd only if `CINELOG_REC_PID ==
$PPID` and `CINELOG_TTY == $TTY`, then runs `read -r -u $fd _cl_nonce; exec {fd}<&-`. The
nonce is therefore never in any process environment, and a tmux server started later
cannot inherit it. The recorder also strips every inherited `CINELOG_*` variable from the
environment before adding these.

### 6.3 `rec --check`

One line on stdout:

```
start <scope> <reason>     scope = team | personal
recorded <uuid>            this tty is already recorded by our own recorder (same uid)
skip <reason>
```

| Start reasons | Holder of the PTY master (nearest ancestor first, then full scan, then comm) |
|---|---|
| `tmux-server`, `screen`, `zellij` | multiplexer server (each pane gets its own cast) |
| `sshd`, `mosh-server` | remote login |
| `sudo`, `su`, `doas` | privilege relay with its own PTY (`use_pty`) |
| `nested-uid` | our own recorder, but of another uid (`su` without a PTY) |
| `script`, `terminal`, `unknown` | anything else |
| `vt` | a VT or serial tty (`/dev/ttyN`) |
| `hidepid` | holder invisible under `hidepid=2`, `record.hidepid_fallback = "start"` |

| Skip reasons | |
|---|---|
| `not-a-tty` | fd 0 is not a terminal |
| `disabled` | kill switch present |
| `hidepid` | holder invisible, `record.hidepid_fallback = "skip"` |
| `error` | internal error or unreadable config (details in `rec.log`, written only when a recorder would start; a tty that is already recorded still answers `recorded <uuid>`) |

`CINELOG_REC_PID`/`CINELOG_TTY` from the environment are a cache only, honoured only when
they match `$PPID`/`$TTY` and the registry validates. At EUID 0 every inherited
`CINELOG_*`, `HOME` and `XDG_*` value is ignored; the only kill switch is
`/etc/cinelog/disable`.

### 6.4 Fallback (never fail the shell)

On any error before `forkpty` (unwritable log dir, a log dir with less than 64 KiB free
space or no free inodes (`statvfs`), bad config, registry dir, …) the
recorder appends the reason to `rec.log`, removes every `CINELOG_*` variable, sets

| Variable | Value |
|---|---|
| `CINELOG_FAILED` | reason (single line) |
| `CINELOG_FAILED_TTY` | tty of fd 0 |
| `CINELOG_FAILED_PID` | the recorder's own pid, which is the exec'd shell's `$$` |

and `execv`s the requested shell unrecorded (then `/bin/zsh`, then `/bin/sh`). The plugin
treats `CINELOG_FAILED_TTY == $TTY && CINELOG_FAILED_PID == $$` as "do not try again on
this tty" (exec-loop guard), prints one warning and unsets the variables.

### 6.5 Registry

- Location: PERSONAL `$XDG_RUNTIME_DIR/cinelog/`, or `${XDG_STATE_HOME:-$HOME/.local/state}/cinelog/run/`
  when `XDG_RUNTIME_DIR` is unset; TEAM (EUID 0) `/run/cinelog/`. Directory 0700.
- File per recorded inner tty: `pts-<N>.json` for `/dev/pts/<N>`, `tty-<name>.json` for
  other ttys (`/` in the name replaced by `-`). Mode 0600, owned by the recorder's euid;
  readers reject files that are not.
- Content: compact JSON (no whitespace) with keys in this order: `format` (1), `pid`,
  `starttime`, `boot_id`, `uuid`, `nonce`, `scope`, `cast`, `pts`, `host_tty`, `shell_pid`,
  `parent_uuid` (optional), `state` (`live`/`paused`/`degraded`), `state_reason`
  (optional), `part`, `version`, `opts` (optional: the exact `CINELOG_OPTS` value, §6.6,
  for shells that attach through the registry and never saw the variable). The plugin MAY
  extract `"nonce":"<hex>"`, `"uuid":"…"`, `"cast":"…"`, `"state":"…"`,
  `"state_reason":"…"` and `"opts":"…"` by pattern matching.
- Written atomically (temp file + rename, so every write gets a new inode) at start and on
  every state change (pause, resume, degrade, rotation); removed on exit. Shells detect
  changes from `inode:mtime:size` of one `zstat -L` (no fork; mtime alone has 1 s
  resolution) and print one warning when `state` becomes `degraded`.
- Valid only if `pid` runs with that `starttime` and `boot_id` matches; same-uid readers
  also check that `pid` holds the master of that pts.
- `cinelog pause/resume/link` and subshells (`recorded <uuid>`) find their entry through
  the tty of `/dev/tty`.

### 6.6 `CINELOG_OPTS`

`key=value` pairs separated by `,`; booleans `0`/`1`; unknown keys are ignored. Values
never contain `,` or `=` (`%`, `,` and `=` in the URL are written as `%25`, `%2C`, `%3D`),
so the plugin parses it with `_cl_opt=(${(s:,:)${CINELOG_OPTS//=/,}})`.

```
motd=1,banners=1,check_free_space=1,external_ip=0,title=1,title_link=0,url=
```

A user shell reads no config files itself.

---

## 7. Compatibility

- Unknown `k` values and unknown fields are ignored by the recorder; readers keep unknown
  fields raw (`markers.data_json`). Unknown `m` kinds in casts are kept raw.
- Protocol `1` is frozen for 1.x: any 1.x plugin works with any 1.x recorder or server.
- `cinelog rec --protocol` prints the supported version; the plugin warns once if its own
  major differs.

## 8. Threat model

The nonce protects against **accidental or printed forgeries**: `cat` of an old cast,
`curl` output, a program echoing OSC 7777. It is never written to casts, exports or the
DB, and never placed in an environment. It is **not** a boundary against processes of the
same uid, which can read the registry and can run `cinelog pause` anyway. `nested-start`
messages (nonce `-`) are accepted only after process-tree corroboration (§4.5), which a
same-uid process could also fake (prctl comm); this is documented, not prevented. TEAM
recording is a convenience: pausing, the kill switch, `nested = "ignore"` and
leading-space commands are allowed, and every pause or skip leaves a visible marker.
