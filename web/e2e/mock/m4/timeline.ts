// Mock of `GET /api/timeline` (work package M4-B; docs/API.md §9.3) over the fixture sessions,
// with the server's rules: the window (default the last 24 h, at most 31 days), the person of
// a session, pane and shell rows, nested sessions in their parent's row, blocks, marks from
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

export function slot(s: Session): { key: string; label: string } {
  const t = s.tmux;
  if (t.pane) {
    const label = t.session === null && t.window === null ? t.pane : `${t.session ?? '?'}:${t.window ?? '?'}.${t.pane}`;
    return { key: `pane:${s.host}/${t.server ?? '-'}/${t.pane}`, label };
  }
  const holder = s.holder_comm || 'shell';
  if (s.tty) return { key: `tty:${s.host}/${s.tty}`, label: `${holder} ${s.tty.replace(/^\/dev\//, '')}` };
  return { key: `session:${s.uuid}`, label: `${holder} ${s.uuid.slice(0, 8)}` };
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
      case 'skip':
        out.push(mark('skip', at(m), null));
        break;
      case 'nested':
        out.push({ ...mark('nested', at(m), null), reason: str(m.data.mode), detail: str(m.data.user), child: str(m.data.child_uuid) });
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
  slot: string;
  person: string;
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
  const root = (f: FixSession): FixSession => {
    const seen = new Set([f.session.uuid]);
    let cur = f;
    for (;;) {
      const p = cur.session.parent_uuid ? byUuid.get(cur.session.parent_uuid) : undefined;
      if (!p || seen.has(p.session.uuid)) return cur;
      seen.add(p.session.uuid);
      cur = p;
    }
  };

  const rows = new Map<string, Acc>();
  let total = 0;
  for (const f of found) {
    const r = root(f).session;
    if (user && person(r) !== user) continue;
    const key = `${slot(r).key}\u0000${person(r)}`;
    let acc = rows.get(key);
    if (!acc) {
      acc = { slot: slot(r).key, person: person(r), newest: r, first: Infinity, sessions: [], blocks: [], ends: [], marks: [], busy: [] };
      rows.set(key, acc);
    }
    if (r.started_at >= acc.newest.started_at) acc.newest = r;
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

  const people = new Map<string, Set<string>>();
  const labels = new Map<string, { at: number; label: string }>();
  for (const r of rows.values()) {
    people.set(r.slot, (people.get(r.slot) ?? new Set()).add(r.person));
    const l = labels.get(r.slot);
    if (!l || r.newest.started_at >= l.at) labels.set(r.slot, { at: r.newest.started_at, label: slot(r.newest).label });
  }
  const rowKey = (r: Acc) => (group === 'user' && people.get(r.slot)!.size === 1 ? r.slot : `${r.slot}|user:${r.person}`);

  const lanes = new Map<string, TimelineLane>();
  const ordered = [...rows.values()].sort((a, b) => a.first - b.first || (rowKey(a) < rowKey(b) ? -1 : 1));
  for (const r of ordered) {
    const slotLabel = labels.get(r.slot)!.label;
    const [laneKey, laneLabel, rowLabel] = group === 'user' ? [`user:${r.person}`, r.person, slotLabel] : [r.slot, slotLabel, r.person];
    const blocks = r.blocks.sort((a, b) => a.ts_start - b.ts_start || a.seq - b.seq);
    const row: TimelineRow = {
      key: rowKey(r),
      label: rowLabel,
      host: r.newest.host,
      user: r.person,
      tmux: r.newest.tmux,
      holder_comm: r.newest.holder_comm,
      tty: r.newest.tty,
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
