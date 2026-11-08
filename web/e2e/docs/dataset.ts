// The demo data set behind the documentation screenshots: an invented workday of one user
// ("alice" on host "build01") written as real casts for `cinelog serve`. Everything here is
// synthetic; nothing is read from a real log directory.
//
// The casts follow docs/PROTOCOL.md §5: an asciicast v3 header, a `session` marker at t=0,
// and per command the keystrokes, zsh's echo of the line, a `cmd` marker, the output, a
// `done` marker and the next prompt. The legacy session is an old-plugin cast with
// `Begin`/`End` banners and its `.commands.log`.

import { mkdirSync, writeFileSync } from 'node:fs';
import { join } from 'node:path';

export type Ev = [t: number, code: string, data: string];

export interface DemoCommand {
  /** Session seconds. */
  at: number;
  /** Seconds; `null` while it still runs (live session). */
  dur: number | null;
  typed: string;
  cwd: string;
  exit?: number;
  out: string[];
}

export interface DemoMarker {
  t: number;
  k: 'pause' | 'resume' | 'skip' | 'nested';
  data: Record<string, unknown>;
}

export interface Tmux {
  session: string;
  window: string;
  windowName: string;
  pane: string;
}

export interface DemoSession {
  uuid: string;
  /** Epoch seconds. */
  start: number;
  /** Session seconds of the last event; `null` for the live session. */
  duration: number | null;
  user: string;
  euid: number;
  sudoUser: string | null;
  holder: string;
  tty: string;
  tmux: Tmux | null;
  parent: string | null;
  legacy: boolean;
  commands: DemoCommand[];
  markers: DemoMarker[];
}

export const DEMO = {
  host: 'build01',
  user: 'alice',
  /** tmux pane %1: the deploy work, with a pause, a hidden command and a sudo session. */
  deploy: '0196f3a2-4c00-7a11-8e00-00000000d001',
  /** The root shell opened with `sudo -i` from the deploy pane. */
  root: '0196f3a2-4c00-7a11-8e00-00000000d002',
  /** tmux pane %2: log digging. */
  logs: '0196f3a2-4c00-7a11-8e00-00000000d003',
  /** An ssh login without tmux. */
  ssh: '0196f3a2-4c00-7a11-8e00-00000000d004',
  /** tmux pane %4: still recording. */
  live: '0196f3a2-4c00-7a11-8e00-00000000d005',
  /** An old-plugin cast (the server derives its uuid from the file). */
  legacy: '0196f3a2-4c00-7a11-8e00-00000000d006',
  /** The deploy command the session screenshot shows and annotates. */
  deploySeq: 6,
  /** Session seconds the session screenshot opens at: the end of that command's output. */
  deployShotT: 2539,
  /** The search the search screenshot runs. */
  query: 'rollout',
} as const;

const SH = 31337;
const COLS = 120;
const ROWS = 32;

const green = (s: string) => `\x1b[32m${s}\x1b[0m`;
const red = (s: string) => `\x1b[31m${s}\x1b[0m`;
const yellow = (s: string) => `\x1b[33m${s}\x1b[0m`;
const bold = (s: string) => `\x1b[1m${s}\x1b[0m`;
const dim = (s: string) => `\x1b[2m${s}\x1b[0m`;

const round = (t: number) => Math.round(t * 1e6) / 1e6;

function tmux(window: string, windowName: string, pane: string): Tmux {
  return { session: 'ops', window, windowName, pane };
}

function session(over: Partial<DemoSession> & Pick<DemoSession, 'uuid' | 'start' | 'commands'>): DemoSession {
  const last = over.commands.at(-1);
  const end = last && last.dur !== null ? last.at + last.dur + 20 : null;
  return {
    duration: end,
    user: DEMO.user,
    euid: 1000,
    sudoUser: null,
    holder: 'tmux: server',
    tty: '/dev/pts/3',
    tmux: null,
    parent: null,
    legacy: false,
    markers: [],
    ...over,
  };
}

const PODS = [
  'NAME                          READY   STATUS    RESTARTS   AGE',
  'api-7d9c6b5f4-2xkqp           1/1     Running   0          41s',
  'api-7d9c6b5f4-8mzrt           1/1     Running   0          28s',
  'api-7d9c6b5f4-qj4vn           1/1     Running   0          15s',
  'worker-5f8b9c7d6-tn2wd        1/1     Running   2          3d4h',
];

