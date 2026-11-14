// Geometry of the timeline chart: lane and row positions, command blocks merged into
// drawable runs (a week of team data is ~20k blocks, far more than there are pixels), marks,
// annotation spans, density bars, and hit testing for hover, click and the keyboard.

import type {
  DensityBin,
  Scope,
  Timeline,
  TimelineAnnotation,
  TimelineBlock,
  TimelineMark,
  TimelineRow,
  TimelineRowGroup,
  TimelineSession,
} from '../../lib/api/types';
import { fmtDuration, fmtTs, oneLine } from '../../lib/format';
import { nestedLink } from '../../lib/markers';
import type { TimeScale } from './scale';

export const LANE_H = 24;
/** Header of a row group (a tmux session, or the shells outside tmux). */
export const GROUP_H = 20;
export const ROW_H = 26;
/** Blocks are inset this much from the row edges, nested shells a bit more per level. */
const PAD = 4;
const DEPTH_INSET = 3;
const MIN_BLOCK_PX = 2;
const MIN_SPAN_PX = 3;
/** Pointer tolerance around thin blocks and points. */
export const HIT_PX = 3;

export type BlockClass = 'ok' | 'fail' | 'unknown';

export function blockClass(b: Pick<TimelineBlock, 'exit'>): BlockClass {
  if (b.exit === null) return 'unknown';
  return b.exit === 0 ? 'ok' : 'fail';
}

export interface RowGeom {
  row: TimelineRow;
  /** Index among the drawn rows. */
  index: number;
  lane: number;
  y: number;
  live: boolean;
}

/** The header of the rows of one tmux session (or of the shells outside tmux) in a lane. */
export interface GroupGeom {
  /** Unique in the layout: the lane and the group (`collapsed` holds these). */
  id: string;
  group: TimelineRowGroup;
  lane: number;
  y: number;
  /** Rows of the group, nested ones included, drawn or not. */
  count: number;
  live: boolean;
  collapsed: boolean;
}

export interface LaneGeom {
  key: string;
  label: string;
  y: number;
  rows: RowGeom[];
  /** Group headers and rows, top to bottom. */
  items: ({ kind: 'group'; group: GroupGeom } | { kind: 'row'; row: RowGeom })[];
}

export interface Layout {
  lanes: LaneGeom[];
  /** The drawn rows (those of a collapsed group are left out). */
  rows: RowGeom[];
  groups: GroupGeom[];
  height: number;
  /** Row index by row key (annotations). */
  rowByKey: Map<string, number>;
  /** Row index by session uuid (every session is drawn in exactly one row). */
  rowBySession: Map<string, number>;
}

export function groupId(laneKey: string, groupKey: string): string {
  return `${laneKey}\u0000${groupKey}`;
}

/** Key of the group of the panes of the tmux session a paused shell attached to. */
export function attachedGroupKey(host: string, m: TimelineMark): string | null {
  return m.tmux ? `tmux:${host}/${m.tmux.server}/${m.tmux.session_id}` : null;
}

/**
 * Lanes, group headers and rows from top to bottom. A lane gets group headers when it has
 * a tmux session (a lane of shells only stays flat); the rows of the groups in `collapsed`
 * ([`GroupGeom.id`]) are not drawn.
 */
export function layout(tl: Timeline, collapsed: ReadonlySet<string> = new Set()): Layout {
  const lanes: LaneGeom[] = [];
  const rows: RowGeom[] = [];
  const groups: GroupGeom[] = [];
  const rowByKey = new Map<string, number>();
  const rowBySession = new Map<string, number>();
  let y = 0;
  tl.lanes.forEach((lane, li) => {
    const g: LaneGeom = { key: lane.key, label: lane.label, y, rows: [], items: [] };
    y += LANE_H;
    const grouped = lane.rows.some((r) => r.group?.kind === 'tmux');
    let open: GroupGeom | null = null;
    for (const row of lane.rows) {
      const live = row.sessions.some((s) => s.state === 'live');
      if (grouped && row.group && open?.group.key !== row.group.key) {
        const id = groupId(lane.key, row.group.key);
        open = { id, group: row.group, lane: li, y, count: 0, live: false, collapsed: collapsed.has(id) };
        groups.push(open);
        g.items.push({ kind: 'group', group: open });
        y += GROUP_H;
      }
      if (open) {
        open.count += 1;
        open.live ||= live;
        if (open.collapsed) continue;
      }
      const r: RowGeom = { row, index: rows.length, lane: li, y, live };
      rowByKey.set(row.key, r.index);
      for (const s of row.sessions) rowBySession.set(s.uuid, r.index);
      rows.push(r);
      g.rows.push(r);
      g.items.push({ kind: 'row', row: r });
      y += ROW_H;
    }
    lanes.push(g);
  });
  return { lanes, rows, groups, height: y, rowByKey, rowBySession };
}

