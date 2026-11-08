// What the timeline shows, kept in the URL (`/timeline?from=&to=&group=`; docs/API.md §9.3):
// the ts window and the grouping. Without `from`/`to` the view follows the last 24 h.

import type { TimelineGroup } from '../../lib/api/types';

export const HOUR = 3600;
export const DAY = 86_400;
/** The server's limit. */
export const MAX_SPAN = 31 * DAY;
/** Zooming in stops here. */
export const MIN_SPAN = 60;

export interface TimelineView {
  /** `null`: follow the last 24 h (the server picks the window). */
  from: number | null;
  to: number | null;
  group: TimelineGroup;
}

const num = (v: string | null): number | null => {
  if (v === null || v.trim() === '') return null;
  const n = Number(v);
  return Number.isFinite(n) ? n : null;
};

/** The view in a query string; a broken or inverted window falls back to following. */
export function viewFromQuery(sp: URLSearchParams): TimelineView {
  const group: TimelineGroup = sp.get('group') === 'pane' ? 'pane' : 'user';
  const from = num(sp.get('from'));
  const to = num(sp.get('to'));
  if (from === null || to === null || from >= to) return { from: null, to: null, group };
  return { from, to, group };
}

/** `keep` holds the other keys of the page (the open session). */
export function viewToQuery(v: TimelineView, keep?: URLSearchParams): URLSearchParams {
  const sp = new URLSearchParams(keep);
  sp.delete('from');
  sp.delete('to');
  sp.delete('group');
  if (v.from !== null && v.to !== null) {
    sp.set('from', String(round(v.from)));
    sp.set('to', String(round(v.to)));
  }
  if (v.group !== 'user') sp.set('group', v.group);
  return sp;
}

const round = (ts: number) => Math.round(ts * 1000) / 1000;

/** `[from, to]` with a span between `MIN_SPAN` and `MAX_SPAN`, around the same centre. */
export function clampWindow(from: number, to: number): { from: number; to: number } {
  const span = Math.min(MAX_SPAN, Math.max(MIN_SPAN, to - from));
  const mid = (from + to) / 2;
  return { from: mid - span / 2, to: mid + span / 2 };
}

/**
 * Zooms by `factor` (< 1 in, > 1 out) keeping `anchor` (a ts inside the window, default the
 * centre) at the same relative position.
 */
export function zoomWindow(from: number, to: number, factor: number, anchor = (from + to) / 2): { from: number; to: number } {
  const cur = to - from;
  // Zooming in never widens a window already below the minimum; zooming out stops at the maximum.
  const span = factor < 1 ? Math.max(cur * factor, Math.min(MIN_SPAN, cur)) : Math.min(cur * factor, Math.max(MAX_SPAN, cur));
  const rel = to > from ? Math.min(1, Math.max(0, (anchor - from) / (to - from))) : 0.5;
  const f = anchor - span * rel;
  return { from: f, to: f + span };
}

/** Moves the window by `fraction` of its span (negative: back in time). */
export function panWindow(from: number, to: number, fraction: number): { from: number; to: number } {
  const d = (to - from) * fraction;
  return { from: from + d, to: to + d };
}

/** The calendar day `YYYY-MM-DD` as a window (local midnight to midnight, or UTC). */
export function dayWindow(day: string, utc: boolean): { from: number; to: number } | null {
  const m = /^(\d{4})-(\d{2})-(\d{2})$/.exec(day);
  if (!m) return null;
  const [y, mo, d] = [Number(m[1]), Number(m[2]) - 1, Number(m[3])];
  const start = utc ? Date.UTC(y, mo, d) : new Date(y, mo, d).getTime();
  const end = utc ? Date.UTC(y, mo, d + 1) : new Date(y, mo, d + 1).getTime();
  if (!Number.isFinite(start) || !Number.isFinite(end)) return null;
  return { from: start / 1000, to: end / 1000 };
}

/** `YYYY-MM-DD` of `ts` (the date picker's value). */
export function dayOf(ts: number, utc: boolean): string {
  const d = new Date(ts * 1000);
  const pad = (n: number) => String(n).padStart(2, '0');
  return utc
    ? `${d.getUTCFullYear()}-${pad(d.getUTCMonth() + 1)}-${pad(d.getUTCDate())}`
    : `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())}`;
}

/** The query with the opened session (`open`, `cmd`) set, or removed for `uuid = null`. */
export function withOpen(query: URLSearchParams, uuid: string | null, cmd: number | null): URLSearchParams {
  const sp = new URLSearchParams(query);
  sp.delete('open');
  sp.delete('cmd');
  if (uuid) sp.set('open', uuid);
  if (uuid && cmd !== null) sp.set('cmd', String(cmd));
  return sp;
}
