import { flushSync, mount, unmount } from 'svelte';
import { afterAll, afterEach, beforeAll, describe, expect, it } from 'vitest';
import type { Timeline, TimelineBlock, TimelineRow, TimelineSession } from '../../lib/api/types';
import TimelineChart from './TimelineChart.svelte';
import { LANE_H } from './layout';

// The chart is refreshed in place (follow mode, bus events): hover and keyboard state must
// survive a response with fewer rows or blocks.

const A = 'aaaaaaaa-0000-7000-8000-000000000001';
const B = 'bbbbbbbb-0000-7000-8000-000000000002';

function block(session: string, seq: number, ts: number): TimelineBlock {
  return { session, seq, t_start: ts, ts_start: ts, ts_end: ts + 20, exit: 0, typed: `cmd-${session[0]}${seq}`, cwd: null, approx: false, depth: 0 };
}

function session(uuid: string): TimelineSession {
  return { uuid, user: 'alice', state: 'ended', started_at: 0, ended_at: 1000, parent_uuid: null, legacy: false };
}

function row(key: string, uuid: string, blocks: TimelineBlock[]): TimelineRow {
  return {
    key,
    label: key,
    host: 'h1',
    user: 'alice',
    tmux: { server: null, session_id: null, session: null, window: null, window_name: null, pane: null },
    holder_comm: null,
    tty: null,
    sessions: [session(uuid)],
    blocks,
    marks: [],
    density: [],
  };
}

function timeline(rows: TimelineRow[]): Timeline {
  return {
    from: 0,
    to: 1000,
    group: 'user',
    lanes: [{ key: 'user:alice', label: 'alice', rows }],
    idle: [],
    annotations: [],
    commands: rows.reduce((n, r) => n + r.blocks.length, 0),
    aggregated: false,
    bin_secs: null,
  };
}

const full = () =>
  timeline([
    row('a', A, [block(A, 1, 100), block(A, 2, 300), block(A, 3, 600)]),
    row('b', B, [block(B, 1, 200), block(B, 2, 800)]),
  ]);

// jsdom has no layout: `bind:clientWidth` needs a ResizeObserver and reads 0 px.
const clientWidth = Object.getOwnPropertyDescriptor(HTMLElement.prototype, 'clientWidth');
beforeAll(() => {
  globalThis.ResizeObserver ??= class {
    observe(): void {}
    unobserve(): void {}
    disconnect(): void {}
  } as unknown as typeof ResizeObserver;
  Object.defineProperty(HTMLElement.prototype, 'clientWidth', { configurable: true, get: () => 800 });
});
afterAll(() => {
  if (clientWidth) Object.defineProperty(HTMLElement.prototype, 'clientWidth', clientWidth);
});

let cleanup: (() => void) | null = null;
afterEach(() => {
  cleanup?.();
  cleanup = null;
});

function setup() {
  const props = $state({ timeline: full() });
  const opened: TimelineBlock[] = [];
  const target = document.createElement('div');
  document.body.append(target);
  const c = mount(TimelineChart, {
    target,
    props: {
      get timeline() {
        return props.timeline;
      },
      utc: true,
      collapse: true,
      now: 2000,
      selected: null,
      receivedAt: 0,
      onopen: (b: TimelineBlock) => opened.push(b),
      onopensession: () => {},
      onzoom: () => {},
    },
  });
  flushSync();
  cleanup = () => {
    unmount(c);
    target.remove();
  };
  const plot = target.querySelector<HTMLElement>('.plot')!;
  const key = (k: string) => {
    plot.dispatchEvent(new KeyboardEvent('keydown', { key: k, bubbles: true, cancelable: true }));
    flushSync();
  };
  const tip = () => target.querySelector('[data-testid="tl-tip"]')?.textContent ?? null;
  const announce = () => target.querySelector('#tl-announce')!.textContent;
  const swap = (tl: Timeline) => {
    props.timeline = tl;
    flushSync();
  };
  return { target, plot, key, tip, announce, swap, opened };
}

describe('TimelineChart refreshes', () => {
  it('keeps the keyboard cursor on its command when the oldest blocks slide out', () => {
    const t = setup();
    t.key('ArrowRight');
    t.key('End');
    expect(t.tip()).toContain('cmd-a3');
    // Row `a` lost two blocks, row `b` is gone: the cursor is now at index 0.
    t.swap(timeline([row('a', A, [block(A, 3, 600), block(A, 4, 700)])]));
    expect(t.tip()).toContain('cmd-a3');
    expect(t.announce()).toContain('cmd-a3');
    t.key('Enter');
    expect(t.opened.map((b) => [b.session, b.seq])).toEqual([[A, 3]]);
    t.key('ArrowRight');
    expect(t.tip()).toContain('cmd-a4');
  });

  it('drops the cursor and the tooltip when its command or row disappears', () => {
    const t = setup();
    t.key('ArrowDown');
    t.key('ArrowDown');
    t.key('End');
    expect(t.tip()).toContain('cmd-b2');
    t.swap(timeline([row('a', A, [block(A, 1, 100)])]));
    expect(t.tip()).toBeNull();
    expect(t.announce()).toBe('');
    expect(t.target.querySelector('[data-testid="tl-cursor"]')).toBeNull();
    t.key('Enter');
    expect(t.opened).toEqual([]);
    // The next arrow starts over at the first visible command.
    t.key('ArrowRight');
    expect(t.tip()).toContain('cmd-a1');
  });

  it('re-tests the pointer against the new response', () => {
    const t = setup();
    // Over row `a` (below the lane header), on block 3 (ts 600..620 of 0..1000 at 800 px).
    const y = LANE_H + 5;
    t.plot.dispatchEvent(new MouseEvent('pointermove', { clientX: 485, clientY: y, bubbles: true }));
    flushSync();
    expect(t.tip()).toContain('cmd-a3');
    t.swap(timeline([row('a', A, [block(A, 1, 100)])]));
    expect(t.tip()).toBeNull();
    t.swap(timeline([row('a', A, [block(A, 9, 600)])]));
    expect(t.tip()).toContain('cmd-a9');
  });
});
