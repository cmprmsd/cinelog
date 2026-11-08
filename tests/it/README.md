# cinelog integration tests (`cinelog-it`)

Scenarios from plan §12.2, run against the real binary, the real zsh plugin, tmux and PTYs.

```sh
cargo test -p cinelog-it                      # all scenarios that can run here
cargo test -p cinelog-it --test i07_typed_exact
cargo test --release -p cinelog-it -- --ignored --test-threads=1 bench_   # B1/B2 (nightly)
```

## Harness

Each scenario builds a `Sandbox` (`src/sandbox.rs`): a throwaway HOME/ZDOTDIR/XDG tree, a
`.zshrc` that sources a copy of `cinelog.plugin.zsh` (and `shell/`), and a `config.toml`
that sends all casts to `<sandbox>/logs`, switches server autostart off and silences the MOTD
extras. Panes run `env -i <sandbox env> zsh -i` in an isolated `tmux -L cinelog-it-<id> -f
/dev/null` server. Tests drive panes with `send-keys`, poll `capture-pane`, and read casts
(`src/cast.rs`, an independent reader written from docs/PROTOCOL.md) and registry files.

Dropping the sandbox kills every tmux server it registered and every process that carries its
environment marker `CLIT_SANDBOX=<id>` (not a `CINELOG_*` variable, so nothing strips it),
restores permissions and removes the tree. Sockets live in a short directory because of the
108-byte `sun_path` limit: `<sandbox>/s`, or, when the sandbox path is too long for that (for
example a deep `CINELOG_IT_TMPDIR`), a private `clit-<id>` below `CINELOG_IT_SOCKDIR`
(default `/tmp`) that is removed too.

Everything else the harness creates (sandboxes, the userns and sshd probe dirs, unit-test
fixtures) lives in `clit-*` directories below `CINELOG_IT_TMPDIR`. The only paths outside it
are that socket fallback and the root scenarios' prefix and staging dirs (see below).

`output_with_timeout` (I1, the recorder probe) runs the command in its own process group. It
fails, after killing the group, if the command outlives its timeout or if it exits while a
descendant still holds its stdout/stderr open (a background job the plugin left behind), so
such a leak fails the scenario instead of hanging it.

Scenarios that need a private mount (I8 full disk, I15) wrap the shell in `unshare -rm`
(tmpfs on the log dir) plus a nested `unshare -U --map-user=<uid>`, so the shell runs with the
real uid. The test reads those logs through `/proc/<pid>/root`.

## Prerequisites and gating

A scenario declares what it needs with `need!(…)`. A missing prerequisite prints a `SKIP` line
and passes, except:

- a **broken** component always fails: the `cinelog` build fails, or the recorder probe gets
  anything but a recording or the M1-0 stub's answer;
- with `CINELOG_IT_REQUIRE=1` (CI) every missing tool or component fails;
- the opt-in prerequisites (root, the password-sudo user) fail when `CINELOG_IT_ROOT=1` asks
  for them, or under `CINELOG_IT_REQUIRE=1` in a `--features root` build.

The recorder probe skips while `cinelog rec` is still the M1-0 fallback stub, and the plugin
probe skips while `cinelog.plugin.zsh` is the pre-rewrite plugin.

The sandbox `PATH` never contains a foreign `cinelog`: a directory that holds one (a system
install, a leftover of a team install) is replaced by a shadow directory of symlinks to its
other entries. "Missing binary" therefore means missing on any machine.

| Variable | Meaning |
|---|---|
| `CINELOG_IT_BIN`, `CINELOG_IT_BIN_TESTHOOKS` | Prebuilt binaries under test (the second with `--features testhooks`). Unset: the harness builds them once into `<target>/cinelog-it/`. |
| `CINELOG_IT_REQUIRE=1` | Missing prerequisites fail instead of skipping (opt-in scenarios excepted). |
| `CINELOG_IT_ROOT=1` + `--features root` | Enables the root scenarios (I5, the sudo part of I6). **Throwaway machines only.** |
| `CINELOG_IT_PW_USER`, `CINELOG_IT_PW_PASS` | A user with a password and a password-requiring sudo rule (I6 real sudo prompt). |
| `CINELOG_IT_TMPDIR` | Where sandboxes and probe dirs are created (default: the system temp dir). |
| `CINELOG_IT_SOCKDIR` | Parent of the socket-dir fallback when `<sandbox>/s` is too long for `sun_path` (default `/tmp`). |
| `CINELOG_IT_TIMEOUT_SCALE` | Multiplies every timeout (slow runners). |
| `CINELOG_IT_KEEP=1` | Keep sandbox trees for debugging. |
| `CINELOG_IT_B1_BASELINE_MS` | B1 baseline (default 990 ms, the [rec] spike). |

## Root scenarios (manual or CI only)

I5 runs `sudo -n cinelog install --team --system-zshrc --prefix <P> --binary <bin>
--allow-unverified --module <repo> --no-migrate` into a throwaway root-owned prefix
(`/tmp/cinelog-it-team.*`, plan §12.2), so `/usr/local` is never touched. Two system
locations cannot be avoided, because root's zsh only reads its compiled-in system zshrc and
TEAM paths at EUID 0 are fixed below `/` (§5.6):

