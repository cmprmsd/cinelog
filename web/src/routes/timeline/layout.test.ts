import { describe, expect, it } from 'vitest';
import type { Timeline, TimelineBlock, TimelineMark, TimelineRow, TimelineSession } from '../../lib/api/types';
import {
  LANE_H,
  ROW_H,
  activityRuns,
  annotationColor,
  annotationGeoms,
  blockAt,
  blockClass,
  blockEnd,
  blockInfo,
  blockRuns,
  blockXs,
  densityBars,
  firstVisible,
  focusRows,
  hitBlock,
  hitMark,
  layout,
  locateBlock,
  GROUP_H,
  groupId,
  attachedGroupKey,
  markGeoms,
  markLabel,
  markLink,
  maxDensity,
  nearestBlock,
  rowAt,
  sessionEnd,
  sessionMap,
  sessionSpans,
} from './layout';
import { TimeScale } from './scale';

const S = 'aaaaaaaa-0000-7000-8000-000000000001';

function block(seq: number, ts_start: number, ts_end: number | null, over: Partial<TimelineBlock> = {}): TimelineBlock {
  return { session: S, seq, t_start: ts_start, ts_start, ts_end, exit: 0, typed: `cmd ${seq}`, cwd: '/srv', approx: false, depth: 0, ...over };
}

function session(over: Partial<TimelineSession> = {}): TimelineSession {
  return { uuid: S, user: 'alice', state: 'ended', started_at: 0, ended_at: 1000, parent_uuid: null, legacy: false, ...over };
}

function row(key: string, blocks: TimelineBlock[], over: Partial<TimelineRow> = {}): TimelineRow {
  return {
    key,
    label: key,
    host: 'h1',
    user: 'alice',
    tmux: { server: null, session_id: null, session: null, window: null, window_name: null, pane: null, pane_index: null },
    holder_comm: null,
    tty: null,
    group: null,
    parent: null,
    depth: 0,
    sessions: [session()],
    blocks,
    marks: [],
    density: [],
    activity: [],
    ...over,
  };
}

function timeline(rows: TimelineRow[][], over: Partial<Timeline> = {}): Timeline {
  return {
    from: 0,
    to: 1000,
    group: 'user',
    lanes: rows.map((r, i) => ({ key: `user:${i}`, label: `u${i}`, rows: r })),
    idle: [],
    annotations: [],
    commands: rows.flat().reduce((n, r) => n + r.blocks.length, 0),
    aggregated: false,
    bin_secs: null,
    activity_bin_secs: 60,
    ...over,
  };
}

const scale = new TimeScale(0, 1000, 1000);

