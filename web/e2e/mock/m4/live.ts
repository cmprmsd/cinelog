// Mock of `GET /api/sessions/{uuid}/live` (work package M4-D; docs/API.md §9.1): `LiveInit`
// with the screen so far (the concatenated output stands in for the server's avt dump), then
// every appended event, then `LiveOffline` + `done` on a crash or `done` on a normal end.
// Sessions that are not live get their final screen and `done` right away.
//
// Forgotten commands are masked like `/cast` (`[redacted by …]`), also in commands appended
// through the control endpoint.
//
// Mock-only control, for the specs (`e2e/live.spec.ts`): `POST /api/_mock/live/{uuid}` with a
// `LiveControl` body grows the cast, runs a command, pauses, crashes or ends the recorder,
// refuses the stream, or resets everything. Recorder changes go out on the UI bus like the
// real server's; `recorderOf` gives the current status for the bus `hello`.

import type { IncomingMessage, ServerResponse } from 'node:http';
import type { LiveInit, LiveMessage, LiveSession, SessionState } from '../../../src/lib/api/types.ts';
import { OUTPUT_DELAY, sessionEvents, type CastEvent, type FixSession } from '../fixtures.ts';
import type { MockContext, MockHandler } from './context.ts';

export type LiveControl =
  | { op: 'append'; data: string }
  | { op: 'input'; data: string }
  /** Runs a command (typed keys, marker, echo, output, prompt); answers its `seq`. */
  | { op: 'command'; typed: string; output: string }
  | { op: 'pause'; reason: string; detail: string | null }
  | { op: 'resume' }
  | { op: 'crash' }
  | { op: 'end' }
  | { op: 'unavailable'; on: boolean }
  | { op: 'reset' };

export const CONTROL_PREFIX = '/api/_mock/live/';

/** An appended event; `seq` marks the output of an appended command (masked when forgotten). */
interface Extra {
  ev: CastEvent;
  seq?: number;
}

type Mask = (seq: number) => string | null;

interface LiveState {
  /** Events appended after the fixture's own (absolute session seconds). */
  extra: Extra[];
  streams: Set<ServerResponse>;
  /** The recorder status after a pause or resume; `null`: recording. */
  recorder: LiveSession | null;
  unavailable: boolean;
  /** What `reset` restores. */
  original: { state: SessionState; ended_at: number | null };
}

const states = new Map<string, LiveState>();

function stateOf(f: FixSession): LiveState {
  let st = states.get(f.session.uuid);
  if (!st) {
    st = { extra: [], streams: new Set(), recorder: null, unavailable: false, original: { state: f.session.state, ended_at: f.session.ended_at } };
    states.set(f.session.uuid, st);
  }
  return st;
}

/**
 * Seqs of appended commands: unique for the whole run, so a forget (which cannot be undone)
 * never masks a command of a later test.
 */
let nextSeq = 1000;

/** The recorder status of a live fixture session, as the bus `hello` should report it. */
export function recorderOf(uuid: string): LiveSession | null {
  return states.get(uuid.toLowerCase())?.recorder ?? null;
}

const round = (t: number) => Math.round(t * 1e6) / 1e6;

function events(f: FixSession, st: LiveState, mask: Mask): CastEvent[] {
  const extra = st.extra.map(({ ev, seq }): CastEvent => {
    const by = seq === undefined ? null : mask(seq);
    return by === null ? ev : [ev[0], ev[1], `[redacted by ${by}]\r\n`];
  });
  return [...sessionEvents(f, mask), ...extra];
}

function send(res: ServerResponse, msg: LiveMessage): void {
  res.write(`data: ${JSON.stringify(msg)}\n\n`);
}

function finish(res: ServerResponse, offline: boolean): void {
  if (offline) send(res, { state: 'offline' });
  res.write('event: done\ndata:\n\n');
  res.end();
}

function liveSession(f: FixSession, over: Partial<LiveSession> = {}): LiveSession {
  return { session: f.session, status: 'recording', reason: null, detail: null, since: null, ...over };
}

async function openStream(ctx: MockContext, f: FixSession, req: IncomingMessage, res: ServerResponse): Promise<void> {
  const st = stateOf(f);
  if (st.unavailable) ctx.fail(503, 'unavailable', 'live streaming is not available');
  const mask: Mask = ctx.maskFor(f);
  res.writeHead(200, { 'Content-Type': 'text/event-stream', 'Cache-Control': 'no-store', 'X-Accel-Buffering': 'no' });
  const all = events(f, st, mask);
  const init: LiveInit = {
    cols: f.session.cols,
    rows: f.session.rows,
    time: all.at(-1)?.[0] ?? 0,
    init: all.filter(([, code]) => code === 'o').map(([, , d]) => d).join(''),
  };
  send(res, init);
  if (f.session.state !== 'live') return finish(res, f.session.state === 'crashed');
  st.streams.add(res);
  const ping = setInterval(() => res.write(': keep-alive\n\n'), 15_000);
  req.on('close', () => {
    clearInterval(ping);
    st.streams.delete(res);
  });
}

