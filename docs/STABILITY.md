# What cinelog 1.x keeps stable

cinelog follows semantic versioning from 1.0.0. One version number covers everything in this
repository: `VERSION`, the Cargo workspace version, `web/package.json` and
`cinelog --version` always agree, and a release is tagged `v<VERSION>`. Within 1.x,
everything listed here changes only by **addition**: new optional fields, new marker or
event kinds, new subcommands, flags, endpoints and config keys. Nothing is renamed or
removed, and no output listed as machine-readable changes its shape.

## Frozen interfaces

| Interface | Specification | Rule within 1.x |
|---|---|---|
| Marker protocol v1 (plugin → recorder) | [PROTOCOL.md](PROTOCOL.md) §1–§4 | `cinelog rec --protocol` prints `1`. Any 1.x plugin works with any 1.x recorder or server |
| Cast files | asciicast v3; the `m` event kinds of PROTOCOL.md §5 | Readers keep unknown `m` kinds raw. Legacy casts are only ever read |
| Plugin ⇄ binary | PROTOCOL.md §6: commands, environment, nonce fd, `rec --check` lines, registry `format: 1`, `CINELOG_OPTS` | Additive only |
| Command line | `cinelog --help` (`crates/cinelog/src/cli.rs`) | Machine-readable outputs: `rec --check`, `link [--json]`, `search --json`, `show --json`, `status --json`, `doctor --json` (`{version, checks: [{id, status, message}]}`), `doctor --skew`, `token list --json`, `tls show --json`, `import --report --json`, `meta export`. Exit codes as documented |
| Web API v1 | [API.md](API.md); TypeScript types generated from `crates/cinelog-web/src/api/types.rs` | Additive only. `GET /healthz` → `{"version": "<VERSION>"}`, public |
| `server.json` | `{pid, url, port, fingerprint, version}` in the runtime dir | Read by `link`, `search`, `doctor` and the MOTD |
| Databases | `index.db` and `meta.db`, schema version 1 each (`PRAGMA user_version`) | Migrations only move forward. A binary that finds a newer schema opens it read-only and says so (`status`, `/api/status`): search, show and the web UI work, nothing is indexed or written. `index.db` can always be rebuilt from the casts; `meta.db` is authoritative and backed up nightly as JSON Lines |
| Config | `~/.config/cinelog/config.toml`, `/etc/cinelog/team.toml` (every key optional) | Keys are never renamed; unknown keys are ignored |
| Release assets | `cinelog-x86_64-unknown-linux-musl`, `cinelog-aarch64-unknown-linux-musl`, `SHA256SUMS`; `shell/checksums/v<VERSION>.sha256` on `main` | Checksum files are append-only and never rewritten |
| zim module | `zmodule cmprmsd/cinelog --on-pull 'zsh shell/fetch.zsh'`; entry `cinelog.plugin.zsh`; optional `shell/early.zsh` | `fetch.zsh` options and exit codes (0 = a working binary of `VERSION`, 1 = not, 2 = usage) |
| Shell surface | functions `cinelog`, `cinelog-link`, `cinelog-fetch`, `load_motd`; alias `cs`; `zstyle ':cinelog:' auto-fetch`; `CINELOG_DISABLE`; `/etc/cinelog/disable` | Additive only |
| TEAM system integration | `/usr/local/bin/cinelog`, `/usr/local/share/cinelog/`, the `# >>> cinelog >>>` … `# <<< cinelog <<<` block in `/etc/zsh/zshrc`, `cinelog-team.service`, `/etc/cinelog/`, `/var/lib/cinelog/`, `/run/cinelog/` | Changed only by `sudo cinelog install --team [--upgrade]` |
| PERSONAL integration | `~/.local/bin/cinelog` (symlink), `cinelog.service` (user unit), `~/logs/YYYY-MM/`, `~/.local/state/cinelog/<host>/` | Changed only by `cinelog install [--refresh]` |

## Version skew

Different 1.x versions of the parts of an installation work together. They are still
reported, so you know what to update:

- `cinelog doctor` compares this binary with the plugin checkout, the team binary
  (`/usr/local/bin/cinelog`), this scope's running server (`server.json`) and, if you ran
  `cinelog token team-login`, the team server (`/healthz`).
- The MOTD of a recorded shell prints the local part of the same checks
  (`cinelog doctor --skew`).
- A newer user binary than the team binary prints the exact upgrade command with the full
  path of your binary: `sudo ~/.zim/modules/cinelog/bin/cinelog install --team --upgrade`.
  Under sudo's `secure_path` a bare `cinelog` would usually be the old team binary.

An upgrade never interrupts recording: binaries are replaced by rename, so running
recorders keep their `(deleted)` executable until their shell exits, and the next shell
starts the new one.
