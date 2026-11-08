# Cinelog

Records every interactive zsh session exactly once, as an asciicast v3 file with a marker for
every command (typed text, expansion, cwd, exit status). One static binary (`cinelog`) does the
recording; a zim module (`cinelog.plugin.zsh`) starts it and emits the command markers.

Version 0.10 is the first release of the rewrite: recorder, plugin, `link`, `pause`/`resume`,
`doctor` and `install`. 0.11 adds the index (`cinelog serve --no-http`, live indexing, the
compressor, the import of old casts) and `cinelog search`/`show`/`forget`; the web UI follows
in 0.12.
The marker protocol and the shell/binary interface are specified in [docs/PROTOCOL.md](docs/PROTOCOL.md).

## Installation

1. Install zsh and the [zim framework](https://github.com/zimfw/zimfw).
2. Add the module to `~/.zimrc`, with the download hook:

   ```zsh
   zmodule cmprmsd/cinelog --on-pull 'zsh shell/fetch.zsh'
   ```

3. Run `zimfw install`. `shell/fetch.zsh` downloads the release binary for your architecture
   into `<module>/bin/cinelog` and verifies it against the checksum committed in
   `shell/checksums/`. Without network access it builds the binary with `cargo` instead
   (`cargo build --release --locked`).
4. Optional, for the fastest start: make this the first line of `~/.zshrc`, above any instant
   prompt:

   ```zsh
   [[ -r ${ZIM_HOME:-~/.zim}/modules/cinelog/shell/early.zsh ]] && source ${ZIM_HOME:-~/.zim}/modules/cinelog/shell/early.zsh
   ```

5. Run `cinelog install` once (directories, config template, migration of the old settings,
   the indexer's systemd user unit), open a new terminal and check it with `cinelog doctor`.

`zimfw update` keeps the module and the binary in sync. Asciinema, Python and the old viewer
are no longer needed: `cinelog install` stops a running `http_server.py` of the old plugin and
warns if something else still listens on its port. The indexer runs as
`~/.config/systemd/user/cinelog.service` (`loginctl enable-linger` keeps it running while you
are logged out); without systemd, recorded shells start it themselves (`server.autostart`).
Old casts in `~/logs` are imported read-only on the first run (never written, never
compressed).

Checksum trust: only a module that tracks `main` verifies against the checksum committed in
this repository. The checksum commit lands on `main` after the release tag, so a module
pinned with `--tag vX.Y.Z` (or to a commit) verifies against the release's `SHA256SUMS` only,
and fetch.zsh warns about it. See [shell/checksums/README.md](shell/checksums/README.md).

As root (root's own zim `--on-pull`), fetch.zsh never builds with cargo, ignores `PATH`,
`TMPDIR` and `CINELOG_FETCH_BASE_URL` from the environment (only `--base-url` counts) and
runs no `bin/cinelog` that is not root-owned. Root gets its binary from a release download or,
for team setups, from `sudo <module>/bin/cinelog install --team`.

### Team (root) recording

```zsh
sudo ~/.zim/modules/cinelog/bin/cinelog install --team --system-zshrc
```

installs a root-owned copy of the binary and plugin and a guarded block in the system zshrc, so
root shells (`sudo -s`, `sudo -i`, `su`, root logins) are recorded below `/root/logs/<user>/`.
Always name the module's binary by its full path: under sudo's `secure_path` a bare `cinelog`
is the installed (possibly older) team binary. Upgrades use `... install --team --upgrade`.
Team recording is a convenience, not an audit control: pausing and leading-space commands stay
allowed, and every pause or skip leaves a visible marker.

## Usage

| Command | |
|---|---|
| `cinelog link [--json]` | session uuid and cast path of the current shell |
| `cinelog pause [--reason TEXT]`, `cinelog resume` | pause and resume the recording (a pause marker marks the gap); `resume` also continues a recording that stopped on a full disk |
| `cinelog search [-a] [TERM]` | search commands (and with `-a` their output) in fzf; `--print`/`--json` without fzf; `hist` in a recorded shell is the same |
| `cinelog show <entry>` | a command with its output (the fzf preview) |
| `cinelog forget <uuid>[:seq]` | removes a session or command from the index (casts stay on disk) |
| `cinelog status`, `cinelog import --report` | indexer state; per-file results of the legacy import |
| `cinelog index --once`, `cinelog index --rebuild` | one-shot indexing, or a rebuild from the casts (meta.db is kept) |
| `cinelog doctor` | checks the installation, the current shell and its terminal |
| `cinelog install [--refresh]` | personal setup |

Casts go to `~/logs/<YYYY-MM>/<host>_<time>_<id>.cast` (mode 0600). Commands that start with a
space leave only a "hidden command" marker. Keystrokes are recorded except at password-like
prompts (echo off, or a prompt ending in `password:`, `passphrase:`, `pin:`, `token:`).
Configuration lives in `~/.config/cinelog/config.toml`; `cinelog install` writes a commented
template. `touch ~/.config/cinelog/disable` switches recording off.

## Behaviour notes

- **tmux and terminal integration.** The recorder takes the place of the shell process. It
  follows the shell's working directory, so `#{pane_current_path}` and "open new tab here"
  keep working, and it mirrors the foreground command into its own command line, so tmux
  automatic-rename shows `vim` or `sleep` rather than `cinelog`; `ps -o comm` still shows
  `cinelog`. Set `record.mirror_cmdline = false` to turn the mirroring off.
- **Multiplexers.** Each tmux, screen or zellij pane is recorded by its own recorder; the outer
  recording pauses while a multiplexer client is in the foreground.
- **Login shells.** The outer shell execs into the recorder while it reads `.zshrc`, and the
  recorded inner shell is not a login shell, so `.zlogin` does not run. Move its content to
  `.zshrc`.
- **powerlevel10k instant prompt** above the module works, but sourcing `shell/early.zsh` at the
  top of `.zshrc` stays the recommended setup.
- **Failure never blocks a shell.** If recording cannot start (full or unwritable log directory,
  broken config, missing binary) the shell starts unrecorded with one `cinelog: not recording`
  warning and the reason in `~/.local/state/cinelog/rec.log`.

## Credits

Thanks to Marcin [ku1ik](https://github.com/ku1ik) for the [asciinema suite](https://github.com/asciinema/),
whose cast format and player cinelog builds on.
