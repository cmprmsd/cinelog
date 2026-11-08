// The E2E data set: a few synthetic sessions that exercise search, highlighting, chapters,
// recorder markers, nesting, legacy badges, segmented playback and forget. Specs refer to
// these through `FIX`, so a real-server fixture only has to reproduce the same content.

import type { Command, Marker, Session, SessionState, TmuxLocation } from '../../src/lib/api/types.ts';

export interface FixCommand {
  seq: number;
  t_start: number;
  t_end: number;
  exit: number | null;
  typed: string;
  cwd: string;
  /** Output lines after the command line. */
  output: string[];
  approx?: boolean;
  confidence?: number;
}

export interface FixSession {
  session: Session;
  commands: FixCommand[];
  markers: Marker[];
  /** `segmented`: served as a segmented index with these segment starts. */
  segments: number[] | null;
  duration: number;
}

export const FIX = {
  host: 'lab1',
  needle: '0b9e4c1a-7d2e-4f00-9a51-0000000000a1',
  legacy: '0b9e4c1a-7d2e-4f00-9a51-0000000000a2',
  nested: '0b9e4c1a-7d2e-4f00-9a51-0000000000a3',
  segmented: '0b9e4c1a-7d2e-4f00-9a51-0000000000a4',
  live: '0b9e4c1a-7d2e-4f00-9a51-0000000000a5',
  forget: '0b9e4c1a-7d2e-4f00-9a51-0000000000a6',
  /** The markup that must render as text. */
  evil: '<img src=x onerror="document.title=\'pwned\'">',
  forgetWord: 'forgetmetoken',
  // A real server derives some ids itself (the legacy uuid comes from the file); the global
  // setup passes those as `CINELOG_E2E_FIX`.
  ...(JSON.parse(globalThis.process?.env.CINELOG_E2E_FIX ?? '{}') as Record<string, string>),
} as const;

const NO_TMUX: TmuxLocation = { server: null, session_id: null, session: null, window: null, window_name: null, pane: null };

function tmux(pane: string, window: string, windowName: string): TmuxLocation {
  return { server: '4242:991', session_id: '$0', session: 'work', window, window_name: windowName, pane };
}

function session(uuid: string, startedAt: number, over: Partial<Session>): Session {
  return {
    uuid,
    host: FIX.host,
    scope: 'personal',
    user: 'alice',
    euid: 1000,
    sudo_user: null,
    claimed_user: null,
    attribution: 'ancestor',
    login_via: null,
    tty: '/dev/pts/3',
    holder_comm: 'tmux: server',
    tmux: NO_TMUX,
    parent_uuid: null,
    parts: 1,
    compressed: false,
    cast_bytes: 4096,
    started_at: startedAt,
    ended_at: startedAt + 600,
    duration: null,
    exit_status: 0,
    tz: 'Europe/Berlin',
    cols: 80,
    rows: 24,
    term: 'xterm-256color',
    legacy: false,
    hidden: false,
    state: 'ended' as SessionState,
    commands: 0,
    ...over,
  };
}

function cmds(list: Omit<FixCommand, 'seq'>[]): FixCommand[] {
  return list.map((c, i) => ({ seq: i + 1, ...c }));
}

function marker(t: number, startedAt: number, kind: string, data: Record<string, unknown> = {}): Marker {
  return { t, ts: startedAt + t, kind, data };
}