describe('layout', () => {
  it('stacks lanes and rows', () => {
    const tl = timeline([[row('a', []), row('b', [])], [row('c', [], { sessions: [session({ state: 'live' })] })]]);
    const lay = layout(tl);
    expect(lay.rows.map((r) => r.y)).toEqual([LANE_H, LANE_H + ROW_H, 2 * LANE_H + 2 * ROW_H]);
    expect(lay.height).toBe(2 * LANE_H + 3 * ROW_H);
    expect(lay.rows[2]!.live).toBe(true);
    expect(lay.rowByKey.get('c')).toBe(2);
    expect(rowAt(lay, LANE_H + 1)).toBe(0);
    expect(rowAt(lay, 1)).toBe(-1);
    expect(rowAt(lay, lay.height + 5)).toBe(-1);
  });

  it('heads the rows of a tmux session and leaves out those of a collapsed one', () => {
    const tmux = { key: 'tmux:h1/1:2/$0', kind: 'tmux' as const, label: 'ops', host: 'h1' };
    const shells = { key: 'shells:h1', kind: 'shells' as const, label: 'shells', host: 'h1' };
    const live = [session({ uuid: 'n', state: 'live' })];
    const tl = timeline([
      [
        row('sh', [], { group: shells }),
        row('p1', [], { group: tmux }),
        row('p1/nested:root', [], { group: tmux, parent: 'p1', depth: 1, sessions: live }),
        row('p2', [], { group: tmux }),
      ],
      // A lane without a tmux session stays flat.
      [row('other', [], { group: shells })],
    ]);
    const lay = layout(tl);
    expect(lay.groups.map((g) => [g.group.label, g.count, g.live, g.y])).toEqual([
      ['shells', 1, false, LANE_H],
      ['ops', 3, true, LANE_H + GROUP_H + ROW_H],
    ]);
    expect(lay.lanes[0]!.items.map((i) => (i.kind === 'group' ? `[${i.group.group.label}]` : i.row.row.key))).toEqual([
      '[shells]',
      'sh',
      '[ops]',
      'p1',
      'p1/nested:root',
      'p2',
    ]);
    expect(lay.rows.map((r) => r.y)).toEqual([
      LANE_H + GROUP_H,
      LANE_H + 2 * GROUP_H + ROW_H,
      LANE_H + 2 * GROUP_H + 2 * ROW_H,
      LANE_H + 2 * GROUP_H + 3 * ROW_H,
      2 * LANE_H + 2 * GROUP_H + 4 * ROW_H,
    ]);
    expect(lay.lanes[1]!.items.map((i) => i.kind)).toEqual(['row']);

    const ops = lay.groups[1]!;
    expect(ops.id).toBe(groupId('user:0', tmux.key));
    const folded = layout(tl, new Set([ops.id]));
    expect(folded.rows.map((r) => r.row.key)).toEqual(['sh', 'other']);
    expect(folded.groups[1]).toMatchObject({ collapsed: true, count: 3, live: true });
    expect(folded.rowBySession.has('n')).toBe(false);
    expect(folded.height).toBe(lay.height - 3 * ROW_H);
  });

  it('names the group a tmux pause points to', () => {
    const pause: TimelineMark = {
      session: S,
      kind: 'pause',
      ts: 1,
      ts_end: 2,
      reason: 'multiplexer',
      detail: 'tmux: client',
      child: null,
      child_scope: null,
      child_url: null,
      tmux: { server: '1:2', session_id: '$0', session: 'ops' },
    };
    expect(attachedGroupKey('h1', pause)).toBe('tmux:h1/1:2/$0');
    expect(attachedGroupKey('h1', { ...pause, tmux: null })).toBeNull();
    expect(markLabel(pause)).toBe('in tmux session ops: recorded in its panes, paused here');
    expect(markLabel({ ...pause, tmux: { ...pause.tmux!, session: null } })).toContain('session $0');
  });

  it('draws activity as typing in the terminal the person was in, typing elsewhere, and output', () => {
    const bin = (ts: number, inputs: number, out_bytes = 0) => ({ ts, inputs, out_bytes });
    const a = row('a', [], { activity: [bin(0, 5), bin(60, 2), bin(120, 0, 900), bin(300, 1)] });
    const b = row('b', [], { activity: [bin(0, 1), bin(60, 2), bin(120, 3)] });
    const other = row('c', [], { user: 'bob', activity: [bin(0, 1)] });
    const tl = timeline([[a, b], [other]]);
    const focus = focusRows(tl);
    // alice: `a` wins minute 0 and the tie in minute 1; `b` has minute 2. bob is on his own.
    expect([...focus.get('alice')!].sort((x, y) => x[0] - y[0])).toEqual([
      [0, 'a'],
      [60, 'a'],
      [120, 'b'],
      [300, 'a'],
    ]);
    expect(focus.get('bob')!.get(0)).toBe('c');
    const runs = (r: TimelineRow) => activityRuns(r, focus.get(r.user), 60, scale).map((x) => [x.kind, x.x, x.w]);
    // Adjacent bins of one kind are one run; the gap before minute 5 starts a new one.
    expect(runs(a)).toEqual([
      ['focus', 0, 120],
      ['output', 120, 60],
      ['focus', 300, 60],
    ]);
    expect(runs(b)).toEqual([
      ['typed', 0, 120],
      ['focus', 120, 60],
    ]);
    expect(activityRuns(row('d', [], { activity: [bin(-500, 1), bin(990, 1)] }), undefined, 60, scale)).toEqual([{ kind: 'typed', x: 990, w: 10 }]);
  });

  it('spans a row\'s sessions from start to end', () => {
    const r = row('n', [], {
      sessions: [
        session({ uuid: 'a', started_at: 100, ended_at: 300 }),
        session({ uuid: 'b', started_at: 600, ended_at: null, state: 'live' }),
        session({ uuid: 'c', started_at: 2000, ended_at: 2100 }),
      ],
    });
    // The live one is drawn to now; the one after the window is left out.
    expect(sessionSpans(r, scale, 750)).toEqual([
      { x: 100, w: 200 },
      { x: 600, w: 150 },
    ]);
  });

  it('colours by exit', () => {
    expect(blockClass({ exit: 0 })).toBe('ok');
    expect(blockClass({ exit: 2 })).toBe('fail');
    expect(blockClass({ exit: null })).toBe('unknown');
  });

  it('draws open blocks to now or the session end', () => {
    const live = new Map([[S, session({ state: 'live', ended_at: null })]]);
    const ended = new Map([[S, session({ ended_at: 700 })]]);
    expect(blockEnd(block(1, 100, 200), live, 900)).toBe(200);
    expect(blockEnd(block(1, 100, null), live, 900)).toBe(900);
    expect(blockEnd(block(1, 100, null), ended, 900)).toBe(700);
    expect(blockEnd(block(1, 100, null), new Map(), 900)).toBe(100);
  });

  it('draws open blocks and open marks of a never-closed session to the same end', () => {
    // A degraded session: the server sends its indexed end as `ended_at`.
    const degraded = session({ state: 'degraded', ended_at: 800 });
    const sessions = new Map([[S, degraded]]);
    expect(sessionEnd(degraded, 900)).toBe(800);
    expect(sessionEnd(session({ state: 'live', started_at: 950, ended_at: null }), 900)).toBe(950);
    expect(sessionEnd(undefined, 900)).toBeNull();
    const r = row('a', [block(1, 100, null)], {
      sessions: [degraded],
      marks: [{ session: S, kind: 'pause', ts: 300, ts_end: null, reason: null, detail: null, child: null, child_scope: null, child_url: null, tmux: null }],
    });
    const xs = blockXs(r, scale, sessions, 900);
    const [pause] = markGeoms(r, scale, sessions, 900);
    expect(xs.x1[0]).toBeCloseTo(800);
    expect(pause!.x + pause!.w).toBeCloseTo(800);
  });

  it('finds a command again after the response changed', () => {
    const T = 'bbbbbbbb-0000-7000-8000-000000000002';
    const other = (seq: number, ts: number) => block(seq, ts, ts + 5, { session: T });
    const before = layout(
      timeline([[row('a', [block(1, 10, 20), block(2, 30, 40), block(3, 50, 60)]), row('b', [other(1, 15), other(2, 70)], { sessions: [session({ uuid: T })] })]]),
    );
    expect(locateBlock(before, { session: T, seq: 2 })).toEqual({ row: 1, i: 1 });
    expect(blockAt(before, { row: 0, i: 2 })?.seq).toBe(3);
    // The window slid: row `a` lost its oldest block, row `b` is gone.
    const after = layout(timeline([[row('a', [block(2, 30, 40), block(3, 50, 60)])]]));
    expect(locateBlock(after, { session: S, seq: 3 })).toEqual({ row: 0, i: 1 });
    expect(locateBlock(after, { session: T, seq: 2 })).toBeNull();
    expect(locateBlock(after, { session: S, seq: 1 })).toBeNull();
    expect(locateBlock(after, null)).toBeNull();
    expect(blockAt(after, { row: 1, i: 1 })).toBeNull();
    expect(blockAt(after, { row: 0, i: 2 })).toBeNull();
    expect(blockAt(after, null)).toBeNull();
  });
});