/** The data set, with wall times relative to `now` (epoch seconds). */
export function buildDemo(now: number): DemoSession[] {
  const t0 = Math.floor(now);
  const h = 3600;

  const deploy = session({
    uuid: DEMO.deploy,
    start: t0 - 6.5 * h,
    tmux: tmux('0', 'deploy', '%1'),
    commands: [
      { at: 5, dur: 1, typed: 'cd ~/src/shop-api', cwd: '~', out: [] },
      { at: 12, dur: 2, typed: 'git pull --rebase', cwd: '~/src/shop-api', out: ['Updating 4be1c02..9f3e7aa', 'Fast-forward', ` src/orders.rs | 18 ${green('++++++++++++')}${red('------')}`, ' 1 file changed, 12 insertions(+), 6 deletions(-)'] },
      {
        at: 40,
        dur: 412,
        typed: 'cargo test --workspace',
        cwd: '~/src/shop-api',
        out: [`${green('   Compiling')} shop-api v2.4.0`, `${green('    Finished')} test profile in 6m 31s`, 'running 214 tests', `test result: ${green('ok')}. 214 passed; 0 failed; 0 ignored`],
      },
      {
        at: 1200,
        dur: 95,
        typed: 'make lint',
        cwd: '~/src/shop-api',
        exit: 2,
        out: [`${red('error')}: unused variable: \`retry\``, '  --> src/orders.rs:88:13', `${red('error')}: could not compile \`shop-api\` due to 1 previous error`, 'make: *** [Makefile:14: lint] Error 101'],
      },
      { at: 1500, dur: 70, typed: 'make lint', cwd: '~/src/shop-api', out: [`${green('    Finished')} lint in 1m 08s`, 'no warnings'] },
      {
        at: 2400,
        dur: 140,
        typed: 'kubectl -n shop rollout restart deploy/api && kubectl -n shop rollout status deploy/api',
        cwd: '~/src/shop-api',
        out: [
          'deployment.apps/api restarted',
          'Waiting for deployment "api" rollout to finish: 1 of 3 updated replicas are available...',
          'Waiting for deployment "api" rollout to finish: 2 of 3 updated replicas are available...',
          `deployment "api" successfully ${green('rolled out')}`,
        ],
      },
      { at: 2560, dur: 2, typed: 'kubectl -n shop get pods', cwd: '~/src/shop-api', out: PODS },
      {
        at: 2600,
        dur: 1,
        typed: 'curl -s https://shop.internal/healthz | jq .',
        cwd: '~/src/shop-api',
        out: ['{', `  ${bold('"status"')}: ${green('"ok"')},`, `  ${bold('"version"')}: ${green('"2.4.0"')},`, `  ${bold('"db"')}: ${green('"connected"')}`, '}'],
      },
      { at: 7300, dur: 1400, typed: 'sudo -i', cwd: '~/src/shop-api', out: [] },
      { at: 9100, dur: 30, typed: 'git log --oneline -5', cwd: '~/src/shop-api', out: [`${yellow('9f3e7aa')} orders: retry payment capture once`, `${yellow('4be1c02')} api: expose build version in /healthz`, `${yellow('1c9a7e0')} ci: cache cargo registry`, `${yellow('77d0b13')} orders: index by customer id`, `${yellow('0a51f2c')} release 2.3.9`] },
      { at: 15000, dur: 4, typed: 'git tag -s v2.4.0 -m "release 2.4.0" && git push --tags', cwd: '~/src/shop-api', out: ['To git.internal:shop/shop-api.git', ` * [new tag]         ${green('v2.4.0')} -> v2.4.0`] },
      { at: 21000, dur: 2, typed: 'df -h /var/lib/docker', cwd: '~/src/shop-api', out: ['Filesystem      Size  Used Avail Use% Mounted on', '/dev/nvme1n1p1  492G  371G  121G  76% /var/lib/docker'] },
    ],
    markers: [
      { t: 3000, k: 'skip', data: { sh: SH } },
      { t: 3600, k: 'pause', data: { reason: 'user', detail: 'lunch' } },
      { t: 6900, k: 'resume', data: { reason: 'user' } },
      { t: 7303, k: 'nested', data: { child_uuid: DEMO.root, scope: 'team', user: 'root', sudo_user: DEMO.user, pid: 41720, starttime: 991200, mode: 'pause' } },
      { t: 7303, k: 'pause', data: { reason: 'nested', detail: 'root' } },
      { t: 8699.9, k: 'resume', data: { reason: 'nested-end' } },
    ],
  });

  const root = session({
    uuid: DEMO.root,
    start: deploy.start + 7303,
    user: 'root',
    euid: 0,
    sudoUser: DEMO.user,
    holder: 'sudo',
    tty: '/dev/pts/9',
    tmux: tmux('0', 'deploy', '%1'),
    parent: DEMO.deploy,
    commands: [
      { at: 4, dur: 1, typed: 'systemctl status nginx --no-pager', cwd: '/root', out: [`${green('●')} nginx.service - A high performance web server`, `     Active: ${green('active (running)')} since Mon 2026-11-02 08:14:03 CET; 6 days ago`, '   Main PID: 1123 (nginx)'] },
      { at: 60, dur: 3, typed: 'nginx -t && systemctl reload nginx', cwd: '/root', out: ['nginx: the configuration file /etc/nginx/nginx.conf syntax is ok', 'nginx: configuration file /etc/nginx/nginx.conf test is successful'] },
      { at: 1200, dur: 2, typed: 'journalctl -u nginx -n 3 --no-pager', cwd: '/root', out: [dim('Nov 08 10:41:17 build01 systemd[1]: Reloading nginx.service...'), dim('Nov 08 10:41:17 build01 systemd[1]: Reloaded nginx.service.')] },
    ],
  });

  const logs = session({
    uuid: DEMO.logs,
    start: t0 - 5.2 * h,
    tty: '/dev/pts/4',
    tmux: tmux('1', 'logs', '%2'),
    commands: [
      { at: 3, dur: 2, typed: 'cd /var/log/shop', cwd: '~', out: [] },
      { at: 30, dur: 4, typed: 'grep -c "payment capture failed" api.log', cwd: '/var/log/shop', out: ['37'] },
      { at: 90, dur: 6, typed: 'grep "payment capture failed" api.log | tail -3', cwd: '/var/log/shop', out: [`2026-11-08T09:12:44Z ${red('ERROR')} payment capture failed order=88412 err=timeout`, `2026-11-08T09:12:51Z ${red('ERROR')} payment capture failed order=88415 err=timeout`, `2026-11-08T09:13:02Z ${red('ERROR')} payment capture failed order=88419 err=timeout`] },
      { at: 2700, dur: 600, typed: 'tail -f api.log | grep --line-buffered rollout', cwd: '/var/log/shop', exit: 130, out: [`2026-11-08T10:02:10Z ${green('INFO')} rollout api-7d9c6b5f4 started`, `2026-11-08T10:04:30Z ${green('INFO')} rollout api-7d9c6b5f4 complete`, '^C'] },
      { at: 9600, dur: 20, typed: 'zstdcat archive/api-2026-11-07.log.zst | grep -c ERROR', cwd: '/var/log/shop', exit: 0, out: ['112'] },
    ],
  });

  const ssh = session({
    uuid: DEMO.ssh,
    start: t0 - 2.6 * h,
    holder: 'sshd-session',
    tty: '/dev/pts/7',
    commands: [
      { at: 2, dur: 1, typed: 'uptime', cwd: '~', out: [' 11:58:21 up 6 days,  3:44,  2 users,  load average: 0.42, 0.51, 0.47'] },
      { at: 15, dur: 2, typed: 'free -h', cwd: '~', out: ['               total        used        free      shared  buff/cache   available', 'Mem:            62Gi        21Gi        18Gi       1.2Gi        24Gi        41Gi'] },
      { at: 600, dur: 180, typed: 'rsync -a --info=progress2 backups/ nas:/volume1/backups/build01/', cwd: '~', out: ['  9,881,344,512 100%  54.12MB/s    0:02:54 (xfr#212, to-chk=0/1880)'] },
    ],
  });

  const live = session({
    uuid: DEMO.live,
    start: t0 - 25 * 60,
    tty: '/dev/pts/11',
    tmux: tmux('2', 'tests', '%4'),
    commands: [
      { at: 4, dur: 1, typed: 'cd ~/src/shop-api', cwd: '~', out: [] },
      { at: 20, dur: 3, typed: 'git switch -c fix/payment-retry', cwd: '~/src/shop-api', out: ["Switched to a new branch 'fix/payment-retry'"] },
      {
        at: 60,
        dur: null,
        typed: 'cargo test -p orders -- --nocapture',
        cwd: '~/src/shop-api',
        out: [
          `${green('   Compiling')} orders v2.4.1 (/home/alice/src/shop-api/orders)`,
          `${green('    Finished')} test profile [unoptimized + debuginfo] target(s) in 38.20s`,
          `${green('     Running')} unittests src/lib.rs (target/debug/deps/orders-3f1c)`,
          '',
          'running 48 tests',
          `test capture::retries_once_on_timeout ... ${green('ok')}`,
          `test capture::gives_up_after_second_timeout ... ${green('ok')}`,
          `test capture::idempotency_key_is_stable ... ${green('ok')}`,
          `test refunds::partial_refund_keeps_order_open ... ${green('ok')}`,
          'test integration::checkout_against_sandbox has been running for over 60 seconds',
        ],
      },
    ],
  });

  const legacy = session({
    uuid: DEMO.legacy,
    start: t0 - 12 * 24 * h,
    tty: '/dev/pts/2',
    holder: 'terminator',
    legacy: true,
    commands: [
      { at: 3, dur: 2, typed: 'uptime', cwd: '~', out: [' 09:01:12 up 2 days, 22:31,  1 user,  load average: 0.08, 0.12, 0.09'] },
      { at: 20, dur: 30, typed: 'make release', cwd: '~/src/shop-api', out: ['cargo build --release', `${green('    Finished')} release profile in 2m 41s`] },
    ],
  });

  return [deploy, root, logs, ssh, live, legacy];
}

