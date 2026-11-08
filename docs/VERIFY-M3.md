# M3 verification record (v0.12)

Plan §13 "M3 Accept" and Appendix B (M3) items. Run on the dev machine (Arch Linux, kernel 6.18,
zsh 5.9.2, tmux 3.7c, curl 8.22 with nghttp2, Chromium 152 headless) on 2026-11-08 against this
tree. Every run used a temporary HOME/XDG tree, a private `tmux -L` server and an ephemeral
loopback port.

| Item | Result | How |
|---|---|---|
| 1. E2E: login, search, player, chapters, segmented playback | pass | `pnpm -C web e2e` (contract mock): 26/26. `CINELOG_E2E_BIN=target/debug/cinelog pnpm -C web exec playwright test` (a real `cinelog serve` over the fixture casts of `web/e2e/real/`, including a 50 MB segmented cast and a legacy cast with banners): 26/26, including "a forgotten command is gone from search" (a command forget also strips the echoed command line from the index, docs/API.md §5.6). Every spec fails on a CSP violation report. |
| 1. Request matrix over HTTP/1.1 and HTTP/2 | pass | I22 (`tests/it/tests/i22_web.rs`, curl `--http1.1` and `--http2` against `cinelog serve`): no cookie 401, bad Host 400, Origin of another port 403, cookie replayed with another User-Agent 401 and its session ended. In-process: `auth::tests::over_tls::request_matrix_over_h1_and_h2`. |
| 2. 517 MB legacy cast | pass | Release `cinelog serve` on a copy of the real-corpus index (casts only read): first open (detail, `/play` until ready, index, segment 0) 0.45 s, cached open 0.16 s, worst segment fetch for a seek 0.047 s over 523 segments. `real_cast_bench`: plan from indexed segments 0.14 s, fully on demand (no indexed segments) 1.28 s; all segments pass the player's checks and join up. |
| 3. Team server under the hardened unit, two named tokens | partly (root half manual) | `systemd-analyze security --offline` on `packaging/systemd/cinelog-team.service`: exposure 2.1 OK. Non-root half: `token::tests::two_members_share_the_real_team_server` (real team server over an indexed cast: two tokens, same sessions, search and status; revoke ends only that member's sessions). Root half: `sudo CINELOG_IT_ROOT=1 packaging/verify-team-unit.sh` after `sudo cinelog install --team`, not run here (no root). |
| 4. `cinelog link` URL and the title option | pass | I22: a shell recorded while the server runs prints `url https://…/s/<uuid>` from `cinelog link`, the tmux pane title carries `[<url>]` with `shell.title_link = true`, and the URL serves the SPA; the session's detail and the typed command are reachable over the API. |
| 5. Both themes, keyboard-only flow, CSP with the player | pass | `ui.spec.ts` (theme toggle persists, both themes render the player; keyboard path `/`, `j`/`k`, Enter, Space, `]`, `?`, Esc) against the mock and the real server; no CSP report in any spec (cast and segmented modes, `'wasm-unsafe-eval'`). |
| HTTP/2 `:authority` host check | pass (curl) | I22 sends a foreign `Host` over `--http2` (curl maps it to `:authority`): 400. |
| DECRSPS in the player's bundled VT | open | not checked in a browser. |

## Fixed after verification

- A command forget left the echoed command line searchable (zsh
  echoes it before preexec, into the output before the `cmd` marker). The purge, a rebuild
  and queries before the purge now empty those lines (`db::tests::command_forget_removes_the_echo`).

## Manual list (needs root)

- `sudo cinelog install --team`, then `sudo CINELOG_IT_ROOT=1 packaging/verify-team-unit.sh`
  (hardened unit paths with an empty capability set, two members, revoke, restart).