describe('block runs', () => {
  it('merge touching blocks of one kind and keep failures on top', () => {
    const blocks = [
      block(1, 10, 20),
      block(2, 20, 30),
      block(3, 25, 26, { exit: 1 }),
      block(4, 30, 40),
      block(5, 100, 110, { approx: true }),
      block(6, 200, 300, { depth: 1 }),
    ];
    const r = row('a', blocks);
    const xs = blockXs(r, scale, new Map([[S, session()]]), 1000);
    const runs = blockRuns(r, xs, 1000);
    expect(runs.map((x) => [x.cls, x.count, Math.round(x.x), Math.round(x.w), x.approx])).toEqual([
      ['ok', 3, 10, 30, false],
      ['ok', 1, 100, 10, true],
      ['fail', 1, 25, 2, false],
      ['ok', 1, 200, 100, false],
    ]);
    expect(runs[3]!.h).toBeLessThan(runs[0]!.h);
  });

  it('give every block at least 2 px and drop what is outside', () => {
    const r = row('a', [block(1, -50, -40), block(2, 500, 500.1), block(3, 990, 1100)]);
    const xs = blockXs(r, scale, new Map(), 1000);
    expect(xs.x1[1]! - xs.x0[1]!).toBe(2);
    const runs = blockRuns(r, xs, 1000);
    expect(runs.map((x) => [Math.round(x.x), Math.round(x.w)])).toEqual([
      [500, 2],
      [990, 10],
    ]);
  });

  it('scale to 20k blocks', () => {
    const blocks = Array.from({ length: 20_000 }, (_, i) => block(i, i * 0.05, i * 0.05 + 0.02, { exit: i % 9 === 0 ? 1 : 0 }));
    const r = row('a', blocks);
    const t0 = performance.now();
    const runs = blockRuns(r, blockXs(r, scale, new Map(), 1000), 1000);
    expect(performance.now() - t0).toBeLessThan(200);
    expect(runs.length).toBeLessThan(3000);
  });
});

