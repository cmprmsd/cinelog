# Migrating to cinelog 1.0

This guide covers two starting points:

- [The old plugin](#from-the-old-plugin): asciinema, `hist`, the Python viewer, up to commit
  `bcd4406` on `main`.
- [A 0.10–0.13 pre-release](#from-a-010013-pre-release) of the rewrite.

Neither path changes or deletes a recording. Old casts are only ever read.

## From the old plugin

### What changes

| Old | 1.0 |
|---|---|
| `asciinema rec` started from `.zshrc`, `exit` when it ended | `cinelog rec` takes the shell's place. On any problem the shell starts unrecorded, with one warning |
| `LOGFILE` / `COMMANDS_LOGFILE` exported to every child (tmux panes wrote into the wrong cast) | Nothing exported. Each tmux pane, ssh login and terminal gets its own cast, decided from the PTY |
| `~/logs/terminal_<date>.cast` plus `.commands.log` | `~/logs/<YYYY-MM>/<host>_<time>_<id>.cast`, with the commands inside the cast as markers |
| `/root/logs/<SUDO_USER>/…` from root's own zim setup | The same directory, now written by the root-owned team install (`install --team`) |
| `hist [-a] TERM` (Python, docopt) | `cinelog search [-a] TERM` (native, with fzf) |
| `here` and `~/.zsh_history.d/` | `cinelog search --cwd .` (this directory and below). The old directory is left alone |
| `http_server.py` on `http://<host>:<10000+uid>/?token=…` | `cinelog serve` on **https**://`<host>:<10000+uid>`/. The token is typed once, then an HttpOnly cookie is used. No token ever appears in a URL |
| `~/.config/cinelog/cinelog-settings.conf` | `~/.config/cinelog/config.toml`. `install` migrates the old file once and leaves it in place |
| `CINELOG_AUTH_UUID` | Not reused (it leaked into recordings). `install` prints a new token |
| asciinema, python3-docopt, the bundled player | Not needed. The player is part of the web UI |

### Steps

1. **Save local edits of the module.** A module that was edited in place, or that diverged
   from GitHub, does not update cleanly. Stash the edits first:

   ```zsh
   git -C ~/.zim/modules/cinelog status --short
   git -C ~/.zim/modules/cinelog stash push --include-untracked -m 'before cinelog 1.0'
   ```

2. **Add the download hook** to the module line in `~/.zimrc`:

   ```zsh
   zmodule cmprmsd/cinelog --on-pull 'zsh shell/fetch.zsh'
   ```

3. **Update and recompile.** `zimfw compile` rebuilds the `.zwc` files, so a stale compiled
   copy of the old plugin cannot shadow the new one:

   ```zsh
   zimfw update && zimfw compile
   ```

   `shell/fetch.zsh` downloads the binary to `~/.zim/modules/cinelog/bin/cinelog`, verifies
   it and installs it.

   Then delete the old viewer's session pages. They are copies of recorded terminal output,
   and git leaves them behind because they were never tracked (the MOTD reminds you while
   the directory exists):

   ```zsh
   rm -rf ~/.zim/modules/cinelog/asciinema-player
   ```

4. **Optional:** source `shell/early.zsh` from the first line of `~/.zshrc` (see the
   [README](../README.md#install)).

5. **Run the personal install:**

   ```zsh
   ~/.zim/modules/cinelog/bin/cinelog install
   ```

   It:
   - creates `~/logs` and the state directory with private modes;
   - writes `config.toml`, carrying over `MOTD_ENABLED`, `SHOW_START_END_TIME`,
     `CHECK_FREE_SPACE` and `GET_EXTERNAL_IP` from the old settings file;
   - keeps `CINELOG_VIEWER_PORT` as the web port, so bookmarks keep their port (the scheme
     is now https);
   - stops your old `python3 …/cinelog/http_server.py`, and warns if something else still
     listens on that port;
   - links `~/.local/bin/cinelog` and starts the user unit `cinelog.service`. Without
     systemd, the recorder starts the server itself;
   - prints the **login token once** (the certificate comes with the server's first start;
     `cinelog tls show` prints its fingerprint);
   - ends with `cinelog doctor`.

   Use `--no-migrate` to ignore the old settings file.

6. **Open a new terminal.** New shells are recorded by `cinelog rec`. Shells that are
   already open keep their asciinema recording until they exit. Shells that still run the
   old plugin may start the old viewer again until they are closed.

7. **Let the import run.** The server imports the old casts in the background:
   - Files are opened read-only, and nothing is ever written next to them.
   - Command boundaries are rebuilt from the old `Begin`/`End` banners and the order of
     `.commands.log`. The old timestamps there were wrong.
   - Imported commands carry an "approx" badge and a confidence.
   - `cinelog import --report` and `cinelog status` show the progress, as aggregate numbers
     only.
   - The old `TERM=dumb` artefacts (empty casts from `zsh -i -c`) are hidden.

8. **Team (root) recording.** If root shells were recorded before, run once:

   ```zsh
   sudo ~/.zim/modules/cinelog/bin/cinelog install --team --system-zshrc
   ```

   This stops root's old Python viewers, installs the root-owned binary and plugin copy, the
   system zshrc block and `cinelog-team.service`, and creates your member token. Old casts
   below `/root/logs/<user>/` are imported into the team index.
   - Root's own zim setup can keep the module, or drop it. The system zshrc block loads the
     root-owned copy first, and a second copy then returns at once.
   - Old personal casts that contain `sudo` sessions stay in your personal index.

9. **Check:** `cinelog doctor` in a recorded shell. It reports anything that still points at
   the old setup:
   - a missing `--on-pull` hook;
   - a running old viewer;
   - `early.zsh` placed below the instant prompt;
   - root sourcing files from user-writable paths.

### Things to adjust by hand

- **`.zlogin` no longer runs** in the recorded shell, because the inner shell is not a login
  shell. Move its content to `.zshrc`, or for exported variables to `.zprofile`.
- **Scripts that called `hist`** should call `cinelog search --print` or `--json`.
- **Bookmarks** of the old viewer need `https://`. Session links are now `/s/<uuid>`, and
  `cinelog link` prints the current one.
- **Your stashed edits** from step 1 are still in `git -C ~/.zim/modules/cinelog stash list`.
  Most old options are now config keys (see the [README](../README.md#configuration)).
- **Disk space of the old casts.** 1.0 never compresses or rewrites old-plugin casts. To
  shrink them, write compressed copies elsewhere and delete the originals yourself once
  you have checked the copies:

  ```zsh
  cinelog archive-legacy --to /mnt/archive/cinelog --dry-run   # what would be written
  cinelog archive-legacy --to /mnt/archive/cinelog             # <DIR>/<path below ~/logs>.zst
  ```

  The copies are plain zstd (`zstd -d` restores the cast). The target may not be inside
  `~/logs`. A copy whose cast is still growing is written again on the next run.

### Rolling back

```zsh
cinelog install --uninstall                       # the user unit and ~/.local/bin/cinelog
sudo /usr/local/bin/cinelog install --team --uninstall   # if you ran install --team
```

Then check out the last old-plugin commit in the module directory and freeze the module, so
that `zimfw update` leaves it there. zimfw has no option to pin a commit; a frozen module is
never installed or updated, but its init script is still loaded.

```zsh
git -C ~/.zim/modules/cinelog checkout bcd4406
git -C ~/.zim/modules/cinelog stash pop   # only if you stashed edits in step 1
```

In `~/.zimrc`, replace the cinelog line (and drop its `--on-pull` hook) with

```zsh
zmodule cmprmsd/cinelog --frozen
```

and run `zimfw build && zimfw compile`. To leave the old plugin later, remove `--frozen` and
run `git -C ~/.zim/modules/cinelog checkout main` before `zimfw update`.

Old casts were never touched. Casts that 1.0 wrote are plain asciicast v3 and play in
asciinema 3 and other v3 players. The old viewer cannot play them; it only plays v1 and v2.

## From a 0.10–0.13 pre-release

Protocol v1 and both database schemas (v1) are unchanged, so no data migration runs.

1. **Personal:** run `zimfw update`.
   - `fetch.zsh` verifies the 1.0 binary and swaps it in by rename.
   - It then runs `bin/cinelog install --refresh`. This repoints `~/.local/bin/cinelog` and
     restarts your server, whether it runs as the user unit or detached.
   - Recorders that are already running keep their old `(deleted)` executable until their
     shell exits. The next shell uses 1.0.
2. **Team:** run
   `sudo ~/.zim/modules/cinelog/bin/cinelog install --team --upgrade`.
   - The new binary is verified against `shell/checksums/` and copied over the old one.
   - The plugin copy is refreshed.
   - `cinelog-team` is restarted. Running root recorders keep their old binary.
   - Call the module's binary by its full path: under sudo's `secure_path`, a bare `cinelog`
     is the old team binary.
3. **Removed in 1.0:** the `hist` shim. Use `cinelog search`.
4. **Version skew:** until everything is upgraded, the MOTD and `cinelog doctor` print one
   `cinelog: …` line per mismatch, with the upgrade command. Any 1.x plugin, recorder and
   server work together ([STABILITY.md](STABILITY.md)), so the order of the upgrades does not
   matter.
5. **Rolling back a release:** check out the older tag in the module and run its
   `fetch.zsh`. Database migrations only move forward: if the newer release had migrated
   `index.db` or `meta.db`, the older binary opens them read-only and says so in
   `cinelog status` and the web UI's status. `cinelog serve` then serves the web UI without
   indexing (logins are kept in memory), `search` and `show` work, and forget, annotations
   and `cinelog index` refuse to write until you upgrade again.

## Where things are

| | Personal | Team |
|---|---|---|
| Casts | `~/logs/<YYYY-MM>/` | `/root/logs/<member>/<YYYY-MM>/` |
| Config | `~/.config/cinelog/config.toml` | `/etc/cinelog/team.toml` |
| Index and meta DB | `~/.local/state/cinelog/<host>/` | `/var/lib/cinelog/` |
| Tokens | `~/.config/cinelog/auth.toml` (hashes only) | `/etc/cinelog/tokens.toml` (hashes only) |
| TLS certificate | `~/.config/cinelog/tls/` | `/etc/cinelog/tls/` |
| Runtime (registry, `server.json`, lock) | `$XDG_RUNTIME_DIR/cinelog/` | `/run/cinelog/` |
| Recorder log | `~/.local/state/cinelog/rec.log` | `/var/lib/cinelog/rec.log` |
| Kill switch | `~/.config/cinelog/disable` or `CINELOG_DISABLE=1` | `/etc/cinelog/disable` |
| Server | `cinelog.service` (user unit) | `cinelog-team.service` |
