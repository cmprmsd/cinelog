// The timeline's time axis: ts → x, piecewise linear. Idle spans (no command in any row for
// at least 10 min, `Timeline.idle`) collapse to a fixed-width break (plan §8.3), the rest
// shares the width in proportion to its length.

import type { TimeSpan } from '../../lib/api/types';

/** Width of a collapsed idle break in px. */
export const BREAK_PX = 14;

export interface Piece {
  from: number;
  to: number;
  x0: number;
  x1: number;
  idle: boolean;
}

export class TimeScale {
  readonly from: number;
  readonly to: number;
  readonly width: number;
  readonly pieces: readonly Piece[];
  /** px per second outside the breaks. */
  readonly pxPerSec: number;

  constructor(from: number, to: number, width: number, idle: readonly TimeSpan[] = [], collapse = true) {
    this.from = from;
    this.to = to;
    this.width = Math.max(1, width);
    const breaks = collapse ? usableBreaks(from, to, idle, this.width) : [];
    const idleSecs = breaks.reduce((n, b) => n + (b.to - b.from), 0);
    const activeSecs = to - from - idleSecs;
    const activePx = this.width - breaks.length * BREAK_PX;
    this.pxPerSec = activeSecs > 0 ? activePx / activeSecs : this.width / Math.max(1e-9, to - from);
    const pieces: Piece[] = [];
    let ts = from;
    let x = 0;
    const active = (end: number) => {
      if (end <= ts) return;
      const x1 = x + (end - ts) * this.pxPerSec;
      pieces.push({ from: ts, to: end, x0: x, x1, idle: false });
      ts = end;
      x = x1;
    };
    for (const b of breaks) {
      active(b.from);
      pieces.push({ from: b.from, to: b.to, x0: x, x1: x + BREAK_PX, idle: true });
      ts = b.to;
      x += BREAK_PX;
    }
    active(to);
    if (pieces.length === 0) pieces.push({ from, to, x0: 0, x1: this.width, idle: false });
    this.pieces = pieces;
  }

  private pieceAt(ts: number): Piece {
    const p = this.pieces;
    let lo = 0;
    let hi = p.length - 1;
    while (lo < hi) {
      const mid = (lo + hi) >> 1;
      if (p[mid]!.to < ts) lo = mid + 1;
      else hi = mid;
    }
    return p[lo]!;
  }

  /** x of `ts`; outside the window it continues at the active slope. */
  x(ts: number): number {
    if (ts <= this.from) return (ts - this.from) * this.pxPerSec;
    if (ts >= this.to) return this.width + (ts - this.to) * this.pxPerSec;
    const p = this.pieceAt(ts);
    const span = p.to - p.from;
    return span > 0 ? p.x0 + ((ts - p.from) / span) * (p.x1 - p.x0) : p.x0;
  }

  /** ts at `x` (clamped to the window). */
  ts(x: number): number {
    if (x <= 0) return this.from;
    if (x >= this.width) return this.to;
    for (const p of this.pieces) {
      if (x <= p.x1) {
        const w = p.x1 - p.x0;
        return w > 0 ? p.from + ((x - p.x0) / w) * (p.to - p.from) : p.from;
      }
    }
    return this.to;
  }

  breaks(): Piece[] {
    return this.pieces.filter((p) => p.idle);
  }
}

/**
 * The idle spans worth collapsing: inside the window, and only while the active part keeps
 * at least half the width (a nearly idle window stays linear).
 */
function usableBreaks(from: number, to: number, idle: readonly TimeSpan[], width: number): TimeSpan[] {
  const list = idle
    .map((s) => ({ from: Math.max(from, s.from), to: Math.min(to, s.to) }))
    .filter((s) => s.to > s.from)
    .sort((a, b) => a.from - b.from);
  const natural = width / (to - from);
  const worth = list.filter((s) => (s.to - s.from) * natural > BREAK_PX * 2);
  const idleSecs = worth.reduce((n, s) => n + (s.to - s.from), 0);
  if (to - from - idleSecs <= 0 || width - worth.length * BREAK_PX < width / 2) return [];
  return worth;
}

// ------------------------------------------------------------------ ticks

export interface Tick {
  ts: number;
  x: number;
  label: string;
  /** A day boundary (drawn stronger, labelled with the date). */
  major: boolean;
}

const MIN = 60;
const STEPS = [1, 2, 5, 10, 15, 30, MIN, 5 * MIN, 15 * MIN, 30 * MIN, 3600, 3 * 3600, 6 * 3600, 12 * 3600, 86_400, 2 * 86_400, 7 * 86_400];

/** Seconds east of UTC at `ts` in the display zone. */
function offsetAt(ts: number, utc: boolean): number {
  return utc ? 0 : -new Date(ts * 1000).getTimezoneOffset() * 60;
}

const pad = (n: number) => String(n).padStart(2, '0');

function parts(ts: number, utc: boolean): { mo: number; d: number; h: number; mi: number; s: number; wd: number } {
  const t = new Date(ts * 1000);
  const s = t.getUTCSeconds();
  return utc
    ? { mo: t.getUTCMonth() + 1, d: t.getUTCDate(), h: t.getUTCHours(), mi: t.getUTCMinutes(), s, wd: t.getUTCDay() }
    : { mo: t.getMonth() + 1, d: t.getDate(), h: t.getHours(), mi: t.getMinutes(), s, wd: t.getDay() };
}

const WEEKDAY = ['Sun', 'Mon', 'Tue', 'Wed', 'Thu', 'Fri', 'Sat'];

export function tickLabel(ts: number, step: number, utc: boolean): { label: string; major: boolean } {
  const p = parts(ts, utc);
  const midnight = p.h === 0 && p.mi === 0;
  if (step >= 86_400 || (midnight && p.s === 0)) return { label: `${WEEKDAY[p.wd]} ${pad(p.mo)}-${pad(p.d)}`, major: true };
  if (step < MIN) return { label: `${pad(p.h)}:${pad(p.mi)}:${pad(p.s)}`, major: false };
  return { label: `${pad(p.h)}:${pad(p.mi)}`, major: false };
}

/** Axis ticks at round local (or UTC) times, at least `minPx` apart, none inside a break. */
export function ticks(scale: TimeScale, utc: boolean, minPx = 90): Tick[] {
  const step = STEPS.find((s) => s * scale.pxPerSec >= minPx) ?? STEPS[STEPS.length - 1]!;
  const out: Tick[] = [];
  let lastX = -Infinity;
  for (const p of scale.pieces) {
    if (p.idle) continue;
    const off = offsetAt(p.from, utc);
    let ts = Math.ceil((p.from + off) / step) * step - off;
    for (let guard = 0; ts <= p.to && guard < 1000; guard += 1) {
      // Re-align after a DST change inside the piece.
      const o = offsetAt(ts, utc);
      if (o !== off && step >= 3600) ts = Math.round((ts + o) / step) * step - o;
      const x = scale.x(ts);
      if (x - lastX >= minPx / 2 && x >= 0 && x <= scale.width) {
        out.push({ ts, x, ...tickLabel(ts, step, utc) });
        lastX = x;
      }
      ts += step;
    }
  }
  return out;
}