/** A command by identity; indices into a response do not survive a refresh. */
export interface BlockRef {
  session: string;
  seq: number;
}

/** Where a command is in a layout (row index, block index). */
export interface BlockAt {
  row: number;
  i: number;
}

/** Where `ref` is drawn in `lay`, `null` when the response does not have it (any more). */
export function locateBlock(lay: Layout, ref: BlockRef | null): BlockAt | null {
  if (!ref) return null;
  const row = lay.rowBySession.get(ref.session);
  if (row === undefined) return null;
  const i = lay.rows[row]!.row.blocks.findIndex((b) => b.session === ref.session && b.seq === ref.seq);
  return i >= 0 ? { row, i } : null;
}

/** The command at `at`, `null` when `at` is outside the layout. */
export function blockAt(lay: Layout, at: BlockAt | null): TimelineBlock | null {
  return at ? (lay.rows[at.row]?.row.blocks[at.i] ?? null) : null;
}

// ------------------------------------------------------------------ block ends

/**
 * Where a session's open spans are drawn to: now while it is live, else its end (the server
 * sends the indexed end as `ended_at` for a session that was never closed); `null` when unknown.
 */
export function sessionEnd(s: TimelineSession | undefined, now: number): number | null {
  if (!s) return null;
  if (s.state === 'live') return Math.max(s.started_at, now);
  return s.ended_at;
}

/** Where a block is drawn to: its end, else the end of its session. */
export function blockEnd(b: TimelineBlock, sessions: ReadonlyMap<string, TimelineSession>, now: number): number {
  if (b.ts_end !== null) return Math.max(b.ts_start, b.ts_end);
  return Math.max(b.ts_start, sessionEnd(sessions.get(b.session), now) ?? b.ts_start);
}

export function sessionMap(tl: Timeline): Map<string, TimelineSession> {
  const m = new Map<string, TimelineSession>();
  for (const lane of tl.lanes) for (const row of lane.rows) for (const s of row.sessions) m.set(s.uuid, s);
  return m;
}

// ------------------------------------------------------------------ blocks

/** Pixel extents of a row's blocks, in `row.blocks` order (for hit testing). */
export interface BlockXs {
  x0: Float64Array;
  x1: Float64Array;
}

export function blockXs(row: TimelineRow, scale: TimeScale, sessions: ReadonlyMap<string, TimelineSession>, now: number): BlockXs {
  const n = row.blocks.length;
  const x0 = new Float64Array(n);
  const x1 = new Float64Array(n);
  row.blocks.forEach((b, i) => {
    const a = scale.x(b.ts_start);
    x0[i] = a;
    x1[i] = Math.max(a + MIN_BLOCK_PX, scale.x(blockEnd(b, sessions, now)));
  });
  return { x0, x1 };
}

/** A drawn rectangle standing for one or more neighbouring blocks of the same kind. */
export interface Run {
  x: number;
  w: number;
  y: number;
  h: number;
  cls: BlockClass;
  approx: boolean;
  /** Blocks merged into it. */
  count: number;
}

function depthBox(depth: number): { dy: number; h: number } {
  const inset = PAD + Math.min(2, depth) * DEPTH_INSET;
  return { dy: inset, h: ROW_H - 2 * inset };
}

/**
 * Blocks merged into runs: consecutive blocks (by start) of the same class, hatch and
 * depth whose pixels touch become one rectangle. Only what is inside `[0, width]` is kept.
 */
export function blockRuns(row: TimelineRow, xs: BlockXs, width: number): Run[] {
  const open = new Map<string, Run>();
  const out: Run[] = [];
  row.blocks.forEach((b, i) => {
    const a = Math.max(0, xs.x0[i]!);
    const e = Math.min(width, xs.x1[i]!);
    if (e < 0 || a > width || e - a <= 0) return;
    const cls = blockClass(b);
    const key = `${cls}${b.approx ? 'h' : ''}${Math.min(2, b.depth)}`;
    const cur = open.get(key);
    if (cur && a <= cur.x + cur.w + 0.75) {
      cur.w = Math.max(cur.w, e - cur.x);
      cur.count += 1;
      return;
    }
    const { dy, h } = depthBox(b.depth);
    const run: Run = { x: a, w: e - a, y: dy, h, cls, approx: b.approx, count: 1 };
    open.set(key, run);
    out.push(run);
  });
  // Deeper shells over outer ones; failures over successes over unknown, so a failed
  // command is never hidden behind a merged neighbour.
  const rank = { unknown: 0, ok: 1, fail: 2 } as const;
  return out.sort((a, b) => a.y - b.y || rank[a.cls] - rank[b.cls]);
}

