// Mock of `GET /api/timeline` (work package M4-B; docs/API.md §9.3) over the fixture sessions,
// with the server's rules: the window (default the last 24 h, at most 31 days), the person of
// a session, pane and shell rows grouped by tmux session, nested sessions in a row of their
// own below their parent's, blocks, marks from
// the markers, idle breaks and aggregation above 20 000 commands. Annotations live in the
// M4-C mock; specs that need them on the timeline add them with `page.route`.

import type {
  DensityBin,
  Marker,
  Session,
  TimeSpan,
  Timeline,
  TimelineBlock,
  TimelineGroup,
  TimelineLane,
  TimelineMark,
  TimelineRow,
  TimelineRowGroup,
} from '../../../src/lib/api/types.ts';
import type { FixSession } from '../fixtures.ts';
import type { MockContext, MockHandler } from './context.ts';

const DAY = 86_400;
const MAX_WINDOW = 31 * DAY;
const AGGREGATE_ABOVE = 20_000;
const BIN_SIZES = [60, 300, 900, 1800, 3600, 10_800, 21_600, 86_400];
const IDLE_MIN = 600;
/** Room kept around a point mark so it never sits inside an idle break. */
const POINT_PAD = 60;

export const handle: MockHandler = async (ctx, { res, url, method }) => {
  if (url.pathname !== '/api/timeline' || method !== 'GET') return false;
  ctx.json(res, 200, timeline(ctx, url.searchParams, Date.now() / 1000));
  return true;
};

function finite(ctx: MockContext, sp: URLSearchParams, name: string): number | null {
  const v = sp.get(name);
  if (v === null) return null;
  const n = Number(v);
  if (v.trim() === '' || !Number.isFinite(n)) ctx.fail(400, 'bad_request', `${name} must be a finite number`);
  return n;
}

export function person(s: Session): string {
  return s.sudo_user || s.claimed_user || s.user;
}

export interface Slot {
  key: string;
  /** Within its group. */
  label: string;
  group: TimelineRowGroup;
  /** Window and pane index of a pane; `null` outside tmux. */
  order: [number, number] | null;
}

const index = (v: string | null): number => (v !== null && /^\d+$/.test(v) ? Number(v) : Infinity);

export function slot(s: Session): Slot {
  const t = s.tmux;
  if (t.pane) {
    const pane = t.pane_index || t.pane;
    const label = t.window ? `${[t.window, t.window_name].filter(Boolean).join(' ')} · ${pane}` : pane;
    return {
      key: `pane:${s.host}/${t.server ?? '-'}/${t.pane}`,
      label,
      group: { key: `tmux:${s.host}/${t.server ?? '-'}/${t.session_id ?? '-'}`, kind: 'tmux', label: t.session || t.session_id || 'tmux', host: s.host },
      order: [index(t.window), index(t.pane_index)],
    };
  }
  const holder = s.holder_comm || 'shell';
  const group: TimelineRowGroup = { key: `shells:${s.host}`, kind: 'shells', label: 'shells', host: s.host };
  if (s.tty) return { key: `tty:${s.host}/${s.tty}`, label: `${holder} ${s.tty.replace(/^\/dev\//, '')}`, group, order: null };
  return { key: `session:${s.uuid}`, label: `${holder} ${s.uuid.slice(0, 8)}`, group, order: null };
}

/** The label of a pane or shell on its own (a lane of group `pane`). */
export function slotPath(x: Slot): string {
  return x.group.kind === 'tmux' ? `${x.group.label} › ${x.label}` : x.label;
}

function sessionEnd(s: Session, now: number): number {
  if (s.state === 'live') return Math.max(now, s.started_at);
  return s.ended_at ?? s.started_at + (s.duration ?? 0);
}

