# M1 verification record (v0.10)

Checks from plan Appendix B (M1) and the M1 acceptance list that unit and integration tests
do not cover on their own. Run on the dev machine (Arch Linux, kernel 6.18, zsh 5.9.2, tmux 3.7c,
GNU screen 5.0.2, Chromium 152 headless) on 2026-11-08 against a release build of this tree. Every
run used a temporary HOME/ZDOTDIR/XDG tree and a private `tmux -L` server.

| Item | Result | How |
|---|---|---|
| cmdline mirroring with tmux `automatic-rename` | pass | `automatic-rename-format '#{pane_current_command}'`: during `sleep 4` the window is named `sleep` (`#{pane_current_command}` = `sleep`, `ps -o args` of the pane pid = `sleep`, `ps -o comm` = `cinelog`); back at the prompt it is `zsh`. Also covered by I21. |
| recorder `chdir` with `pane_current_path` | pass | after `cd /tmp` in the recorded shell, `#{pane_current_path}` = `/tmp`. |
| start with `TERM=linux` | pass on a pty | `zsh -i` with `TERM=linux` on a pty records (cast `term.type` = `linux`, `cinelog link` answers). A real VT login (`/dev/ttyN`) needs a console session and stays on the manual list. |
| prompt heuristic | pass | Keystrokes typed at both `ssh-keygen` passphrase prompts and at git's `Password for 'https://…':` prompt are absent from `i` events. The text typed at git's `Username for …:` prompt and at the shell prompt is present, so there is no false suppression at echoing prompts. |
| `read -u` of the nonce fd under zim and powerlevel10k | pass | Fresh `zimfw install` with `romkatv/powerlevel10k` and the cinelog module, p10k instant prompt preamble at the top of `.zshrc`. The second shell (instant prompt cache present) is recorded: MOTD and session line on the terminal, `hello`/`cmd`/`done` markers in the cast, `cinelog link` answers. |
| B1 throughput (141 MB `cat`, headless) | pass | spike `ptyrec` median 1009 ms, `cinelog rec` median 1032 ms (+2 %); `bench_b1_throughput`: median 1080 ms vs. the 990 ms baseline (+9 %). |
| B2 startup | pass | `bench_b2_startup`: p95 added 17.4 ms (budget 35 ms). |
| marker encoding | pass | `shell/tests/plugin_test.zsh`: p95 1.1 ms at 200 B (budget 2 ms), worst realistic command 2.5 ms. |
| player | pass | A recorded cast (`cmd` labels rewritten to `$ <typed>` as the server will do) plays in asciinema-player 3.18.0 in headless Chromium: 8 chapter markers, `seek({marker:'next'})` lands on them, no console errors. |
| `zimfw install`, online | pass | Clean HOME, module from a git URL, `--on-pull 'zsh shell/fetch.zsh'`: the release asset is downloaded (`CINELOG_FETCH_BASE_URL=file://…`), verified against the committed `shell/checksums/v0.10.0.sha256`, installed and run (`install --refresh`). A tampered asset is rejected with a checksum mismatch and the installed binary is kept. |
| `zimfw install`, offline | pass | Same, inside a network namespace without network: the download fails and the cargo fallback builds `bin/cinelog`. |
| `doctor` holders | pass (sudo: fixture only) | stdin on a terminator pts: `holder "terminator"`; in a tmux pane: `holder "tmux: server"`; in a recorded shell: own recorder. `sudo` (use_pty) needs a real sudo prompt: covered by `tests/fixtures/proc/sudo-use-pty.txt` and by I5/I6 in the root CI job. |
| I18 (screen) | pass | with a locally unpacked GNU screen 5.0.2 on `PATH`. |
| aarch64 smoke run | open | only on the ARM runner of `release.yml`. |

## Manual list (needs root, a console or CI)

- I5 and the sudo part of I6 (`CINELOG_IT_ROOT=1`, `--features root`, real sudo prompt).
- `install --team` and `install --team --uninstall` without `--prefix`.
- A real VT login (`TERM=linux` on `/dev/ttyN`).
- The aarch64 release smoke test.
