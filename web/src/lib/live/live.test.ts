import { describe, expect, it, vi } from 'vitest';
import type { BusEvent, LiveSession, Session } from '../api/types';
import {
  badgeFor,
  initialView,
  liveOptions,
  liveSource,
  LIVE_BUFFER_MS,
  needsReconnect,
  probeOutcome,
  probeStream,
  recorderBadge,
  reduce,
  retryDelay,
  type LiveInput,
  type LiveView,
} from './live';

const ID = '0b9e4c1a-7d2e-4f00-9a51-0000000000a5';
const OTHER = '0b9e4c1a-7d2e-4f00-9a51-0000000000a1';
const CHILD = '0b9e4c1a-7d2e-4f00-9a51-0000000000a3';

const session = (uuid: string) => ({ uuid }) as Session;
const rec = (over: Partial<LiveSession> = {}, uuid = ID): LiveSession => ({
  session: session(uuid),
  status: 'recording',
  reason: null,
  detail: null,
  since: null,
  ...over,
});

const run = (inputs: LiveInput[], from: LiveView = initialView()) => inputs.reduce((v, i) => reduce(v, i, ID), from);
const bus = (event: BusEvent): LiveInput => ({ type: 'bus', event });

describe('reduce: the stream', () => {
  it('goes connecting → open → reconnecting → open', () => {
    let v = run([{ type: 'stream-open' }]);
    expect(v.stream).toBe('open');
    v = run([{ type: 'stream-error' }], v);
    expect(v.stream).toBe('reconnecting');
    v = run([{ type: 'stream-open' }], v);
    expect(v.stream).toBe('open');
  });

  it('a crash ends in offline, and the following done keeps it', () => {
    const v = run([{ type: 'stream-open' }, { type: 'stream-offline' }, { type: 'stream-done' }, { type: 'stream-error' }]);
    expect(v.stream).toBe('offline');
  });

  it('done alone ends normally and clears the recorder', () => {
    const v = run([bus({ type: 'hello', data: { version: 'v', server_started_at: 0, live: [rec()] } }), { type: 'stream-open' }, { type: 'stream-done' }]);
    expect(v).toMatchObject({ stream: 'ended', recorder: null });
  });

  it('late events after the end do not reopen it, a reconnect does', () => {
    let v = run([{ type: 'stream-done' }, { type: 'stream-open' }]);
    expect(v.stream).toBe('ended');
    v = run([{ type: 'reconnect' }, { type: 'stream-open' }], v);
    expect(v.stream).toBe('open');
  });

  it('probes: refused, missing, and fine', () => {
    expect(run([{ type: 'stream-error' }, { type: 'probe', status: 503 }])).toMatchObject({ stream: 'unavailable', refused: 503 });
    expect(run([{ type: 'probe', status: 0 }])).toMatchObject({ stream: 'unavailable', refused: 0 });
    expect(run([{ type: 'probe', status: 404 }])).toMatchObject({ stream: 'missing' });
    expect(run([{ type: 'probe', status: 200 }]).stream).toBe('connecting');
    // A stream error while refused stays refused; a reconnect leaves it.
    const v = run([{ type: 'probe', status: 503 }, { type: 'stream-error' }]);
    expect(v.stream).toBe('unavailable');
    expect(run([{ type: 'reconnect' }], v).stream).toBe('connecting');
    // Nothing leaves `missing`.
    expect(run([{ type: 'probe', status: 404 }, { type: 'reconnect' }, { type: 'stream-open' }]).stream).toBe('missing');
  });

  it('session metadata: crashed means offline', () => {
    expect(run([{ type: 'detail', state: 'crashed' }]).stream).toBe('offline');
    expect(run([{ type: 'detail', state: 'ended' }]).stream).toBe('connecting');
    expect(run([{ type: 'detail', state: 'live' }, { type: 'stream-open' }]).stream).toBe('open');
  });
});

