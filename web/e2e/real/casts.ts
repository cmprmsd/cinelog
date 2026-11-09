// Writes the E2E data set (`FIX`, `../mock/fixtures.ts`) as real casts for `cinelog serve`:
// asciicast v3 files with the recorder's markers (docs/PROTOCOL.md §5) for the cinelog
// sessions, and an old-plugin cast with `Begin`/`End` banners plus its `.commands.log` for the
// legacy one. The content matches what the contract mock serves, so the specs run unchanged.

import { mkdirSync, writeFileSync, appendFileSync } from 'node:fs';
import { join } from 'node:path';
import { buildFixtures, FIX, OUTPUT_DELAY, type FixCommand, type FixSession } from '../mock/fixtures.ts';

/** Bytes of the segmented session (plan §12.3: a 50 MB fixture). */
const SEGMENTED_BYTES = 50 * 1024 * 1024;
/** Invisible filler for the segmented session: it must not scroll the screen. */
const FILLER = '\x1b[0m';
const SH = 4242;

type Ev = [t: number, code: string, data: string];

export interface WrittenFixtures {
  /** The cast of the live session, which the caller keeps growing. */
  livePath: string;
  /** Session start (epoch seconds) of the live session. */
  liveStart: number;
  /** Time of the live cast's last event (session seconds). */
  liveLastT: number;
}

function marker(k: string, fields: Record<string, unknown>): string {
  return JSON.stringify({ cl: 1, k, ...fields });
}

function sessionMarker(f: FixSession): string {
  const s = f.session;
  const tmux = s.tmux.pane ? { socket: '/tmp/tmux-1000/default', server_pid: 4242, server_start: 991, session_id: s.tmux.session_id, pane: s.tmux.pane } : null;
  return marker('session', {
    uuid: s.uuid,
    part: 1,
    start_epoch_us: Math.round(s.started_at * 1e6),
    tz: s.tz,
    tz_offset_s: 3600,
    scope: s.scope,
    host: s.host,
    user: s.user,
    uid: s.euid ?? 1000,
    euid: s.euid ?? 1000,
    sudo_user: s.sudo_user,
    attribution: s.attribution,
    tty: s.tty,
    holder_comm: s.holder_comm,
    tmux,
    parent_uuid: s.parent_uuid,
    input: true,
    rec_version: '0.12.0',
    protocol: 1,
  });
}

function prompt(f: FixSession, cwd: string): string {
  return `\x1b[1;32m${f.session.user}@${f.session.host}\x1b[0m:\x1b[1;34m${cwd}\x1b[0m$ `;
}

/** The events of a cinelog cast, as the mock's `sessionEvents` lays them out. */
function cinelogEvents(f: FixSession): Ev[] {
  const ev: Ev[] = [[0, 'm', sessionMarker(f)]];
  const t = f.session.tmux;
  if (t.pane) {
    ev.push([0, 'm', marker('ctx', { session_name: t.session, window_index: t.window, window_name: t.window_name, pane_index: '0', pane_id: t.pane })]);
  }
  ev.push([0, 'o', prompt(f, f.commands[0]?.cwd ?? '~')]);
  f.commands.forEach((c: FixCommand, i: number) => {
    const n = i + 1;
    const keys = [...c.typed, '\r'];
    const t0 = Math.max(ev.at(-1)![0], c.t_start - 0.02 * keys.length - 0.05);
    keys.forEach((k, j) => ev.push([round(t0 + j * 0.02), 'i', k]));
    // zsh echoes the line and moves to the next row before preexec sends the `cmd` marker.
    ev.push([round(c.t_start - 0.01), 'o', `${c.typed}\r\n`]);
    ev.push([c.t_start, 'm', marker('cmd', { seq: n, sh: SH, n, typed: c.typed, cwd: c.cwd, user: f.session.user, euid: f.session.euid, host: f.session.host, pane: t.pane })]);
    ev.push([round(c.t_start + OUTPUT_DELAY), 'o', `${c.output.join('\r\n')}\r\n`]);
    ev.push([c.t_end, 'm', marker('done', { seq: n, sh: SH, n, exit: c.exit ?? 0, pipestatus: [c.exit ?? 0], cwd: c.cwd })]);
    ev.push([c.t_end, 'o', prompt(f, c.cwd)]);
  });
  for (const m of f.markers) {
    // The child is served by this same server, so it has this session's scope (a `team`
    // child would be linked to the TEAM server instead).
    const data = m.kind === 'nested' ? { scope: f.session.scope, pid: 5151, starttime: 77, ...m.data, mode: 'record' } : m.data;
    ev.push([m.t, 'm', marker(m.kind, data)]);
  }
  ev.sort((a, b) => a[0] - b[0]);
  return ev;
}