describe('hit testing', () => {
  const r = row('a', [block(1, 100, 200), block(2, 150, 160, { depth: 1 }), block(3, 300, 300.2)]);
  const xs = blockXs(r, scale, new Map(), 1000);

  it('finds the innermost block, then the nearest thin one', () => {
    expect(hitBlock(r, xs, 120)).toBe(0);
    expect(hitBlock(r, xs, 155)).toBe(1);
    expect(hitBlock(r, xs, 303)).toBe(2);
    expect(hitBlock(r, xs, 297.5)).toBe(2);
    expect(hitBlock(r, xs, 250)).toBe(-1);
    expect(hitBlock(row('e', []), blockXs(row('e', []), scale, new Map(), 0), 10)).toBe(-1);
  });

  it('moves the keyboard cursor between rows by position', () => {
    expect(nearestBlock(xs, 310)).toBe(2);
    expect(nearestBlock(xs, 0)).toBe(0);
    expect(firstVisible(xs, 1000)).toBe(0);
    expect(firstVisible(blockXs(row('b', [block(1, -100, -50)]), scale, new Map(), 0), 1000)).toBe(-1);
  });

  it('prefers point marks', () => {
    const mark = (kind: TimelineMark['kind'], ts: number, ts_end: number | null = null): TimelineMark => ({
      session: S,
      kind,
      ts,
      ts_end,
      reason: null,
      detail: null,
      child: null,
      child_scope: null,
      child_url: null,
      tmux: null,
    });
    const r2 = row('m', [], { marks: [mark('pause', 100, 400), mark('skip', 200), mark('pause', 900, null)] });
    const geoms = markGeoms(r2, scale, new Map([[S, session({ ended_at: 950 })]]), 1000);
    expect(hitMark(geoms, 201)?.mark.kind).toBe('skip');
    expect(hitMark(geoms, 300)?.mark.kind).toBe('pause');
    expect(hitMark(geoms, 600)).toBeNull();
    // An open pause of an ended session is drawn to the session end.
    expect(geoms[2]!.x + geoms[2]!.w).toBeCloseTo(950);
  });
});