describe('reduce: the UI bus', () => {
  it('hello picks this session out of the live list', () => {
    const v = run([bus({ type: 'hello', data: { version: 'v', server_started_at: 0, live: [rec({}, OTHER), rec({ status: 'paused' })] } })]);
    expect(v.recorder?.status).toBe('paused');
    expect(run([bus({ type: 'hello', data: { version: 'v', server_started_at: 0, live: [rec({}, OTHER)] } })], v).recorder).toBeNull();
  });

  it('updates for other sessions are ignored', () => {
    const v = run([bus({ type: 'session-update', data: rec({ status: 'paused' }, OTHER) })]);
    expect(v.recorder).toBeNull();
  });

  it('session-end: crashed is offline, ended waits for done', () => {
    const start = run([bus({ type: 'session-start', data: rec() }), { type: 'stream-open' }]);
    expect(run([bus({ type: 'session-end', data: { session: ID, state: 'crashed', ended_at: null } })], start)).toMatchObject({
      stream: 'offline',
      recorder: null,
    });
    expect(run([bus({ type: 'session-end', data: { session: ID, state: 'ended', ended_at: 5 } })], start)).toMatchObject({
      stream: 'open',
      recorder: null,
    });
    expect(run([bus({ type: 'session-end', data: { session: OTHER, state: 'crashed', ended_at: null } })], start).stream).toBe('open');
  });

  it('a forgotten session is missing; a forgotten command asks for a reconnect', () => {
    const gone: BusEvent = { type: 'forgotten', data: { target: { kind: 'session', uuid: ID }, by: 'alice', at: 1 } };
    const cmd: BusEvent = { type: 'forgotten', data: { target: { kind: 'command', uuid: ID, seq: 2 }, by: 'alice', at: 1 } };
    const range: BusEvent = { type: 'forgotten', data: { target: { kind: 'range', uuid: ID, t_from: 1, t_to: 2 }, by: 'alice', at: 1 } };
    const elsewhere: BusEvent = { type: 'forgotten', data: { target: { kind: 'command', uuid: OTHER, seq: 2 }, by: 'alice', at: 1 } };
    expect(run([bus(gone)]).stream).toBe('missing');
    expect(run([bus(cmd)]).stream).toBe('connecting');
    expect(needsReconnect(cmd, ID)).toBe(true);
    expect(needsReconnect(range, ID)).toBe(true);
    expect(needsReconnect(gone, ID)).toBe(false);
    expect(needsReconnect(elsewhere, ID)).toBe(false);
  });
});

describe('reduce: seeding a late subscriber', () => {
  const paused = rec({ status: 'paused', reason: 'multiplexer', detail: 'tmux: client' });

  it('a seed shows a pause that began before the view subscribed', () => {
    const v = run([{ type: 'stream-open' }, { type: 'seed', live: [rec({}, OTHER), paused] }]);
    expect(v.recorder).toEqual(paused);
    expect(badgeFor(v).label).toBe('paused (tmux)');
  });

  it('a seed without this session leaves it unknown (LIVE from the stream)', () => {
    const v = run([{ type: 'stream-open' }, { type: 'seed', live: [rec({}, OTHER)] }]);
    expect(v).toMatchObject({ recorder: null, heard: true });
    expect(badgeFor(v).label).toBe('LIVE');
  });

  it('what the bus said since subscribing wins over an older seed', () => {
    const resumed = rec({ since: 5 });
    let v = run([{ type: 'stream-open' }, bus({ type: 'session-update', data: resumed }), { type: 'seed', live: [paused] }]);
    expect(v.recorder).toEqual(resumed);
    v = run([bus({ type: 'hello', data: { version: 'v', server_started_at: 0, live: [] } }), { type: 'seed', live: [paused] }]);
    expect(v.recorder).toBeNull();
    v = run([bus({ type: 'session-end', data: { session: ID, state: 'ended', ended_at: 1 } }), { type: 'seed', live: [paused] }]);
    expect(v.recorder).toBeNull();
  });

  it('events about other sessions do not block the seed', () => {
    const v = run([bus({ type: 'session-update', data: rec({}, OTHER) }), { type: 'seed', live: [paused] }]);
    expect(v.recorder).toEqual(paused);
  });

  it('a seed does not undo a crash', () => {
    const v = run([{ type: 'stream-open' }, { type: 'stream-offline' }, { type: 'seed', live: [paused] }]);
    expect(badgeFor(v).kind).toBe('offline');
  });
});