/** Spreads invisible output over the session so it exceeds `bytes` (segmented playback). */
function withFiller(ev: Ev[], duration: number, bytes: number): Ev[] {
  const chunk = FILLER.repeat(16 * 1024);
  const n = Math.ceil(bytes / chunk.length);
  const out = [...ev];
  for (let i = 0; i < n; i++) out.push([round(((i + 0.5) / n) * duration), 'o', chunk]);
  out.sort((a, b) => a[0] - b[0]);
  return out;
}

function v3(f: FixSession, ev: Ev[]): string {
  const head = JSON.stringify({
    version: 3,
    term: { cols: f.session.cols, rows: f.session.rows, type: f.session.term },
    timestamp: Math.floor(f.session.started_at),
    env: { SHELL: '/bin/zsh', TERM: f.session.term },
  });
  return `${head}\n${lines(ev)}`;
}

/** v3 event lines: intervals since the previous event (at `from`). */
function lines(ev: Ev[], from = 0): string {
  let prev = from;
  let s = '';
  for (const [t, code, data] of ev) {
    s += `[${Math.max(0, t - prev).toFixed(6)}, ${JSON.stringify(code)}, ${JSON.stringify(data)}]\n`;
    prev = t;
  }
  return s;
}

/** The old plugin's banners: `printf '%*s\n' $COLUMNS "Begin [<local time>]"` (UTC here). */
function banner(kind: 'Begin' | 'End  ', epoch: number, cols: number): string {
  const d = new Date(Math.floor(epoch) * 1000).toISOString();
  const text = `${kind} [${d.slice(0, 10)} ${d.slice(11, 19)}]`;
  return `${text.padStart(cols)}\r\n`;
}

/** An old-plugin cast (no `session` marker) and its commands.log; the server runs with TZ=UTC. */
function legacyCast(f: FixSession): { cast: string; log: string } {
  const start = Math.floor(f.session.started_at);
  const cols = f.session.cols;
  const ev: Ev[] = [[0.1, 'o', prompt(f, f.commands[0]!.cwd)]];
  let log = '';
  for (const c of f.commands) {
    ev.push([c.t_start - 0.3, 'o', `${c.typed}\r\n`]);
    ev.push([c.t_start, 'o', banner('Begin', start + c.t_start, cols)]);
    ev.push([round(c.t_start + OUTPUT_DELAY), 'o', `${c.output.join('\r\n')}\r\n`]);
    ev.push([c.t_end, 'o', banner('End  ', start + c.t_end, cols) + prompt(f, c.cwd)]);
    log += `[${c.t_start}, "m", "${c.typed}"]\n`;
  }
  ev.push([f.duration, 'x', '0']);
  const head = JSON.stringify({ version: 3, term: { cols, rows: f.session.rows, type: 'xterm-256color' }, timestamp: start });
  return { cast: `${head}\n${lines(ev)}`, log };
}

function stamp(epoch: number): string {
  return new Date(epoch * 1000).toISOString().replace(/[-:]/g, '').replace(/\.(\d{3})Z$/, '$1000');
}

/** Writes every fixture session below `logs` (the server's `paths.logs`). */
export function writeFixtures(logs: string, now: number): WrittenFixtures {
  let written: WrittenFixtures | null = null;
  for (const f of buildFixtures(now)) {
    const s = f.session;
    const month = new Date(s.started_at * 1000).toISOString().slice(0, 7);
    const dir = join(logs, month);
    mkdirSync(dir, { recursive: true, mode: 0o700 });
    if (s.legacy) {
      const { cast, log } = legacyCast(f);
      const path = join(logs, `${s.host}_legacy_${s.uuid.slice(-8)}.cast`);
      writeFileSync(path, cast, { mode: 0o600 });
      writeFileSync(`${path}.commands.log`, log, { mode: 0o600 });
      continue;
    }
    let ev = cinelogEvents(f);
    if (s.uuid === FIX.segmented) ev = withFiller(ev, f.duration, SEGMENTED_BYTES);
    const path = join(dir, `${s.host}_${stamp(s.started_at)}_${s.uuid.slice(-8)}.cast`);
    if (s.state === 'live') {
      // No exit event: the session stays live while the caller keeps the file growing.
      writeFileSync(path, v3(f, ev), { mode: 0o600 });
      written = { livePath: path, liveStart: s.started_at, liveLastT: ev.at(-1)![0] };
      continue;
    }
    ev.push([f.duration, 'o', '']);
    ev.push([f.duration, 'x', String(s.exit_status ?? 0)]);
    writeFileSync(path, v3(f, ev), { mode: 0o600 });
  }
  if (!written) throw new Error('the fixture set has no live session');
  return written;
}

/** Appends an empty output event to the live cast at `now` (epoch seconds). */
export function touchLive(w: WrittenFixtures, now: number): void {
  const t = Math.max(w.liveLastT, round(now - w.liveStart));
  appendFileSync(w.livePath, lines([[t, 'o', '']], w.liveLastT));
  w.liveLastT = t;
}

const round = (t: number) => Math.round(t * 1e6) / 1e6;
