# M5 verification record (1.0)

Plan §13 "M5 Accept" and the final security and completeness review. Run on 2026-11-08
against the 1.0 tree. Dev machine: a Linux desktop (zsh 5.9.2, tmux 3.7c, Chromium
152 headless). Every run used a temporary HOME/ZDOTDIR/XDG tree, a private `tmux -L` server
and an ephemeral loopback port.

The first M5 verification pass (commit a690948, "fix verification findings") found the
build, tests and E2E green and changed no tracked file; its open items were the M5 handoff
gaps that had no owner. They are fixed in the final pass below.

| Item | Result | How |
|---|---|---|
| 1. Release on a tag (aarch64 smoke run, checksum commit) | open (manual) | Needs a pushed tag: `release.yml` jobs build (x86_64 and native aarch64 smoke), publish, checksums, verify. docs/MANUAL-TESTS.md L1–L5. |
| 2. Upgrade 0.12 → 1.0 | pass | I24: verified atomic `--on-pull` swap, a shell recorded by the old binary keeps recording on its `(deleted)` exe, `doctor --skew` names the old server, `install --refresh` restarts the detached server on the new binary, `install --team --upgrade` in `--root` mode. The systemd restart of the team unit is manual (MANUAL-TESTS). |
| 2. Migrations; rollback | pass | Schemas are still v1 (no migration step). Rollback: I25 and `db::tests::a_newer_schema_is_read_only_and_reported` (below). |
| 3. `hist` removed; README, MIGRATING, PROTOCOL, API, SECURITY with screenshots | pass | `plugin_test.zsh` asserts the shim is gone; docs and `docs/screenshots/*` (`pnpm -C web docs:shots`, 33 views). |
| 4. Manual matrix complete | pass | docs/MANUAL-TESTS.md (root-only, real VT, aarch64, tag release). |
| Build and tests | pass | `cargo fmt --check`, `cargo clippy --workspace --all-targets --all-features -D warnings`, `cargo test --workspace --exclude cinelog-it` (825 passed), `cargo test -p cinelog-it` (107 passed, 0 failed), `zsh shell/fetch.test.zsh` (52, root cases in a user namespace), `zsh shell/tests/plugin_test.zsh` (37), `pnpm -C web check/lint/test/build`, E2E mock 60/60 and real 53 (+7 mock-only). |
| Dependency advisories | pass | `cargo audit` 0.22.2: 292 crates, 1422 advisories, no vulnerability, no warning (also `--deny unsound --deny yanked`). `pnpm audit --prod`: none. `pnpm audit` (dev tools): deepmerge (low) via the Svelte vite plugin and minimatch (moderate) via eslint, both build-time only. CI now runs both audits. |

## Fixed in the final pass

- **Pinned modules trusted the release's own checksums.** A module checked out at a tag
  never receives `shell/checksums/v<VERSION>.sha256`, so fetch.zsh verified the binary only
  against the `SHA256SUMS` served next to it. A detached checkout now refuses that and builds
  the pinned source (or keeps its binary) unless the checksum file is copied from `main` or
  `--trust-release-sums` is given (`t_pinned_checkout_refuses_release_sums`).
- **`cinelog archive-legacy --to DIR [--dry-run]`** (plan §6.6, §15.10) was missing: zstd
  copies of the legacy casts in the compressor's frame format, never next to the originals,
  which are read with `O_NOATIME` and never changed (unit tests, I13 step with sha256, mtime,
  atime and inode checks, real corpus in VERIFY-M2).
- **A growing legacy cast lost its forgets.** Its uuid hashes its size, so each import
  replaced the session and the forget rows keyed by the old uuid stopped applying. The writer
  now copies the forget rows and playback spans to the new uuid and moves the annotations,
  in meta.db, before the import (`a_forget_on_a_legacy_cast_survives_its_growth`).
- **A newer schema failed instead of opening read-only** (plan §10.3). Read-only opens now
  accept a newer `user_version` of a cinelog database; `status` and `/api/status` report it;
  `serve` runs the web server without the indexer, keeps logins in memory and writes to
  neither database; meta writes answer 503 (`newer_schema_opens_read_only_and_says_so`,
  `newer_meta_db_keeps_logins_in_memory`, I25).
- **New-session links 404ed for about 2 s** (VERIFY-M4).
- **No dependency audit** in CI: `cargo audit` and `pnpm audit --prod` jobs added.

## Manual list

- The tag release (`release.yml`), including the aarch64 smoke run and the checksum commit.
- `sudo cinelog install --team --upgrade` with an active `cinelog-team.service`, and
  `sudo CINELOG_IT_ROOT=1 packaging/verify-team-unit.sh`.