describe('badges', () => {
  it('LIVE while open and recording', () => {
    expect(badgeFor(run([{ type: 'stream-open' }]))).toMatchObject({ kind: 'live', label: 'LIVE' });
    expect(badgeFor(run([bus({ type: 'session-start', data: rec() }), { type: 'stream-open' }])).label).toBe('LIVE');
  });

  it('paused by tmux, nested sudo or the user; degraded', () => {
    expect(recorderBadge(rec({ status: 'paused', reason: 'multiplexer', detail: 'tmux: client' }))).toMatchObject({ kind: 'paused', label: 'paused (tmux)' });
    expect(recorderBadge(rec({ status: 'paused', reason: 'multiplexer', detail: 'screen' })).label).toBe('paused (screen)');
    expect(recorderBadge(rec({ status: 'paused', reason: 'multiplexer', detail: null })).label).toBe('paused (multiplexer)');
    expect(recorderBadge(rec({ status: 'paused', reason: 'nested', detail: CHILD.toUpperCase() }))).toMatchObject({ label: 'paused (nested)', child: CHILD });
    expect(recorderBadge(rec({ status: 'paused', reason: 'nested', detail: '../x' })).child).toBeNull();
    const user = recorderBadge(rec({ status: 'paused', reason: 'user', detail: 'lunch' }));
    expect(user).toMatchObject({ label: 'paused (user)' });
    expect(user.title).toContain('lunch');
    expect(recorderBadge(rec({ status: 'paused', reason: null })).label).toBe('paused');
    const deg = recorderBadge(rec({ status: 'degraded', reason: 'disk full', detail: 'ENOSPC' }));
    expect(deg).toMatchObject({ kind: 'degraded', label: 'degraded' });
    expect(deg.title).toContain('ENOSPC');
  });

  it('the stream state wins over the recorder when the stream is not open', () => {
    const paused = bus({ type: 'session-update', data: rec({ status: 'paused', reason: 'multiplexer', detail: 'tmux: client' }) });
    expect(badgeFor(run([paused])).kind).toBe('connecting');
    expect(badgeFor(run([paused, { type: 'stream-open' }])).label).toBe('paused (tmux)');
    expect(badgeFor(run([paused, { type: 'stream-open' }, { type: 'stream-error' }])).label).toBe('reconnecting…');
    expect(badgeFor(run([{ type: 'stream-offline' }]))).toMatchObject({ kind: 'offline', label: 'offline' });
    expect(badgeFor(run([{ type: 'stream-done' }])).kind).toBe('ended');
    expect(badgeFor(run([{ type: 'probe', status: 503 }])).kind).toBe('unavailable');
    expect(badgeFor(run([{ type: 'probe', status: 404 }])).kind).toBe('missing');
  });
});

describe('probing and retries', () => {
  it('maps statuses to what the page does', () => {
    expect(probeOutcome(200)).toBe('retry-now');
    expect(probeOutcome(401)).toBe('signed-out');
    expect(probeOutcome(404)).toBe('missing');
    expect(probeOutcome(503)).toBe('unavailable');
    expect(probeOutcome(429)).toBe('unavailable');
    expect(probeOutcome(0)).toBe('unavailable');
  });

  it('backs off from 1 s doubling to 30 s', () => {
    expect([0, 1, 2, 3, 4, 5, 6, 50].map(retryDelay)).toEqual([1000, 2000, 4000, 8000, 16000, 30000, 30000, 30000]);
  });

  it('reads only the status and aborts the stream', async () => {
    let signal: AbortSignal | undefined;
    const fetchFn = vi.fn(async (_url: RequestInfo | URL, init?: RequestInit) => {
      signal = init?.signal ?? undefined;
      expect(init?.credentials).toBe('same-origin');
      return new Response(null, { status: 503 });
    });
    expect(await probeStream('/api/sessions/x/live', fetchFn as typeof fetch)).toBe(503);
    expect(fetchFn).toHaveBeenCalledWith('/api/sessions/x/live', expect.anything());
    expect(signal?.aborted).toBe(true);
    expect(await probeStream('/x', (async () => Promise.reject(new TypeError('down'))) as typeof fetch)).toBe(0);
  });
});

describe('player setup', () => {
  it('uses the eventsource driver with a fixed small buffer', () => {
    expect(liveSource('/api/sessions/x/live')).toEqual({ driver: 'eventsource', url: '/api/sessions/x/live', bufferTime: LIVE_BUFFER_MS });
  });

  it('starts at once, without controls or parser options', () => {
    const o = liveOptions({ theme: 'auto/dracula', keystrokeOverlay: true });
    expect(o).toMatchObject({ autoPlay: true, controls: false, theme: 'auto/dracula', keystrokeOverlay: true });
    expect(o).not.toHaveProperty('idleTimeLimit');
    expect(o).not.toHaveProperty('markers');
    expect(o).not.toHaveProperty('startAt');
  });
});
