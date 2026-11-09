import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { Bus, BUS_EVENT_NAMES, nextLive, parseBusEvent, type EventSourceLike } from './events.svelte';
import type { BusEvent, LiveSession, Session } from './types';

class FakeSource implements EventSourceLike {
  readyState = 0;
  closed = false;
  private listeners = new Map<string, ((e: MessageEvent<string>) => void)[]>();
  addEventListener(type: string, fn: (e: MessageEvent<string>) => void): void {
    this.listeners.set(type, [...(this.listeners.get(type) ?? []), fn]);
  }
  close(): void {
    this.closed = true;
    this.readyState = 2;
  }
  fire(type: string, data = ''): void {
    for (const fn of this.listeners.get(type) ?? []) fn(new MessageEvent(type, { data }));
  }
}

const hello = JSON.stringify({ version: 'v', server_started_at: 1, live: [] });

describe('bus', () => {
  let sources: FakeSource[];
  let closedCalls: number;
  let bus: Bus;

  beforeEach(() => {
    vi.useFakeTimers();
    sources = [];
    closedCalls = 0;
    bus = new Bus(
      () => {
        const s = new FakeSource();
        sources.push(s);
        return s;
      },
      () => (closedCalls += 1),
    );
  });
  afterEach(() => vi.useRealTimers());

  it('parses known events only', () => {
    expect(parseBusEvent('tag', '{"name":"ops","color":null,"count":1}')).toEqual({
      type: 'tag',
      data: { name: 'ops', color: null, count: 1 },
    });
    expect(parseBusEvent('tag', '{nope')).toBeNull();
    expect(parseBusEvent('message', '{}')).toBeNull();
    expect(BUS_EVENT_NAMES).toContain('session-end');
  });

  it('connects on the first subscriber and closes after the last', () => {
    const got: BusEvent[] = [];
    expect(bus.status).toBe('idle');
    const off1 = bus.subscribe((e) => got.push(e));
    const off2 = bus.subscribe(() => {});
    expect(sources).toHaveLength(1);
    expect(bus.status).toBe('connecting');
    sources[0]!.fire('hello', hello);
    expect(bus.status).toBe('open');
    sources[0]!.fire('session-end', '{"session":"u","state":"crashed","ended_at":null}');
    expect(got.map((e) => e.type)).toEqual(['hello', 'session-end']);
    off1();
    expect(sources[0]!.closed).toBe(false);
    off2();
    expect(sources[0]!.closed).toBe(true);
    expect(bus.status).toBe('idle');
  });

  it('reconnects with backoff after the server refuses the stream', () => {
    bus.subscribe(() => {});
    const first = sources[0]!;
    first.readyState = 2;
    first.fire('error');
    expect(bus.status).toBe('down');
    expect(closedCalls).toBe(1);
    vi.advanceTimersByTime(999);
    expect(sources).toHaveLength(1);
    vi.advanceTimersByTime(1);
    expect(sources).toHaveLength(2);
    sources[1]!.readyState = 2;
    sources[1]!.fire('error');
    vi.advanceTimersByTime(1999);
    expect(sources).toHaveLength(2);
    vi.advanceTimersByTime(1);
    expect(sources).toHaveLength(3);
    // Events of a replaced stream are ignored; a hello resets the backoff.
    first.fire('hello', hello);
    expect(bus.status).toBe('connecting');
    sources[2]!.fire('hello', hello);
    expect(bus.status).toBe('open');
  });

  it('lets the browser retry network errors itself', () => {
    bus.subscribe(() => {});
    sources[0]!.readyState = 0;
    sources[0]!.fire('error');
    expect(bus.status).toBe('connecting');
    vi.advanceTimersByTime(60_000);
    expect(sources).toHaveLength(1);
    expect(closedCalls).toBe(0);
  });

  it('keeps delivering when a handler throws', () => {
    const err = vi.spyOn(console, 'error').mockImplementation(() => {});
    const got: string[] = [];
    bus.subscribe(() => {
      throw new Error('boom');
    });
    bus.subscribe((e) => got.push(e.type));
    sources[0]!.fire('hello', hello);
    expect(got).toEqual(['hello']);
    err.mockRestore();
  });

  it('keeps the live sessions from hello and the events after it', () => {
    const live = (uuid: string, started_at: number, status: LiveSession['status'] = 'recording'): LiveSession => ({
      session: { uuid, started_at } as Session,
      status,
      reason: null,
      detail: null,
      since: null,
      child_scope: null,
      child_url: null,
    });
    bus.subscribe(() => {});
    expect(bus.live).toEqual([]);
    sources[0]!.fire('hello', JSON.stringify({ version: 'v', server_started_at: 1, live: [live('a', 1), live('b', 2)] }));
    expect(bus.live.map((l) => l.session.uuid)).toEqual(['b', 'a']);
    sources[0]!.fire('session-start', JSON.stringify(live('c', 3)));
    sources[0]!.fire('session-update', JSON.stringify(live('a', 1, 'paused')));
    expect(bus.live.map((l) => [l.session.uuid, l.status])).toEqual([
      ['c', 'recording'],
      ['b', 'recording'],
      ['a', 'paused'],
    ]);
    sources[0]!.fire('session-end', JSON.stringify({ session: 'b', state: 'ended', ended_at: 5 }));
    sources[0]!.fire('forgotten', JSON.stringify({ target: { kind: 'session', uuid: 'c' }, by: 'x', at: 6 }));
    sources[0]!.fire('forgotten', JSON.stringify({ target: { kind: 'command', uuid: 'a', seq: 1 }, by: 'x', at: 6 }));
    expect(bus.live.map((l) => l.session.uuid)).toEqual(['a']);
    expect(nextLive(bus.live, { type: 'tag', data: { name: 't', color: null, count: 1 } })).toBeNull();
    sources[0]!.fire('hello', JSON.stringify({ version: 'v', server_started_at: 1, live: [] }));
    expect(bus.live).toEqual([]);
  });

  it('forgets the live sessions when it stops', () => {
    const off = bus.subscribe(() => {});
    sources[0]!.fire('hello', JSON.stringify({ version: 'v', server_started_at: 1, live: [{ session: { uuid: 'a', started_at: 1 }, status: 'recording', reason: null, detail: null, since: null, child_scope: null, child_url: null }] }));
    expect(bus.live).toHaveLength(1);
    off();
    expect(bus.live).toEqual([]);
  });
});
