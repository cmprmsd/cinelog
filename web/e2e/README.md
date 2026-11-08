# Web E2E (work package M3-C, plan §12.3)

`pnpm e2e` builds `dist/` and runs the Playwright specs in Chromium.

- **Default:** `global-setup.ts` starts `mock/server.ts`, a stand-in for `cinelog serve` written
  from docs/API.md and the generated types: HTTPS with a throwaway self-signed certificate
  (needs `openssl`), the server's CSP and security headers, Host/Origin/Content-Type checks,
  `__Host-cl-<port>` cookie sessions bound to the User-Agent, the login rate limit, and the tab 1
  endpoints (search, sessions, tree, detail, commands, cast, segmented index and segments,
  export, forget) over the fixtures in `mock/fixtures.ts`.
- **Real server:** `CINELOG_E2E_BIN=../target/debug/cinelog pnpm e2e` (or `pnpm e2e:real`) runs
  the specs against a real `cinelog serve`: `real/server.ts` writes the `FIX` sessions as casts
  (`real/casts.ts`: v3 casts with the recorder's markers, a 50 MB segmented session, a live
  session it keeps growing, an old-plugin cast with banners and `.commands.log`) into a
  throwaway HOME below `$CINELOG_E2E_TMP` (default the system temp dir), runs `index --once`,
  creates a token and starts `serve` on an ephemeral loopback port (TZ=UTC). Ids the server
  derives itself (the legacy uuid) reach the specs as `CINELOG_E2E_FIX`. `CINELOG_E2E_KEEP=1`
  keeps the sandbox. An already running server: `CINELOG_E2E_URL=https://127.0.0.1:<port>
  CINELOG_E2E_TOKEN=<token> pnpm e2e` (it must hold the same fixture sessions).
- **Browser:** `CINELOG_CHROMIUM=/usr/bin/chromium` uses a system Chromium when Playwright's own
  is not installed (`pnpm exec playwright install chromium`).

Some specs reshape responses with `page.route` instead of adding fixtures: cursor pages of a
session's hits (`n`/`N`), a failing next result page, and `PlayInfo.ready=false` build progress.
They run unchanged against a real server. The forget spec changes server state, so run it once
per fresh fixture set (a CI retry of that spec fails).

Known gap, marked `test.fail` on a real server: zsh echoes the command line before preexec,
so the real index keeps it in the unattributed chunk before the command, and a command forget
leaves it searchable ("a forgotten command is gone from search"; docs/API.md §5.6).

`auth.setup.ts` signs in once and stores the cookie in `e2e/.auth/` (git-ignored) for the
`chromium` project; `login.spec.ts` runs without it. Every test fails on a CSP violation report.

Standalone mock for manual checks: `node e2e/mock/server.ts [port]` (prints URL and token).
