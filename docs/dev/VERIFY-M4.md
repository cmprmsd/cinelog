# M4 verification record (v0.13, re-run with 1.0)

Plan §13 "M4 Accept". First verified against the 0.13 tree (58 mock / 51 real E2E specs),
re-run on 2026-11-08 against the 1.0 tree. Dev machine: a Linux desktop, Chromium
152 headless (`CINELOG_CHROMIUM=/usr/bin/chromium`). Every run used a temporary HOME/XDG
tree, a private `tmux -L` server and an ephemeral loopback port.

| Item | Result | How |
|---|---|---|
| 1. Live E2E: late joiner sees the current screen, the view is read-only, `offline` on a crash | pass | `pnpm -C web e2e` (contract mock, 60/60) runs every `live.spec.ts` case; against a real `cinelog serve` (`pnpm -C web e2e:real`, 53 passed, 7 mock-only live cases skipped) the late joiner case runs too. I23 drives the same over a real recorder: late joiner, output within 2 s, `offline` + `done` on a crash. |
| 2. One week of team data renders in under 500 ms | pass | `timeline.spec.ts` "renders one week of team data in under 500 ms" against the real server: 19950 blocks rendered in 55 ms. |
| 3. Annotations sync across two browsers within 2 s; survive a rebuild and a meta backup restore | pass | `annotations.spec.ts` "two browsers stay in sync within 2 s" (mock and real); I23 checks a second subscriber within 2 s, `index --rebuild` and a meta.db export/import. |
| 4. All three exports validated | pass | `export.spec.ts`: the `.cast` slice is standalone and plays in asciinema-player, `txt` holds the output, `html` is escaped and script-free; I23 checks the three exports server-side. |
| 5. Live sessions appear and disappear in the sidebar within 2 s | pass | `live.spec.ts` "a recorder crash shows offline; the sidebar drops and regains the session within 2 s" (mock); I23 on the UI bus with a real recorder. |
| Web checks | pass | `pnpm -C web check` (0 errors, 0 warnings), `lint`, `test` (161 tests), `build`. |

## Fixed after verification (1.0)

- `/s/<uuid>` of a shell that just started (the link `cinelog link` prints at once) showed
  "does not exist" until the indexer's first commit, about 2 s later. The session page now
  retries a 404 for up to 5 s (`retryNotFound`, `client.test.ts`).