function control(ctx: MockContext, f: FixSession, c: LiveControl): unknown {
  const st = stateOf(f);
  const now = Date.now() / 1000;
  const last = () => Math.max(st.extra.at(-1)?.ev[0] ?? 0, f.duration);
  const emit = (ev: CastEvent, seq?: number) => {
    st.extra.push({ ev, seq });
    for (const res of st.streams) send(res, ev);
  };
  const push = (code: string, data: string) => {
    const ev: CastEvent = [round(Math.max(last(), now - f.session.started_at)), code, data];
    emit(ev);
    return { t: ev[0] };
  };
  const setRecorder = (r: LiveSession | null, published: LiveSession) => {
    st.recorder = r;
    ctx.publish({ type: 'session-update', data: published });
  };
  const endAll = (state: SessionState) => {
    f.session.state = state;
    f.session.ended_at = state === 'ended' ? now : null;
    for (const res of st.streams) finish(res, state === 'crashed');
    st.streams.clear();
    st.recorder = null;
    ctx.publish({ type: 'session-end', data: { session: f.session.uuid, state, ended_at: f.session.ended_at } });
  };
  switch (c.op) {
    case 'append':
      return push('o', c.data);
    case 'input':
      return push('i', c.data);
    case 'command': {
      const seq = nextSeq++;
      const t0 = round(Math.max(last(), now - f.session.started_at));
      const at = (dt: number) => round(t0 + dt);
      [...c.typed, '\r'].forEach((k, i) => emit([at(i * 0.02), 'i', k]));
      const t = at(c.typed.length * 0.02 + 0.05);
      emit([t, 'm', `$ ${c.typed}`]);
      emit([t, 'o', `${c.typed}\r\n`]);
      emit([round(t + OUTPUT_DELAY), 'o', `${c.output}\r\n`], seq);
      emit([round(t + OUTPUT_DELAY + 0.1), 'o', '$ ']);
      return { seq };
    }
    case 'pause': {
      const r = liveSession(f, { status: 'paused', reason: c.reason, detail: c.detail, since: now });
      setRecorder(r, r);
      return {};
    }
    case 'resume':
      setRecorder(null, liveSession(f, { since: now }));
      return {};
    case 'crash':
      endAll('crashed');
      return {};
    case 'end':
      endAll('ended');
      return {};
    case 'unavailable':
      st.unavailable = c.on;
      return {};
    case 'reset': {
      const wasLive = f.session.state === 'live';
      for (const res of st.streams) res.end();
      states.delete(f.session.uuid);
      f.session.state = st.original.state;
      f.session.ended_at = st.original.ended_at;
      if (!wasLive && f.session.state === 'live') ctx.publish({ type: 'session-start', data: liveSession(f) });
      else if (st.recorder) ctx.publish({ type: 'session-update', data: liveSession(f) });
      return {};
    }
  }
}

function isControl(body: unknown): body is LiveControl {
  if (typeof body !== 'object' || body === null) return false;
  const b = body as Record<string, unknown>;
  switch (b.op) {
    case 'append':
    case 'input':
      return typeof b.data === 'string';
    case 'command':
      return typeof b.typed === 'string' && typeof b.output === 'string';
    case 'pause':
      return typeof b.reason === 'string' && (b.detail === null || typeof b.detail === 'string');
    case 'unavailable':
      return typeof b.on === 'boolean';
    case 'resume':
    case 'crash':
    case 'end':
    case 'reset':
      return true;
    default:
      return false;
  }
}

export const handle: MockHandler = async (ctx, { req, res, url, method }) => {
  const path = url.pathname;
  if (path.startsWith(CONTROL_PREFIX)) {
    if (method !== 'POST') ctx.fail(405, 'method_not_allowed', 'method not allowed');
    const id = decodeURIComponent(path.slice(CONTROL_PREFIX.length)).toLowerCase();
    const f = ctx.fixtures.find((x) => x.session.uuid === id) ?? ctx.fail(404, 'not_found', 'no such session');
    const body = await ctx.readJson(req);
    const op = isControl(body) ? body : ctx.fail(400, 'bad_request', 'unknown live control');
    ctx.json(res, 200, control(ctx, f, op));
    return true;
  }
  const m = /^\/api\/sessions\/([^/]+)\/live$/.exec(path);
  if (!m) return false;
  if (method !== 'GET') ctx.fail(405, 'method_not_allowed', 'method not allowed');
  await openStream(ctx, ctx.find(decodeURIComponent(m[1]!)), req, res);
  return true;
};