/** The data set, with wall times relative to `now` (epoch seconds). */
export function buildFixtures(now: number): FixSession[] {
  const base = Math.floor(now) - 3600;

  const needleCmds = cmds([
    { t_start: 1, t_end: 2, exit: 0, typed: 'echo hello', cwd: '/srv', output: ['hello'] },
    { t_start: 4, t_end: 5.5, exit: 0, typed: 'grep -n needle haystack.txt', cwd: '/srv', output: ['3:a needle in the haystack'] },
    { t_start: 8, t_end: 9, exit: 1, typed: 'cat evil.html', cwd: '/srv', output: [FIX.evil] },
    { t_start: 12, t_end: 13, exit: 0, typed: 'ls /srv', cwd: '/srv', output: ['evil.html  haystack.txt'] },
    { t_start: 16, t_end: 17, exit: 2, typed: 'false-command', cwd: '/tmp', output: ['zsh: command not found: false-command'] },
  ]);
  const needle: FixSession = {
    session: session(FIX.needle, base, { tmux: tmux('%1', '0', 'shell') }),
    commands: needleCmds,
    markers: [
      marker(9.5, base, 'nested', { user: 'root', sudo_user: 'alice', mode: 'link', child_uuid: FIX.nested }),
      marker(13.5, base, 'pause', { reason: 'multiplexer', detail: 'tmux: client' }),
      marker(15, base, 'resume', { reason: 'foreground' }),
    ],
    segments: null,
    duration: 20,
  };

  const nested: FixSession = {
    session: session(FIX.nested, base + 9.5, {
      user: 'root',
      euid: 0,
      sudo_user: 'alice',
      holder_comm: 'sudo',
      parent_uuid: FIX.needle,
      tmux: tmux('%1', '0', 'shell'),
    }),
    commands: cmds([{ t_start: 1, t_end: 1.5, exit: 0, typed: 'id', cwd: '/root', output: ['uid=0(root) gid=0(root)'] }]),
    markers: [],
    segments: null,
    duration: 4,
  };

  const legacyStart = base - 10 * 86400;
  const legacy: FixSession = {
    session: session(FIX.legacy, legacyStart, {
      legacy: true,
      attribution: 'legacy',
      holder_comm: null,
      tty: null,
      euid: null,
      tz: null,
    }),
    commands: cmds([
      { t_start: 1, t_end: 3, exit: 0, typed: 'uptime', cwd: '/home/alice', output: ['up 3 days'], approx: true, confidence: 0.62 },
      { t_start: 5, t_end: 7, exit: null, typed: 'make legacy-target', cwd: '/home/alice', output: ['built'], approx: true, confidence: 0.4 },
    ]),
    markers: [],
    segments: null,
    duration: 10,
  };

  const segCmds = cmds(
    Array.from({ length: 12 }, (_, i) => ({
      t_start: 2 + i * 10,
      t_end: 3 + i * 10,
      exit: 0,
      typed: `step ${i + 1}`,
      cwd: '/data',
      output: [`segmented output ${i + 1}`],
    })),
  );
  const segmented: FixSession = {
    session: session(FIX.segmented, base + 60, {
      tmux: tmux('%3', '1', 'build'),
      cast_bytes: 50 * 1024 * 1024,
    }),
    commands: segCmds,
    markers: [],
    segments: [0, 30, 60, 90],
    duration: 120,
  };

  const live: FixSession = {
    session: session(FIX.live, Math.floor(now) - 120, {
      tmux: tmux('%2', '2', 'logs'),
      state: 'live',
      ended_at: null,
      exit_status: null,
    }),
    commands: cmds([{ t_start: 1, t_end: 2, exit: 0, typed: 'uptime', cwd: '/var/log', output: ['up 4 days'] }]),
    markers: [],
    segments: null,
    duration: 30,
  };

  const forget: FixSession = {
    session: session(FIX.forget, base + 300, { holder_comm: 'sshd-session' }),
    commands: cmds([
      { t_start: 1, t_end: 2, exit: 0, typed: `echo ${FIX.forgetWord}`, cwd: '/home/alice', output: [FIX.forgetWord] },
      { t_start: 4, t_end: 5, exit: 0, typed: 'echo keep-me', cwd: '/home/alice', output: ['keep-me'] },
    ]),
    markers: [],
    segments: null,
    duration: 8,
  };

  const all = [needle, nested, legacy, segmented, live, forget];
  for (const f of all) {
    f.session.duration = f.duration;
    f.session.commands = f.commands.length;
    if (f.session.ended_at !== null) f.session.ended_at = f.session.started_at + f.duration;
  }
  return all;
}

export function toCommand(f: FixSession, c: FixCommand): Command {
  return {
    seq: c.seq,
    parent_seq: null,
    depth: 0,
    t_start: c.t_start,
    t_end: c.t_end,
    ts_start: f.session.started_at + c.t_start,
    ts_end: f.session.started_at + c.t_end,
    exit: c.exit,
    pipestatus: c.exit === null ? [] : [c.exit],
    cwd: c.cwd,
    user: f.session.user,
    typed: c.typed,
    expanded: null,
    typed_trunc: false,
    approx: c.approx ?? false,
    confidence: c.confidence ?? null,
    out_bytes: c.output.join('\n').length,
    out_truncated: false,
  };
}

/** Seconds after a command start at which its output is written (the output hit's `t`). */
export const OUTPUT_DELAY = 0.4;

export type CastEvent = [t: number, code: string, data: string];

/**
 * The session's events in absolute session time: keystrokes, the command line, a chapter
 * marker, the output, and a final event at exactly `duration`.
 */
export function sessionEvents(f: FixSession, masked: (seq: number) => string | null): CastEvent[] {
  const ev: CastEvent[] = [];
  const prompt = (cwd: string) => `\x1b[1;32m${f.session.user}@${f.session.host}\x1b[0m:\x1b[1;34m${cwd}\x1b[0m$ `;
  ev.push([0, 'o', prompt(f.commands[0]?.cwd ?? '~')]);
  for (const c of f.commands) {
    const keys = [...c.typed, '\r'];
    const t0 = Math.max(ev.at(-1)![0], c.t_start - 0.02 * keys.length - 0.05);
    keys.forEach((k, i) => ev.push([round(t0 + i * 0.02), 'i', k]));
    ev.push([c.t_start, 'm', `$ ${c.typed}`]);
    ev.push([c.t_start, 'o', `${c.typed}\r\n`]);
    const by = masked(c.seq);
    const out = by !== null ? `[redacted by ${by}]` : c.output.join('\r\n');
    ev.push([round(c.t_start + OUTPUT_DELAY), 'o', `${out}\r\n`]);
    ev.push([c.t_end, 'o', prompt(c.cwd)]);
  }
  for (const m of f.markers) ev.push([m.t, 'o', '']);
  ev.push([f.duration, 'o', '']);
  ev.sort((a, b) => a[0] - b[0]);
  return ev;
}

const round = (t: number) => Math.round(t * 1e6) / 1e6;