// ------------------------------------------------------------------ marks

export interface MarkGeom {
  mark: TimelineMark;
  x: number;
  /** 0 for points. */
  w: number;
  point: boolean;
  label: string;
}

export function markLabel(m: TimelineMark): string {
  switch (m.kind) {
    case 'pause': {
      if (m.tmux) return `in tmux session ${oneLine(m.tmux.session ?? m.tmux.session_id)}: recorded in its panes, paused here`;
      const why = [m.reason, m.detail].filter(Boolean).join(': ');
      return why ? `recording paused (${oneLine(why)})` : 'recording paused';
    }
    case 'skip':
      return 'unrecorded command (leading space)';
    case 'nested':
      return m.detail ? `nested session: ${oneLine(m.detail)}` : 'nested session';
    case 'gap':
      return m.detail ? `output dropped (${oneLine(m.detail)})` : 'output dropped';
    case 'forgotten':
      return m.detail ? `forgotten by ${oneLine(m.detail)}` : 'forgotten';
  }
}

/**
 * Where a click on a `nested` mark goes, on a timeline served by a server of `scope`:
 * `external` (a new tab) for a child of the other scope, `child` (in-app) for one served
 * here; neither for other marks and for a child that cannot be located.
 */
export function markLink(m: TimelineMark, scope: Scope | null, host: string): { child: string | null; external: string | null } {
  if (m.kind !== 'nested') return { child: null, external: null };
  return nestedLink(m.child, m.child_scope, m.child_url, scope, host);
}

export function markGeoms(
  row: TimelineRow,
  scale: TimeScale,
  sessions: ReadonlyMap<string, TimelineSession>,
  now: number,
): MarkGeom[] {
  const out: MarkGeom[] = [];
  for (const m of row.marks) {
    const point = m.kind === 'skip' || m.kind === 'nested';
    const x = scale.x(m.ts);
    if (point) {
      if (x >= -HIT_PX && x <= scale.width + HIT_PX) out.push({ mark: m, x, w: 0, point, label: markLabel(m) });
      continue;
    }
    const end = m.ts_end ?? sessionEnd(sessions.get(m.session), now) ?? scale.to;
    const a = Math.max(0, x);
    const e = Math.min(scale.width, Math.max(x + MIN_SPAN_PX, scale.x(Math.max(m.ts, end))));
    if (e <= 0 || a >= scale.width) continue;
    out.push({ mark: m, x: a, w: Math.max(MIN_SPAN_PX, e - a), point, label: markLabel(m) });
  }
  return out;
}

// ------------------------------------------------------------------ annotations and density

export interface AnnotationGeom {
  annotation: TimelineAnnotation;
  row: number;
  x: number;
  w: number;
}

export function annotationGeoms(list: readonly TimelineAnnotation[], lay: Layout, scale: TimeScale): AnnotationGeom[] {
  const out: AnnotationGeom[] = [];
  for (const a of list) {
    const row = lay.rowByKey.get(a.row);
    if (row === undefined) continue;
    const x0 = Math.max(0, scale.x(a.ts_from));
    const x1 = Math.min(scale.width, Math.max(scale.x(a.ts_from) + MIN_SPAN_PX, scale.x(a.ts_to)));
    if (x1 <= 0 || x0 >= scale.width) continue;
    out.push({ annotation: a, row, x: x0, w: Math.max(MIN_SPAN_PX, x1 - x0) });
  }
  return out;
}

/** A safe CSS colour for an annotation (tag colours are `#rrggbb`; anything else is ignored). */
export function annotationColor(a: Pick<TimelineAnnotation, 'color'>): string {
  return a.color && /^#[0-9a-f]{6}$/i.test(a.color) ? a.color : 'var(--accent)';
}

export interface DensityBar {
  bin: DensityBin;
  x: number;
  w: number;
  /** Bar height and the failed part of it, in px. */
  h: number;
  hFailed: number;
}

/** Bars scaled to the busiest bin of the whole response (`max`). */
export function densityBars(bins: readonly DensityBin[], binSecs: number, scale: TimeScale, max: number): DensityBar[] {
  const full = ROW_H - 2 * PAD + 2;
  const out: DensityBar[] = [];
  for (const bin of bins) {
    const x0 = scale.x(bin.ts);
    const x1 = scale.x(bin.ts + binSecs);
    if (x1 <= 0 || x0 >= scale.width) continue;
    const x = Math.max(0, x0);
    const h = Math.max(2, (full * bin.commands) / Math.max(1, max));
    out.push({ bin, x, w: Math.max(1, Math.min(scale.width, x1) - x - 0.5), h, hFailed: (h * bin.failed) / Math.max(1, bin.commands) });
  }
  return out;
}

