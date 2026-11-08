# Manual test matrix

Run this before every release tag, together with the automated suites. It covers what CI and
the unprivileged test suites cannot cover:

- root on a real system;
- real terminals, consoles and remote logins;
- the aarch64 binary on ARM hardware;
- the tag-driven release.

Record each run in a copy of the [result sheet](#result-sheet) at the end.

Rules for every manual run:

- Use a test machine, or a throwaway user, for anything that needs root. Never point a test
  at real recordings.
- Commands that need root say so. Everything else runs as an ordinary user.
- "Recorded" means all of these:
  - the MOTD shows `cinelog: session <id> → <cast>`;
  - `cinelog link` answers;
  - the session shows up in the web UI within 3 s.

## 0. Automated suites (prerequisite)

| Suite | Command | Expect |
|---|---|---|
| Rust | `cargo fmt --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace` | green |
| Integration (unprivileged) | `cargo test -p cinelog-it` (zsh, tmux, screen, curl, python3; private `tmux -L` servers) | green; `SKIP` lines only for tools this machine lacks |
| Integration (root) | `CINELOG_IT_ROOT=1 cargo test -p cinelog-it --features root` on a machine with passwordless sudo; for the password prompt of I6 also `CINELOG_IT_PW_USER`/`CINELOG_IT_PW_PASS` | I5 (`sudo -s`, `sudo -i`, `sudo su -`) and the sudo part of I6 green |
| Upgrade | `cargo test -p cinelog-it --test i24_upgrade` | an older build is upgraded to this one in `--root` mode; the running recorder keeps its `(deleted)` exe; the skew warnings appear |
| Web | `pnpm -C web check && pnpm -C web lint && pnpm -C web test && pnpm -C web e2e` and `pnpm -C web e2e:real` | green, no CSP report |
| Screenshots | `CINELOG_E2E_BIN=../target/debug/cinelog pnpm -C web docs:shots` | 33 tests: 21 dataset, docs and server checks and 12 screenshots (the split view checked for clipping at 1100, 1440 and 1600 px); review `docs/screenshots/` by eye |
| Benchmarks | `cargo test --release -p cinelog-it -- --ignored --test-threads=1 bench_` | B1 within 10 % of the baseline; B2 adds ≤ 35 ms (p95) |

## 1. Root and the team install (needs root)

On a test machine with zsh, systemd and a user with sudo rights.

| # | Step | Expect |
|---|---|---|
| R1 | `sudo <module>/bin/cinelog install --team --system-zshrc` | Binary at `/usr/local/bin/cinelog` (root:root 0755), the plugin copy below `/usr/local/share/cinelog/`, the `# >>> cinelog >>>` block in `/etc/zsh/zshrc`, `/etc/cinelog/`, `/var/lib/cinelog/`, `/root/logs` (0700). `cinelog-team.service` is active. A member token for `$SUDO_USER` is printed once, with the `token team-login` line and the advice for a separate host name |
| R2 | `sudo CINELOG_IT_ROOT=1 packaging/verify-team-unit.sh` | Every line `ok`, exit 0. It checks the hardened unit paths with an empty capability set, two members seeing the same history, revoke, and restart |
| R3 | In a recorded user shell: `sudo -i`, `sudo -s`, `sudo su -`, `su -` (with a root password), each followed by `id; exit` | One team cast per root shell below `/root/logs/<you>/<YYYY-MM>/`, with the right `sudo_user`. The user cast has a `nested` link and a pause. The timeline shows the root shell in your row |
| R4 | A real sudo password prompt: `sudo -k; sudo true` in a recorded shell | The password appears in no cast (`grep -r` the user and team casts as root) and in no search result |
| R5 | Root login without sudo: on the console, and `ssh root@localhost` if allowed | Recorded in TEAM scope with `login_via` set. Attributed by loginuid, else to `root` |
| R6 | `sudo touch /etc/cinelog/disable`, open a root shell, then remove the file | No team recording while the file exists |
| R7 | Two members: `sudo cinelog token new bob`; both run `cinelog token team-login …`, then `cinelog search --team id` | Both see the same root sessions. After `sudo cinelog token revoke bob`, bob's browser session and `search --team` get 401 |
| R8 | Upgrade: with a root shell still open, build or fetch a newer binary, then `sudo <module>/bin/cinelog install --team --upgrade` | The checksum is verified (or `--allow-unverified` is required for a local build). `ls -l /proc/<recorder pid>/exe` of the open root shell shows `(deleted)`, and the shell keeps recording until it exits. A new root shell runs the new version. `cinelog doctor` as a user shows no team skew |
| R9 | `sudo cinelog install --team --uninstall` | Units, binary, plugin copy and the zshrc block are gone. `/root/logs` and `/var/lib/cinelog` are kept |
| R10 | On a host **without systemd** (a container or a non-systemd distro) with a detached team server running: `sudo <module>/bin/cinelog install --team --upgrade` | No `systemctl` call is attempted. The detached team server is restarted on the new binary (`ls -l /proc/$(pgrep -f 'cinelog serve' -u root)/exe` points at the new `/usr/local/bin/cinelog`); `cinelog doctor` shows no team skew |

## 2. Terminals and logins

For each row, open a new shell in that environment, run `echo needle-$RANDOM`, then
`cinelog search needle-` and `cinelog doctor`.

In a recorded shell, `doctor` looks at the shell's own tty, which is the pts the recorder
created; its `session` line names the cast, and the registry file is always `pts-<N>.json`
for that inner pts. The outer terminal (its tty and the process holding it) is in the cast's
session marker. Take the cast path from doctor's `session` line and run

```zsh
grep -m1 -oE '\\"(tty|holder_comm)\\":\\"[^\\]*' <cast> | tr -d '\\"'
```

which prints `tty:/dev/pts/4` and `holder_comm:terminator`, say. A shell without a holder
process (a VT) has no `holder_comm`.

| # | Environment | Expect |
|---|---|---|
| T1 | Real VT (`Ctrl+Alt+F3` login, `TERM=linux`) | Recorded. The registry file is `pts-<N>.json` (the recorder's own pts). The session marker has `tty:/dev/tty3` and no `holder_comm` |
| T2 | terminator | Recorded; the session marker has `holder_comm:terminator` |
| T3 | alacritty | Recorded; colours and resize correct in playback |
| T4 | ghostty | Recorded; its shell integration still works |
| T5 | kitty | Recorded; the kitty keyboard protocol keystrokes do not corrupt the cast |
| T6 | VS Code integrated terminal | Recorded once; OSC 633 passes through. A VS Code window opening the folder does not hang ("Resolving shell environment" finishes) |
| T7 | `ssh` into the host (with and without tmux on the remote side) | One cast per remote pane; the session marker has `holder_comm:sshd-session` (older OpenSSH: `sshd`) or `holder_comm:tmux: server` |
| T8 | mosh | Recorded; the session marker has `holder_comm:mosh-server` |
| T9 | zellij, two panes | One cast per pane; the outer recording pauses while the client runs |
| T10 | GNU screen, two windows | One cast per window; the outer recording pauses |
| T11 | tmux with automatic-rename: `sleep 5` | The window name shows `sleep`, then `zsh`. New panes open in the current directory |
| T12 | powerlevel10k instant prompt, with `early.zsh` on the first line of `.zshrc` | Recorded. No p10k warning about console output during initialisation |
| T13 | Password prompts: `ssh-keygen -p`, `git` HTTPS credentials, `gpg --symmetric`, a remote `sudo` inside ssh | The secrets are not in any `i` event. Text typed at prompts that echo (git user name) is recorded |

## 3. aarch64

| # | Step | Expect |
|---|---|---|
| A1 | On an ARM64 machine: `zimfw install` with the module | `fetch.zsh` picks `aarch64-unknown-linux-musl`, verifies it and runs `--version` |
| A2 | Open a shell, run a few commands, `cinelog search`, open the web UI | Recorded, indexed and served exactly as on x86_64 |
| A3 | `sudo …/bin/cinelog install --team --system-zshrc`, then a root shell | Team recording works |

## 4. Release (on the tag)

| # | Step | Expect |
|---|---|---|
| L1 | Push tag `v<VERSION>` (after VERSION, `Cargo.toml` and `web/package.json` agree) | `release.yml` builds the web UI, then both musl binaries. Each binary passes its smoke test (`--version`, `rec --check`) **on its native runner**, including `ubuntu-24.04-arm` |
| L2 | Release assets | `cinelog-x86_64-unknown-linux-musl`, `cinelog-aarch64-unknown-linux-musl` and `SHA256SUMS`; `sha256sum -c` passes |
| L3 | Checksum commit | `shell/checksums/v<VERSION>.sha256` lands on `main` and is identical to `SHA256SUMS` |
| L4 | Fresh user, module tracking `main`: `zimfw install` | Verified against the committed file, without a fallback warning |
| L5 | Module pinned with `--tag v<VERSION>` | Without the checksum file the release binary is refused and the pinned source is built; with `--trust-release-sums` it is verified against the release `SHA256SUMS` with a warning; with `v<VERSION>.sha256` copied from `main` it is verified against that file |
| L6 | A tampered asset (`CINELOG_FETCH_BASE_URL=file://…` with one changed byte) | Rejected with a checksum mismatch; the old binary stays |
| L7 | Offline (no network) | The cargo fallback builds `bin/cinelog`; the UI is the placeholder page if `web/dist` is missing |
| L8 | The `verify` job of the same run | `fetch.zsh` installs and verifies the release binary on both x86_64 and aarch64 runners against the published assets |
| L9 | `actionlint .github/workflows/*.yml` before tagging | No findings |

## 5. Upgrade and migration

| # | Step | Expect |
|---|---|---|
| U1 | Previous pre-release → this release on a machine with live shells: `zimfw update` | `bin/cinelog` is swapped atomically. Open shells keep recording on the `(deleted)` exe. `install --refresh` restarts the personal server, whether it runs as the unit or detached. The search history is intact. No schema migration error |
| U2 | Skew: upgrade the user binary but not the team binary | The MOTD of a new shell and `cinelog doctor` print `cinelog: …` lines with `sudo ~/.zim/modules/cinelog/bin/cinelog install --team --upgrade`. After the team upgrade, they are gone |
| U3 | Old plugin → 1.0, following [MIGRATING.md](MIGRATING.md) on a copy of a real old setup (old casts, `.commands.log`, the settings file, the Python viewer running) | The viewer is stopped and the settings are migrated. The import finishes: old casts are unchanged (sha256 before and after), the approx badges are shown, `import --report` gives sensible ratios |
| U4 | Rollback as in [MIGRATING.md](MIGRATING.md#rolling-back) | `zimfw build` accepts the `--frozen` line, and `zimfw update` prints `Skipping frozen module` for cinelog and leaves it at `097b5ba`. The old plugin works again; casts written by 1.0 still play in asciinema 3 |
| U5 | `cinelog.service` installed, but no user manager (for example an `ssh` login without lingering, `systemctl --user` fails); a detached server running: `cinelog install --refresh` | The unit start fails or no server takes `serve.lock`, so the server is restarted detached on the new binary; `cinelog doctor` shows no `server-version` warning |
| U6 | `CINELOG_ZIM_TEST_STRICT=1 zsh shell/tests/zim_test.zsh -v` (needs network to the pinned zimfw 1.20.2 release asset) | All checks pass |

## 6. Web UI

| # | Step | Expect |
|---|---|---|
| W1 | Firefox and Chromium, from another machine on the LAN: open the URL from `cinelog link` | The certificate warning shows the fingerprint `cinelog tls show` prints on the server host. Login works. `document.cookie` is empty |
| W2 | Both themes and the auto mode; the system theme switched while the page is open | The page and the player follow the theme. The choice persists across reloads |
| W3 | Keyboard only: `/`, type, `j`/`k`, Enter, `]`, `[`, Space, `n`, `a`, `e`, `t`, `l`, `?`, Esc | Every step works without the mouse, with visible focus |
| W4 | Two browsers on one session: annotate in one | It appears in the other within 2 s |
| W5 | A running shell: open it from the sidebar's live list, keep typing in the shell | The view follows within about a second and shows LIVE. When the shell exits, the view ends. When the recorder is killed, the view shows offline |
| W6 | Export the current command as `.cast`, `.txt` and `.html` | The `.cast` plays in asciinema-player. The text contains the output. The HTML opens offline, without script |
| W7 | Forget a command that printed a unique word | The word is gone from search, and playback shows `[redacted by <you>]`. `cinelog search -a <the command line>` no longer finds its echo either |
| W8 | A large cast (hundreds of MB): open it and seek through chapters | First open under 6 s, later opens under 1 s; seeks under 2 s |
| W9 | DECRSPS and other rare VT sequences in the player (for example `vttest`, menu 11) | Playback matches the terminal |

## Result sheet

Copy this table into the release notes or the tag message.

```
cinelog <VERSION> manual matrix — <date> — <tester>
machine: <distro, kernel, zsh, tmux>   arm: <machine>

| Section | Rows      | Result         | Notes |
|---------|-----------|----------------|-------|
| 0 auto  | all       | pass/fail      |       |
| 1 root  | R1–R10    |                |       |
| 2 terms | T1–T13    |                |       |
| 3 arm   | A1–A3     |                |       |
| 4 rel   | L1–L9     |                |       |
| 5 upg   | U1–U6     |                |       |
| 6 web   | W1–W9     |                |       |
```

Milestone records: [VERIFY-M1.md](VERIFY-M1.md) (recorder and plugin),
[VERIFY-M2.md](VERIFY-M2.md) (index, real-corpus import and search),
[VERIFY-M3.md](VERIFY-M3.md) (web and team server), [VERIFY-M4.md](VERIFY-M4.md) (live view,
timeline, annotations, export) and [VERIFY-M5.md](VERIFY-M5.md) (1.0 release and the final
security pass).