function prompt(s: DemoSession, cwd: string): string {
  const sign = s.euid === 0 ? '#' : '$';
  const who = s.euid === 0 ? `\x1b[1;31m${s.user}@${DEMO.host}\x1b[0m` : `\x1b[1;32m${s.user}@${DEMO.host}\x1b[0m`;
  return `${who}:\x1b[1;34m${cwd}\x1b[0m${sign} `;
}

function marker(k: string, fields: Record<string, unknown>): string {
  return JSON.stringify({ cl: 1, k, ...fields });
}

function sessionMarker(s: DemoSession): string {
  const t = s.tmux;
  return marker('session', {
    uuid: s.uuid,
    part: 1,
    start_epoch_us: Math.round(s.start * 1e6),
    tz: 'Europe/Berlin',
    tz_offset_s: 3600,
    scope: 'personal',
    host: DEMO.host,
    user: s.user,
    uid: s.euid,
    euid: s.euid,
    sudo_user: s.sudoUser,
    attribution: 'ancestor',
    tty: s.tty,
    holder_comm: s.holder,
    tmux: t ? { socket: '/tmp/tmux-1000/default', server_pid: 2207, server_start: 4410, session_id: '$0', pane: t.pane } : null,
    parent_uuid: s.parent,
    input: true,
    rec_version: '1.0.0',
    protocol: 1,
  });
}