- the prefix's managed block is appended to the real `/etc/zsh/zshrc` (or `/etc/zshrc`) inside
  a `# >>> cinelog-it <P> … >>>` wrapper; the byte-exact original is written back afterwards.
  Every write of the system zshrc is atomic (a temp file in the same directory with the
  original's owner and mode, renamed over it), so an interrupted run never leaves it
  truncated. While it is hooked, the original is also kept at `<zshrc>.cinelog-it.orig` for
  manual recovery (`mv` it back) after a run that was killed before it could restore it; a
  successful restore removes that copy;
- TEAM recorders write below `/root/logs`, `/run/cinelog` and `/var/lib/cinelog`; every path
  that appeared there during the test is removed afterwards, the prefix too.

It refuses to run when a real team install exists (a `cinelog` block in the system zshrc, or
`/usr/local/share/cinelog`). A wrapper left by a crashed run is removed once its prefix is
gone. One install exists at a time per test process, so the I5 tests are serialised even
under the default parallel test threads. The TEAM cast is identified by the user recorder's
`nested.child_uuid`, never by "the new file".

The sudo part of I6 uses only `sudo -u <pw user>` (to create and read that user's sandbox);
the password is typed into the pane and never put on a command line. That user's sandbox and
a world-readable staging copy of the binary and plugin live in `/tmp/clit-pw-<id>` and
`/tmp/clit-pwsrc-<id>` (another user cannot traverse a private `CINELOG_IT_TMPDIR`); both are
removed afterwards.

The runner needs passwordless sudo, zsh as root's login shell (`sudo -i` and `sudo su -`
start it) and an existing `/root/.zshrc` (otherwise zsh-newuser-install may prompt). Run them
only on throwaway machines:

```sh
CINELOG_IT_ROOT=1 CINELOG_IT_PW_USER=clpw CINELOG_IT_PW_PASS=… \
  cargo test -p cinelog-it --features root --test i05_sudo --test i06_secrets
```

## Scenarios

| File | Plan | What it asserts |
|---|---|---|
| `i01_guards.rs` | I1 | `zsh -i -c`, VS Code env resolution, `TERM=dumb`, `INSIDE_EMACS` (with and without a tty): fast, correct output, no cast, no registry entry |
| `i02_tmux_panes.rs` | I2 | a tmux server started from a recorded shell: one cast per pane with holder `tmux: server`, commands and output only in their pane's cast, no `CINELOG_*` in the server env |
| `i03_attach_kill.rs` | I3 | two recorded windows attached to one session; killing the first: `x`, recorder and shell gone, registry entry removed, no cross-writes |
| `i04_multiplexer_pause.rs` | I4 | `tmux attach`, `exec tmux`, `sh -c 'sleep 1; tmux a'`: `multiplexer` + `pause`, no `o`/`i` while paused, `resume{foreground}`, playback back on the primary screen |
| `i05_sudo.rs` | I5 (root) | `sudo -s`/`-i`/`su -`: TEAM cast under `/root/logs/<runner>/`, `sudo_user`, `nested` + `pause{nested}` + `resume{nested-end}`; `nested = "record"` |
| `i06_secrets.rs` | I6 | `read -s`, getpass, a raw-mode prompt, `ssh -tt` to a private sshd, (root) real sudo prompts accepted by sudo: no fragment of the secret (every 4-char window) in any cast's input or output stream or any sandbox file; ordinary input is recorded |
| `i07_typed_exact.rs` | I7 | byte-exact `typed` (escapes, backslashes, UTF-8, a 16 KiB heredoc), leading-space → `skip` only, forged markers ignored (bad nonce passed through, `-` stripped) and counted (`stats.bad_markers` = 4) |
| `i08_fallback.rs` | I8 | read-only log dir, missing binary, corrupt config, full disk: the shell comes up unrecorded (it is the pane process itself), exactly one warning, `CINELOG_FAILED` unset, no exec loop (pid and process count stable), no cast (for the full disk read through the pane's private tmpfs mount), reason in rec.log; the full disk is caught before forkpty (PROTOCOL §6.4) |
| `i09_hangup.rs` | I9 | kill-pane, SIGTERM, SIGHUP, exit with a disowned HUP-ignoring job (`x` = 3), kill-pane with a job: `x`, exit, registry cleanup |
| `i10_nested_zsh.rs` | I10 | nested zsh: `rec --check` says `recorded <uuid>`, one cast, distinct seqs, derived `parent_seq` of inner commands = the `zsh` command |
| `i15_enospc.rs` | I15 | log tmpfs fills up: shell works, registry `degraded`, one warning, `error{enospc}` marker, `cinelog resume` opens `_part2` with `continues` |
| `i16_clock.rs` | I16 | +1 h realtime step through the testhooks clock: `clock{step}` marker, boottime consistent, piecewise wall-time mapping correct before and after |
| `i18_screen.rs` | I18 | GNU screen: outer `pause{multiplexer}` (comm `screen`), the window shell has its own cast, resume after screen exits |
| `i20_ctx.rs` | I20 | `ctx` markers at start, after `rename-session` and after `break-pane` (30 s poll) |
| `i21_mirror.rs` | I21 | `#{pane_current_path}` follows `cd`, `#{pane_current_command}` and automatic-rename show the inner command, comm stays `cinelog` |
| `b_bench.rs` | B1, B2 | `#[ignore]`d nightly benchmarks; B2's baseline is the same `.zshrc` without the `source` line |