describe('marks, annotations and density', () => {
  it('opens a nested child of the other scope on its own server', () => {
    const child = 'c0ffee00-0000-4000-8000-000000000001';
    const nested = (child_scope: TimelineMark['child_scope'], child_url: string | null = null): TimelineMark => ({
      session: S,
      kind: 'nested',
      ts: 0,
      ts_end: null,
      reason: 'pause',
      detail: 'root',
      child,
      child_scope,
      child_url,
      tmux: null,
    });
    // Same scope (or unknown): in-app.
    expect(markLink(nested('personal'), 'personal', 'box.lan')).toEqual({ child, external: null });
    expect(markLink(nested(null), 'personal', 'box.lan')).toEqual({ child, external: null });
    // A TEAM `sudo -i` in a PERSONAL shell: the reported URL, else the TEAM port on this host.
    expect(markLink(nested('team', `https://ops.lan:10000/s/${child}`), 'personal', 'box.lan')).toEqual({
      child,
      external: `https://ops.lan:10000/s/${child}`,
    });
    expect(markLink(nested('team'), 'personal', 'box.lan').external).toBe(`https://box.lan:10000/s/${child}`);
    // A PERSONAL child seen from TEAM without a reported URL: nowhere to go.
    expect(markLink(nested('personal'), 'team', 'box.lan')).toEqual({ child: null, external: null });
    expect(markLink({ ...nested('team'), kind: 'skip' }, 'personal', 'box.lan')).toEqual({ child: null, external: null });
  });

  it('labels every mark kind', () => {
    const m = (kind: TimelineMark['kind'], reason: string | null, detail: string | null): TimelineMark => ({
      session: S,
      kind,
      ts: 0,
      ts_end: null,
      reason,
      detail,
      child: null,
      child_scope: null,
      child_url: null,
      tmux: null,
    });
    expect(markLabel(m('pause', 'multiplexer', 'tmux: client'))).toBe('recording paused (multiplexer: tmux: client)');
    expect(markLabel(m('pause', null, null))).toBe('recording paused');
    expect(markLabel(m('skip', null, null))).toBe('unrecorded command (leading space)');
    expect(markLabel(m('nested', 'record', 'root'))).toBe('nested session: root');
    expect(markLabel(m('gap', null, '12 bytes dropped'))).toBe('output dropped (12 bytes dropped)');
    expect(markLabel(m('forgotten', null, 'bob\x1b[31m'))).toBe('forgotten by bob␛[31m');
  });

  it('places annotations on their row and checks colours', () => {
    const tl = timeline([[row('a', []), row('b', [])]], {
      annotations: [
        { uuid: 'n1', session: S, row: 'b', cmd_seq: null, ts_from: 100, ts_to: 100, tags: [], color: '#ff0000', author: 'alice' },
        { uuid: 'n2', session: S, row: 'gone', cmd_seq: null, ts_from: 100, ts_to: 200, tags: [], color: null, author: 'alice' },
        { uuid: 'n3', session: S, row: 'a', cmd_seq: 1, ts_from: 2000, ts_to: 3000, tags: [], color: null, author: 'alice' },
      ],
    });
    const geoms = annotationGeoms(tl.annotations, layout(tl), scale);
    expect(geoms.map((g) => [g.annotation.uuid, g.row, g.w])).toEqual([['n1', 1, 3]]);
    expect(annotationColor({ color: '#ff0000' })).toBe('#ff0000');
    expect(annotationColor({ color: 'red;background:url(x)' })).toBe('var(--accent)');
    expect(annotationColor({ color: null })).toBe('var(--accent)');
  });

  it('scales density bars to the busiest bin', () => {
    const tl = timeline(
      [[row('a', [], { density: [{ ts: 0, commands: 10, failed: 5 }, { ts: 100, commands: 40, failed: 0 }] })]],
      { aggregated: true, bin_secs: 100 },
    );
    const max = maxDensity(tl);
    expect(max).toBe(40);
    const bars = densityBars(tl.lanes[0]!.rows[0]!.density, 100, scale, max);
    expect(bars).toHaveLength(2);
    expect(bars[1]!.h).toBeGreaterThan(bars[0]!.h);
    expect(bars[0]!.hFailed).toBeCloseTo(bars[0]!.h / 2);
    expect(bars[0]!.w).toBeCloseTo(99.5);
  });

  it('builds hover text from untrusted data as one line', () => {
    const live = new Map([[S, session({ state: 'live', ended_at: null })]]);
    const b = block(1, 100, null, { typed: 'rm -rf /tmp/x\necho <b>', exit: null, cwd: '/tmp' });
    const info = blockInfo(b, row('a', [b]), live, 160, true);
    expect(info.typed).toBe('rm -rf /tmp/x ⏎ echo <b>');
    expect(info.lines[0]).toContain('running for 1m 00s');
    expect(info.lines[0]).toContain('exit unknown');
    expect(info.lines[1]).toBe('alice@h1  /tmp');
    const closed = blockInfo(block(2, 0, 1.5, { approx: true }), row('a', []), sessionMap(timeline([])), 0, true);
    expect(closed.lines[0]).toContain('1.5s · exit 0');
    expect(closed.lines).toContain('approximate boundaries (legacy import)');
  });
});
