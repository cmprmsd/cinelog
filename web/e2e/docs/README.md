# Documentation screenshots

`pnpm docs:shots` (in `web/`) builds `dist/` and runs this Playwright project. It writes
`docs/screenshots/<view>-<light|dark>.png` for the README.

- **Data:** `dataset.ts` writes an invented workday as real casts: user `alice` on host
  `build01`, two tmux panes, a `sudo -i` root shell, an ssh login, a running session and an
  old-plugin cast. It contains no real recordings. The casts follow docs/PROTOCOL.md §5
  (session, cmd/done, pause/resume, skip, nested and multiplexer markers; the ssh login
  attaches to the tmux session of the panes).
- **Server:** `server.ts` starts a real `cinelog serve` in a throwaway HOME below
  `$CINELOG_E2E_TMP` (default the system temp dir). It indexes the casts once, creates a
  token and registers the live session with a stand-in recorder.
  The server binds an ephemeral loopback port. Legacy casts take the machine's host name,
  so the commands run in a private UTS namespace named `build01` (an unprivileged user
  namespace mapped back to your uid). Without user namespaces the run fails, unless
  `CINELOG_DOCS_REAL_HOSTNAME=1` accepts the real host name in the legacy session.
- **Setup:** `global-setup.ts` signs in through the login page, adds the annotations and
  tag colours, and stores the cookie in a temp file. The specs reuse it, because sessions
  are bound to the User-Agent.
- **Specs:**
  - `views.shots.ts` runs every view in the `light` and `dark` projects. The theme is both
    the browser's colour scheme and the stored `cinelog.theme`.
  - `dataset.test.ts` checks the data set; `server.test.ts` checks that the run refuses
    the real host name.
  - `docs.test.ts` checks that every relative link in README.md and docs/ resolves, and
    that every screenshot appears in the README in both themes.

Environment:

| Variable | |
|---|---|
| `CINELOG_E2E_BIN` | the binary (default `../target/debug/cinelog`, relative to `web/`) |
| `CINELOG_CHROMIUM` | a system Chromium, when Playwright's own is not installed |
| `CINELOG_E2E_TMP` | where the sandbox goes |
| `CINELOG_E2E_KEEP=1` | keeps the sandbox for inspection |
| `CINELOG_DOCS_REAL_HOSTNAME=1` | runs without a private host name; the legacy session then shows this machine's name (do not commit such screenshots) |

The main E2E config (`web/playwright.config.ts`) ignores this directory.