/** Output lines spread over the command's run time (all at once for short commands). */
function outputEvents(c: DemoCommand, until: number): Ev[] {
  if (c.out.length === 0) return [];
  const from = c.at + 0.2;
  const span = Math.max(0, until - 0.2 - from);
  if (span < 2) return [[round(from), 'o', `${c.out.join('\r\n')}\r\n`]];
  return c.out.map((line, i) => [round(from + (span * i) / c.out.length), 'o', `${line}\r\n`]);
}

/** All events of a cinelog cast, sorted by time. `now` bounds the running command (session seconds). */
export function castEvents(s: DemoSession, nowT: number): Ev[] {
  const ev: Ev[] = [[0, 'm', sessionMarker(s)]];
  if (s.tmux) {
    ev.push([0, 'm', marker('ctx', { session_name: s.tmux.session, window_index: s.tmux.window, window_name: s.tmux.windowName, pane_index: '0', pane_id: s.tmux.pane })]);
  }
  ev.push([0.05, 'o', prompt(s, s.commands[0]?.cwd ?? '~')]);
  s.commands.forEach((c, i) => {
    const seq = i + 1;
    const keys = [...c.typed, '\r'];
    const typing = c.at - 0.03 * keys.length - 0.1;
    keys.forEach((k, j) => ev.push([round(typing + j * 0.03), 'i', k]));
    ev.push([round(c.at - 0.01), 'o', `${c.typed}\r\n`]);
    ev.push([c.at, 'm', marker('cmd', { seq, sh: SH, n: seq, typed: c.typed, cwd: c.cwd, user: s.user, euid: s.euid, host: DEMO.host, pane: s.tmux?.pane ?? '' })]);
    if (c.dur === null) {
      ev.push(...outputEvents(c, Math.max(c.at + 2, nowT - 5)));
      return;
    }
    const end = c.at + c.dur;
    ev.push(...outputEvents(c, end));
    ev.push([end, 'm', marker('done', { seq, sh: SH, n: seq, exit: c.exit ?? 0, pipestatus: [c.exit ?? 0], cwd: c.cwd })]);
    ev.push([round(end + 0.01), 'o', prompt(s, c.cwd)]);
  });
  for (const m of s.markers) {
    if (m.k === 'skip') ev.push([round(m.t - 0.01), 'o', '\r\n']);
    ev.push([m.t, 'm', marker(m.k, m.data)]);
  }
  ev.sort((a, b) => a[0] - b[0]);
  return ev;
}