function marks(f: FixSession, now: number): TimelineMark[] {
  const s = f.session;
  const at = (m: Marker) => m.ts;
  const str = (v: unknown) => (typeof v === 'string' && v ? v : null);
  const mark = (kind: TimelineMark['kind'], ts: number, ts_end: number | null): TimelineMark => ({
    session: s.uuid,
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
  const out: TimelineMark[] = [];
  let paused: TimelineMark | null = null;
  for (const m of [...f.markers].sort((a, b) => a.t - b.t)) {
    switch (m.kind) {
      case 'pause':
        if (!paused) paused = { ...mark('pause', at(m), null), reason: str(m.data.reason), detail: str(m.data.detail) };
        break;
      case 'resume':
        if (paused) {
          out.push({ ...paused, ts_end: Math.max(paused.ts, at(m)) });
          paused = null;
        }
        break;
      case 'multiplexer': {
        // The follow-up marker of a tmux client names the session it attached to.
        const t = m.data.tmux as { server_pid?: number; server_start?: number; session_id?: string; session_name?: string } | undefined;
        if (paused?.reason === 'multiplexer' && t?.server_pid && t.session_id) {
          paused.tmux = { server: `${t.server_pid}:${t.server_start ?? 0}`, session_id: t.session_id, session: t.session_name || null };
        }
        break;
      }
      case 'skip':
        out.push(mark('skip', at(m), null));
        break;
      case 'nested':
        out.push({ ...mark('nested', at(m), null), reason: str(m.data.mode), detail: str(m.data.user), child: str(m.data.child_uuid), child_scope: m.data.scope === 'team' || m.data.scope === 'personal' ? m.data.scope : null, child_url: str(m.data.url) });
        break;
      case 'gap': {
        const from = typeof m.data.from_t === 'number' ? m.data.from_t : m.t;
        const to = typeof m.data.to_t === 'number' ? m.data.to_t : m.t;
        const bytes = typeof m.data.dropped_bytes === 'number' ? `${m.data.dropped_bytes} bytes dropped` : null;
        out.push({ ...mark('gap', s.started_at + m.t - Math.max(0, to - from), at(m)), detail: bytes });
        break;
      }
    }
  }
  if (paused) out.push({ ...paused, ts_end: s.state === 'live' ? null : Math.max(paused.ts, sessionEnd(s, now)) });
  return out;
}

interface Acc {
  /** Slot of the root row it belongs to. */
  slot: Slot;
  person: string;
  /** The row it is nested in. */
  parent: Acc | null;
  depth: number;
  nested: Acc[];
  newest: Session;
  first: number;
  sessions: Session[];
  blocks: TimelineBlock[];
  ends: number[];
  marks: TimelineMark[];
  /** Mark extents that keep a stretch out of the idle breaks. */
  busy: (readonly [number, number])[];
}

export function timeline(ctx: MockContext, sp: URLSearchParams, now: number): Timeline {
  let from = finite(ctx, sp, 'from');
  let to = finite(ctx, sp, 'to');
  const g = sp.get('group');
  if (g !== null && g !== 'user' && g !== 'pane') ctx.fail(400, 'bad_request', 'group must be user or pane');
  const group: TimelineGroup = g === 'pane' ? 'pane' : 'user';
  if (from === null && to === null) [from, to] = [now - DAY, now];
  else if (to === null) to = from! + DAY;
  else if (from === null) from = to - DAY;
  const w = { from: from!, to: to! };
  if (w.from >= w.to) ctx.fail(400, 'bad_request', 'from must be before to');
  if (w.to - w.from > MAX_WINDOW) ctx.fail(400, 'bad_request', 'the timeline window is at most 31 days');
  const hidden = sp.get('hidden') === 'true';
  const user = sp.get('user');
  const host = sp.get('host');

  const found = ctx
    .visible()
    .filter((f) => (hidden || !f.session.hidden) && (!host || f.session.host === host))
    .filter((f) => f.session.started_at <= w.to && sessionEnd(f.session, now) >= w.from)
    .sort((a, b) => a.session.started_at - b.session.started_at);
  const byUuid = new Map(found.map((f) => [f.session.uuid, f]));
  /** `f` and its ancestors in the window, outermost last. */
  const chain = (f: FixSession): FixSession[] => {
    const seen = new Set([f.session.uuid]);
    const out = [f];
    for (;;) {
      const cur = out[out.length - 1]!;
      const p = cur.session.parent_uuid ? byUuid.get(cur.session.parent_uuid) : undefined;
      if (!p || seen.has(p.session.uuid)) return out;
      seen.add(p.session.uuid);
      out.push(p);
    }
  };

  // A root session is drawn in the row of its (slot, person); a nested one in a row of its
  // own below its parent's, one per user it runs as.
  const rows = new Map<string, Acc>();
  const fresh = (s: Slot, who: string, parent: Acc | null, newest: Session): Acc => ({
    slot: s,
    person: who,
    parent,
    depth: parent ? parent.depth + 1 : 0,
    nested: [],
    newest,
    first: Infinity,
    sessions: [],
    blocks: [],
    ends: [],
    marks: [],
    busy: [],
  });
  let total = 0;
  for (const f of found) {
    const up = chain(f).reverse();
    const r = up[0]!.session;
    if (user && person(r) !== user) continue;
    let key = `${slot(r).key}\u0000${person(r)}`;
    let acc = rows.get(key);
    if (!acc) rows.set(key, (acc = fresh(slot(r), person(r), null, r)));
    for (const d of up.slice(1)) {
      key = `${key}\u0000nested:${d.session.user}`;
      let below = rows.get(key);
      if (!below) {
        rows.set(key, (below = fresh(acc.slot, acc.person, acc, d.session)));
        acc.nested.push(below);
      }
      acc = below;
    }
    if (acc.sessions.length === 0 || f.session.started_at >= acc.newest.started_at) acc.newest = f.session;
    acc.sessions.push(f.session);
    acc.first = Math.min(acc.first, Math.max(w.from, f.session.started_at));
    const end = sessionEnd(f.session, now);
    for (const c of f.commands) {
      const ts_start = f.session.started_at + c.t_start;
      const ts_end = f.session.started_at + c.t_end;
      if (ts_start > w.to || ts_end < w.from) continue;
      acc.blocks.push({
        session: f.session.uuid,
        seq: c.seq,
        t_start: c.t_start,
        ts_start,
        ts_end,
        exit: c.exit,
        typed: c.typed.split('\n')[0]!.slice(0, 200),
        cwd: c.cwd,
        approx: c.approx ?? false,
        depth: 0,
      });
      acc.ends.push(Math.max(ts_start, ts_end));
      total += 1;
    }
    for (const m of marks(f, now)) {
      const point = m.kind === 'skip' || m.kind === 'nested';
      const e = Math.max(m.ts, m.ts_end ?? (point ? m.ts : end));
      if (m.ts > w.to || e < w.from) continue;
      acc.marks.push(m);
      acc.busy.push(point ? [m.ts - POINT_PAD, e + POINT_PAD] : [m.ts, e]);
    }
  }

  const busy = [...rows.values()].flatMap((r) => [...r.blocks.map((b, i) => [b.ts_start, r.ends[i]!] as const), ...r.busy]);
  const idle = idleSpans(busy, w);
  const aggregated = total > AGGREGATE_ABOVE;
  const bin = aggregated ? (BIN_SIZES.find((b) => Math.ceil((w.to - w.from) / b) <= 2000) ?? DAY) : null;

  const roots = [...rows.values()].filter((r) => !r.parent);
  const people = new Map<string, Set<string>>();
  // The newest root session of a slot names it, the newest of those its group.
  const slots = new Map<string, { at: number; slot: Slot }>();
  for (const r of roots) {
    people.set(r.slot.key, (people.get(r.slot.key) ?? new Set()).add(r.person));
    const l = slots.get(r.slot.key);
    if (!l || r.newest.started_at >= l.at) slots.set(r.slot.key, { at: r.newest.started_at, slot: slot(r.newest) });
  }
  const groupLabel = new Map<string, { at: number; label: string }>();
  for (const { at, slot: x } of slots.values()) {
    const l = groupLabel.get(x.group.key);
    if (!l || at >= l.at) groupLabel.set(x.group.key, { at, label: x.group.label });
  }
  const named = (r: Acc): Slot => {
    const x = slots.get(r.slot.key)!.slot;
    return { ...x, group: { ...x.group, label: groupLabel.get(x.group.key)!.label } };
  };
  const rowKey = (r: Acc): string => {
    if (r.parent) return `${rowKey(r.parent)}/nested:${r.newest.user}`;
    return group === 'user' && people.get(r.slot.key)!.size === 1 ? r.slot.key : `${r.slot.key}|user:${r.person}`;
  };
  const byFirst = (a: Acc, b: Acc) => a.first - b.first || (rowKey(a) < rowKey(b) ? -1 : 1);
  const groupFirst = new Map<string, number>();
  for (const r of roots) {
    const k = `${r.person}\u0000${named(r).group.key}`;
    groupFirst.set(k, Math.min(groupFirst.get(k) ?? Infinity, r.first));
  }
  const cmp = (a: number, b: number) => (a === b ? 0 : a < b ? -1 : 1);
  const inGroup = (a: Acc, b: Acc) => {
    const [sa, sb] = [named(a), named(b)];
    const first = cmp(groupFirst.get(`${a.person}\u0000${sa.group.key}`)!, groupFirst.get(`${b.person}\u0000${sb.group.key}`)!);
    if (first) return first;
    if (sa.group.key !== sb.group.key) return sa.group.key < sb.group.key ? -1 : 1;
    if (sa.order && sb.order) return cmp(sa.order[0], sb.order[0]) || cmp(sa.order[1], sb.order[1]) || byFirst(a, b);
    return byFirst(a, b);
  };
  const ordered: Acc[] = [];
  const place = (r: Acc) => {
    ordered.push(r);
    [...r.nested].sort(byFirst).forEach(place);
  };
  roots.sort(group === 'user' ? inGroup : byFirst).forEach(place);

  const lanes = new Map<string, TimelineLane>();
  for (const r of ordered) {
    const x = named(r);
    const label = r.parent ? (r.newest.sudo_user ? `${r.newest.user} (sudo)` : r.newest.user) : group === 'user' ? x.label : r.person;
    const [laneKey, laneLabel] = group === 'user' ? [`user:${r.person}`, r.person] : [r.slot.key, slotPath(x)];
    const blocks = r.blocks.sort((a, b) => a.ts_start - b.ts_start || a.seq - b.seq);
    const row: TimelineRow = {
      key: rowKey(r),
      label,
      host: r.newest.host,
      user: r.person,
      tmux: r.newest.tmux,
      holder_comm: r.newest.holder_comm,
      tty: r.newest.tty,
      group: group === 'user' ? x.group : null,
      parent: r.parent ? rowKey(r.parent) : null,
      depth: r.depth,
      sessions: r.sessions.map((s) => ({
        uuid: s.uuid,
        user: s.user,
        state: s.state,
        started_at: s.started_at,
        // Where the session is drawn to: the indexed end of one that was never closed.
        ended_at: s.state === 'live' ? s.ended_at : (s.ended_at ?? sessionEnd(s, now)),
        parent_uuid: s.parent_uuid,
        legacy: s.legacy,
      })),
      blocks: aggregated ? [] : blocks,
      marks: r.marks.sort((a, b) => a.ts - b.ts),
      density: bin ? density(blocks, w.from, bin) : [],
    };
    const lane = lanes.get(`${laneLabel}\u0000${laneKey}`) ?? { key: laneKey, label: laneLabel, rows: [] };
    lane.rows.push(row);
    lanes.set(`${laneLabel}\u0000${laneKey}`, lane);
  }

  return {
    from: w.from,
    to: w.to,
    group,
    lanes: [...lanes.entries()].sort(([a], [b]) => (a < b ? -1 : a > b ? 1 : 0)).map(([, l]) => l),
    idle,
    annotations: [],
    commands: total,
    aggregated,
    bin_secs: bin,
  };
}

function idleSpans(busy: (readonly [number, number])[], w: { from: number; to: number }): TimeSpan[] {
  const out: TimeSpan[] = [];
  let cursor = w.from;
  for (const [a, b] of [...busy].sort((x, y) => x[0] - y[0])) {
    const start = Math.max(a, w.from);
    if (start - cursor >= IDLE_MIN) out.push({ from: cursor, to: start });
    cursor = Math.max(cursor, Math.min(b, w.to));
  }
  if (w.to - cursor >= IDLE_MIN) out.push({ from: cursor, to: w.to });
  return out;
}

function density(blocks: TimelineBlock[], from: number, bin: number): DensityBin[] {
  const bins = new Map<number, DensityBin>();
  for (const b of blocks) {
    const i = Math.max(0, Math.floor((b.ts_start - from) / bin));
    const d = bins.get(i) ?? { ts: from + i * bin, commands: 0, failed: 0 };
    d.commands += 1;
    if (b.exit !== null && b.exit !== 0) d.failed += 1;
    bins.set(i, d);
  }
  return [...bins.values()].sort((a, b) => a.ts - b.ts);
}