export function maxDensity(tl: Timeline): number {
  let max = 0;
  for (const lane of tl.lanes) for (const row of lane.rows) for (const b of row.density) max = Math.max(max, b.commands);
  return max;
}

// ------------------------------------------------------------------ hit testing

/**
 * The block under `x` in a row: the innermost one containing it, else the nearest within
 * `HIT_PX`; `-1` for none.
 */
export function hitBlock(row: TimelineRow, xs: BlockXs, x: number): number {
  let best = -1;
  let bestDist = Infinity;
  for (let i = 0; i < row.blocks.length; i += 1) {
    const a = xs.x0[i]!;
    if (a - HIT_PX > x) break;
    const e = xs.x1[i]!;
    if (x > e + HIT_PX) continue;
    const dist = x < a ? a - x : x > e ? x - e : 0;
    const prev = best >= 0 ? row.blocks[best]! : null;
    // Inside several: the deeper, then the later one (nested commands sit inside outer ones).
    if (dist < bestDist || (dist === bestDist && prev && row.blocks[i]!.depth >= prev.depth)) {
      best = i;
      bestDist = dist;
    }
  }
  return best;
}

/** A point mark (skip, nested) within `HIT_PX` of `x`, else a span mark containing it. */
export function hitMark(marks: readonly MarkGeom[], x: number): MarkGeom | null {
  let near: MarkGeom | null = null;
  let dist = Infinity;
  for (const m of marks) {
    if (!m.point) continue;
    const d = Math.abs(m.x - x);
    if (d <= HIT_PX + 1 && d < dist) {
      near = m;
      dist = d;
    }
  }
  if (near) return near;
  return marks.find((m) => !m.point && x >= m.x && x <= m.x + m.w) ?? null;
}

export function hitAnnotation(list: readonly AnnotationGeom[], row: number, x: number): AnnotationGeom | null {
  return list.find((a) => a.row === row && x >= a.x - 1 && x <= a.x + a.w + 1) ?? null;
}

/** The row at `y` (chart coordinates), or `-1` over a lane header or below the rows. */
export function rowAt(lay: Layout, y: number): number {
  for (const r of lay.rows) if (y >= r.y && y < r.y + ROW_H) return r.index;
  return -1;
}

// ------------------------------------------------------------------ keyboard

/** The block nearest to `x` in a row (keyboard moves between rows), `-1` when empty. */
export function nearestBlock(xs: BlockXs, x: number): number {
  let best = -1;
  let bestDist = Infinity;
  for (let i = 0; i < xs.x0.length; i += 1) {
    const mid = (xs.x0[i]! + xs.x1[i]!) / 2;
    const d = Math.abs(mid - x);
    if (d < bestDist) {
      best = i;
      bestDist = d;
    }
  }
  return best;
}

/** The first block of a row that is visible (ends inside the chart), `-1` when none. */
export function firstVisible(xs: BlockXs, width: number): number {
  for (let i = 0; i < xs.x0.length; i += 1) if (xs.x1[i]! >= 0 && xs.x0[i]! <= width) return i;
  return -1;
}

// ------------------------------------------------------------------ hover text

export interface BlockInfo {
  typed: string;
  lines: string[];
}

export function blockInfo(
  b: TimelineBlock,
  row: TimelineRow,
  sessions: ReadonlyMap<string, TimelineSession>,
  now: number,
  utc: boolean,
): BlockInfo {
  const s = sessions.get(b.session);
  const running = b.ts_end === null && s?.state === 'live';
  const dur = blockEnd(b, sessions, now) - b.ts_start;
  const exit = b.exit === null ? 'exit unknown' : `exit ${b.exit}`;
  const lines = [
    `${fmtTs(b.ts_start, utc)} · ${running ? `running for ${fmtDuration(dur)}` : fmtDuration(b.ts_end === null ? null : dur)} · ${exit}`,
    `${oneLine(row.user)}@${oneLine(row.host)}${b.cwd ? `  ${oneLine(b.cwd)}` : ''}`,
  ];
  if (b.approx) lines.push('approximate boundaries (legacy import)');
  if (b.ts_end === null && !running) lines.push('never reported its end');
  return { typed: oneLine(b.typed) || '(empty command)', lines };
}