/** v3 event lines: intervals since the previous event (at `from`). */
export function eventLines(ev: Ev[], from = 0): string {
  let prev = from;
  let out = '';
  for (const [t, code, data] of ev) {
    out += `[${Math.max(0, t - prev).toFixed(6)}, ${JSON.stringify(code)}, ${JSON.stringify(data)}]\n`;
    prev = Math.max(prev, t);
  }
  return out;
}

function header(s: DemoSession): string {
  return JSON.stringify({
    version: 3,
    term: { cols: COLS, rows: ROWS, type: 'xterm-256color' },
    timestamp: Math.floor(s.start),
    env: { SHELL: '/bin/zsh', TERM: 'xterm-256color' },
  });
}

/** The old plugin's right-aligned banner (`End` is followed by three spaces). */
function banner(kind: 'Begin' | 'End  ', epoch: number): string {
  const d = new Date(Math.floor(epoch) * 1000).toISOString();
  return `${`${kind} [${d.slice(0, 10)} ${d.slice(11, 19)}]`.padStart(COLS)}\r\n`;
}

/** An old-plugin cast (no `session` marker) and its commands.log; the server runs with TZ=UTC. */
export function legacyCast(s: DemoSession): { cast: string; log: string } {
  const ev: Ev[] = [[0.1, 'o', prompt(s, s.commands[0]!.cwd)]];
  let log = '';
  for (const c of s.commands) {
    const end = c.at + (c.dur ?? 0);
    ev.push([c.at - 0.3, 'o', `${c.typed}\r\n`]);
    ev.push([c.at, 'o', banner('Begin', s.start + c.at)]);
    ev.push([c.at + 0.4, 'o', `${c.out.join('\r\n')}\r\n`]);
    ev.push([end, 'o', banner('End  ', s.start + end) + prompt(s, c.cwd)]);
    log += `[${c.at}, "m", "${c.typed}"]\n`;
  }
  ev.push([s.duration ?? 0, 'x', '0']);
  const head = JSON.stringify({ version: 3, term: { cols: COLS, rows: ROWS, type: 'xterm-256color' }, timestamp: Math.floor(s.start) });
  return { cast: `${head}\n${eventLines(ev)}`, log };
}

function stamp(epoch: number): string {
  return new Date(epoch * 1000).toISOString().replace(/[-:]/g, '').replace(/\.(\d{3})Z$/, '$1000');
}

export interface LiveCast {
  path: string;
  start: number;
}

/** Writes the data set below `logs` (the server's `paths.logs`) and returns the live cast. */
export function writeDemo(logs: string, now: number): LiveCast {
  let live: LiveCast | null = null;
  for (const s of buildDemo(now)) {
    if (s.legacy) {
      const { cast, log } = legacyCast(s);
      mkdirSync(logs, { recursive: true, mode: 0o700 });
      const path = join(logs, `${DEMO.host}_${stamp(s.start).slice(0, 15)}.cast`);
      writeFileSync(path, cast, { mode: 0o600 });
      writeFileSync(`${path}.commands.log`, log, { mode: 0o600 });
      continue;
    }
    const dir = join(logs, new Date(s.start * 1000).toISOString().slice(0, 7));
    mkdirSync(dir, { recursive: true, mode: 0o700 });
    const path = join(dir, `${DEMO.host}_${stamp(s.start)}_${s.uuid.slice(-8)}.cast`);
    const ev = castEvents(s, now - s.start);
    if (s.duration === null) {
      writeFileSync(path, `${header(s)}\n${eventLines(ev)}`, { mode: 0o600 });
      live = { path, start: s.start };
      continue;
    }
    ev.push([s.duration, 'x', '0']);
    writeFileSync(path, `${header(s)}\n${eventLines(ev)}`, { mode: 0o600 });
  }
  if (!live) throw new Error('the demo data set has no live session');
  return live;
}
