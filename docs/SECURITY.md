# cinelog security notes

cinelog records everything that appears in your terminals, and it serves those recordings
over the network. This document covers:

- what ends up in a recording;
- who can read it;
- how the web server is protected;
- which risks remain by design.

The [README](../README.md) covers setup, and [PROTOCOL.md](PROTOCOL.md) §8 covers the marker
threat model in detail.

## Reporting a vulnerability

Please report it privately through GitHub's "Report a vulnerability" form on
[cmprmsd/cinelog](https://github.com/cmprmsd/cinelog/security), not in a public issue. Send
the version (`cinelog --version`), the scope (personal or team), and a reproduction that
uses synthetic data. Never send real recordings.

## 1. What is recorded

| Data | Recorded | Notes |
|---|---|---|
| Terminal output | Yes, everything the shell's PTY prints | Including the output of commands that start with a space (they only lose their command entry) |
| Keystrokes (`i` events) | Yes, with exceptions | Not recorded: while the terminal is in password mode (echo off, canonical input), after a prompt that ends in `password:`, `passphrase:`, `passcode:`, `PIN:` or `token:` (also German `Passwort`/`Kennwort`), while the recording is paused, and terminal answers (cursor reports and the like). The optional `"relay"` rule (`record.input_suppress`) also drops everything typed while `sudo`, `ssh`, `docker`, `kubectl` and similar run in the foreground. It is off by default |
| Command lines | Yes: the typed text and zsh's expansion, byte-exact | A command that starts with a space leaves only a "hidden command" marker |
| Context | user, uid/euid, `SUDO_USER`, host, tty, tmux pane, cwd, exit status, times | |

**There is no automatic redaction** in either scope: no regex scrubbing of secrets at record
or index time. `index.redact` exists as an optional, empty setting. A secret that is
printed, or typed where the terminal echoes it, is in the cast and the index. That includes
a password passed as a command argument, a token in a `curl` header, and `cat` of a key
file.

To keep something out:

- start the command with a space;
- `cinelog pause` before it and `cinelog resume` after it;
- `forget` it afterwards (§3).

## 2. Who can read what

| Item | Mode | Readable by |
|---|---|---|
| Personal casts `~/logs/YYYY-MM/*.cast` | 0600 in 0700 directories | you and root |
| Team casts `/root/logs/<member>/…` | 0600 in 0700 directories, owner root | root only. Members see them only through the team web server or `search --team`, with their own token |
| `index.db`, `meta.db`, nightly `meta-*.jsonl.zst` backups | 0600 in 0700 directories | the owner of the scope (you, or root for the team) |
| Tokens (`auth.toml`, `/etc/cinelog/tokens.toml`) | 0600 | Only `sha256` of each token is stored. A token is shown once, when it is created |
| Saved team login (`~/.config/cinelog/team-token`) | 0600 | you: your member token and the pinned certificate fingerprint |
| Registry (`$XDG_RUNTIME_DIR/cinelog/pts-N.json`, `/run/cinelog/`) | 0600 in 0700 directories | the recorder's user. It holds the session nonce (§4) |
| TLS key | 0600 | the server's user |

- Databases are refused on a network or synced file system (Syncthing, NFS, …), so the index
  does not travel to other machines.
- Old casts are imported read-only, and their files keep whatever modes they had.
- **`record.nested = "record"`** keeps your personal recording running while you work in a
  `sudo` shell. This **copies root's screen content into your user-readable personal cast.**
  The default `pause` writes only a link to the team session.

## 3. Forget

`cinelog forget <uuid>[:<seq>]`, `--from/--to`, or "Forget…" in the UI:

- removes the session, the command or the time range from `index.db`. For a command it also
  removes the command line that zsh echoed just before it (other output in that span stays);
- records the target in `meta.db`, so a rebuild of the index removes it again;
- **masks** that range in every cast, segment, live stream and export the server sends. The
  output becomes one `[redacted by <who>]` event, and the keystrokes are dropped.

**Old-plugin casts that are still growing.** An imported legacy cast is identified by its
name, size and first MiB, so it gets a new uuid each time the old plugin appends to it. The
import under the new uuid replaces the old session row and first carries meta.db's forget
rows and forgotten-command playback spans over to the new uuid (the rows of the old uuid
stay) and moves its annotations, so a forget keeps applying to search, show, playback and
export. If meta.db was written by a newer cinelog (read-only, §10.3 of the plan) and
something has to be carried over, the import of that cast fails until cinelog is upgraded.

Forget **never rewrites a cast on disk.** The owner of the file and root can still read the
original, and so can any backup of `~/logs`. Delete or edit the cast file yourself if the
data must be gone, then run `cinelog index --rebuild`.

## 4. Marker integrity

Commands are marked in-band with an OSC sequence that carries a 128-bit nonce, which is
fresh for every recording.

- The recorder hands the nonce to the shell over an inherited pipe. It never puts the nonce
  into an environment variable, so a later tmux server or child process cannot inherit it.
- The nonce is never written to a cast, an export or the database. A nonce that a program
  prints is scrubbed from the recorded output.
- Markers with a wrong nonce, for example from `cat` of an old cast, `curl` output or a
  hostile program, pass through unrecorded and are counted.

The nonce is **not** a boundary against processes of your own uid. They can read the
registry, and they can run `cinelog pause` anyway. It protects against accidental and
printed forgeries, not against yourself.

## 5. Root and team scope

- **Root ignores the environment.** At EUID 0, cinelog ignores `HOME`, `XDG_*` and every
  `CINELOG_*` variable. Paths are fixed system paths, and the only kill switch is
  `/etc/cinelog/disable`. `sudo -E` cannot redirect root's recording or config.
- **Root runs only root-owned code.**
  - The system zshrc block sources the root-owned plugin copy
    `/usr/local/share/cinelog/cinelog.plugin.zsh` before any user dotfile. A user-writable
    copy sourced later returns at once.
  - The plugin accepts as root only `/usr/local/bin/cinelog`, or a binary whose file and
    parent directories are root-owned and not group- or world-writable.
  - `shell/fetch.zsh` as root trusts only such paths. It ignores `PATH`, `TMPDIR` and
    `CINELOG_FETCH_BASE_URL` from the environment, and never builds with cargo.
- **`sudo -s` keeps your `HOME`.** If root's shell then sources your dotfiles, that is your
  code running as root, recorded or not. `cinelog doctor` warns when root's shell sources
  files from paths that are not root-owned.
- **The team binary** changes only through `sudo cinelog install --team [--upgrade]`. That
  command verifies the binary against `shell/checksums/` before copying it.
- **The team server** runs as a hardened system unit:
  - an empty capability set, `NoNewPrivileges`, `ProtectSystem=strict`, `ProtectHome=tmpfs`
    with only the team log root bound in (`/root/logs`, or `paths.logs` from team.toml);
  - private `/tmp` and devices, and a system call filter.

  It validates recorder registry entries only through world-readable `/proc` data.
- **Team recording is a convenience, not an audit control** (user decision). Members can
  pause, use the kill switch, set `nested = "ignore"` or start commands with a space. Each
  pause or skip leaves a visible mark in the timeline, but a root user who wants to act
  unrecorded can do so. Do not use cinelog as evidence against a privileged user.

## 6. The web server

### Transport

- HTTPS only (rustls), over HTTP/1.1 and HTTP/2. The default bind is every address
  (`0.0.0.0` and `[::]`) on port `10000 + uid` (personal) or `10000` (team). To limit it, set
  `server.bind`, for example `["127.0.0.1:auto"]`.
- The certificate is self-signed unless you configure `server.tls_cert`/`tls_key`.
  - `cinelog tls show`, `doctor` (once the server runs), `install --team` and the login
    page show its SHA-256 fingerprint. The personal `install` does not: the server creates
    the certificate on its first start. **Compare it before you enter a token.** Trust is on first use, so a
    mismatch means someone is in the way.
  - It is regenerated automatically when the host name or the stable addresses change, or
    when it expires within 30 days. The fingerprint then changes, and `doctor` says so.
- `cinelog search --team` pins the team server's fingerprint when you run `token
  team-login`, and refuses any other certificate.

### Authentication

- **Tokens.** 256 random bits, shown once (base32). They are compared in constant time.
- **Login rate limit.** At most 5 login attempts per minute per IPv4 address or IPv6 /64.
- **Session cookie.** A successful login sets
  `__Host-cl-<port>=<id>; HttpOnly; Secure; SameSite=Strict; Path=/`.
  - The port is part of the name, so the personal and team servers on one host do not
    overwrite each other's cookie.
  - The database stores only a hash of the session id.
  - The page's JavaScript cannot read the cookie (`document.cookie` is empty).
- **Session lifetime.** 12 h idle and 7 days at most (`server.session_idle_hours`,
  `session_max_days`).
- **Binding.** Each session is bound to the client address and to a hash of its User-Agent.
  A cookie replayed from another address of the same family, or with another User-Agent, is
  rejected, and the session is deleted.
- **Revocation.** `cinelog token revoke <name>` deletes every session of that principal.
- **No secrets in URLs.** Tokens and cookies never appear in a URL or a log line. Links
  carry only session uuids.

### Request checks

On every request, in this order:

1. **Security headers:**
   - `Content-Security-Policy: default-src 'self'; script-src 'self' 'wasm-unsafe-eval'; …;
     frame-ancestors 'none'` (the full policy is in [API.md](API.md) §3);
   - `X-Frame-Options: DENY`, `Referrer-Policy: no-referrer`, `X-Content-Type-Options:
     nosniff`, `Cross-Origin-Opener-Policy: same-origin`;
   - `Cache-Control: no-store` on the API.
2. **Host allowlist.** The configured names, the host name, the FQDN, the current local
   addresses and loopback. Any other `Host` (or HTTP/2 `:authority`) gets 400, which blocks
   DNS rebinding.
3. **Origin check.** Every state-changing request must carry an `Origin` that is exactly
   `https://<this host:port>`, or it gets 403. A request with a body must be
   `application/json`, or it gets 415. Together with the SameSite cookie, this stops
   cross-site and cross-port request forgery.

### Data handling

- **Ids only.** The API takes session uuids and command numbers, never file paths. It has no
  directory listing.
- **Public routes.** Without a session, only these answer: `/healthz` (the version), the
  login endpoint (version, scope and certificate fingerprint), `/link/<uuid>` (a redirect)
  and the static UI assets, which hold no data.
- **Untrusted text.** All terminal text, command text and annotation text is treated as
  untrusted:
  - search snippets reach the browser as plain text segments;
  - the UI never injects HTML (`{@html}` is banned by a lint rule);
  - annotation bodies are rendered as text, with only `http(s)` links made clickable;
  - the HTML export escapes every character, converts only whitelisted SGR colour
    attributes to inline styles, contains no script, and carries its own
    `default-src 'none'` CSP.
- **Live view is read-only.** There is no input path from the browser to a recorded
  terminal.

## 7. Residual risks

These risks are known and accepted, or left to deployment:

- **Cross-port cookie replay on a shared host.** Browsers send a host's cookies to every
  port of that host. Suppose a hostile local user on the server host runs a listener on
  another port, and lures a team member's browser to it. That user receives the member's
  team cookie, and could replay it **from the member's own address with the member's
  User-Agent**, which passes the binding.
  - Mitigation: give the team server a host name of its own (`server.hostnames =
    ["cinelog-team.lan"]`, and use only that name), so its cookie is never sent to the
    personal servers' origin.
  - `install --team` prints this advice.
- **Same-uid processes** can forge markers, pause your recording, and read your casts,
  databases and saved team login. cinelog does not defend a user against their own
  processes.
- **Root reads everything**, including every personal cast on the host.
- **The keystroke rules are heuristics.** A password prompt that keeps echo on and does not
  end in one of the known words is recorded, for example a custom prompt without a colon, or
  a remote prompt over ssh whose wording differs. Use `pause`, or add `"relay"` to
  `record.input_suppress`.
- **The pause has a short delay.** Pausing on a multiplexer client or a nested recording is
  process-based, with up to 250 ms of lag. The first frame of a `tmux attach` can land in
  the outer cast; an alternate-screen fix-up keeps playback correct.
- **Forgotten data stays on disk** (§3), and so do backups of the casts.
- **Release integrity rests on the repository.** Checksums are committed to `main` by CI.
  There are no detached signatures. A module pinned to a tag never receives that commit:
  `fetch.zsh` then refuses the release's own `SHA256SUMS` (it comes from the same server as
  the binary) and builds from the pinned source instead, unless you copy
  `shell/checksums/v<VERSION>.sha256` from `main` into the checkout or pass
  `--trust-release-sums` ([shell/checksums/README.md](../shell/checksums/README.md)).
- **Dependencies are audited in CI.** `cargo audit` (RustSec) checks `Cargo.lock` and
  `pnpm audit --prod` the bundled web dependencies on every push, pull request and night.
  Both were clean for 1.0 (2026-11-08: 292 crates against 1422 advisories; the two
  `pnpm audit` findings are in build tools only, eslint's minimatch and the Svelte vite
  plugin's deepmerge, and do not reach the binary).
- **LAN exposure.** By default the servers listen on all addresses. TLS, tokens and the
  checks above protect them, but a host on an untrusted network should restrict
  `server.bind`.
