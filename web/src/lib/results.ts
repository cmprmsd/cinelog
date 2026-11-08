// Result grouping (hits of one command collapse into "N more hits") and list windowing.

import type { SearchHit } from './api/types';

export interface HitGroup {
  key: string;
  head: SearchHit;
  rest: SearchHit[];
}

/** Hits of the same command share a group; gap output (no command) stands alone. */
export function groupKey(h: SearchHit): string {
  return h.cmd_seq === null ? h.id : `${h.session}:${h.cmd_seq}`;
}

/** Appends a page of hits, merging into existing groups. Order is first appearance. */
export function appendHits(groups: readonly HitGroup[], hits: readonly SearchHit[]): HitGroup[] {
  const out = groups.map((g) => ({ ...g, rest: [...g.rest] }));
  const index = new Map(out.map((g, i) => [g.key, i]));
  const seen = new Set(out.flatMap((g) => [g.head.id, ...g.rest.map((h) => h.id)]));
  for (const h of hits) {
    if (seen.has(h.id)) continue;
    seen.add(h.id);
    const key = groupKey(h);
    const i = index.get(key);
    if (i === undefined) {
      index.set(key, out.length);
      out.push({ key, head: h, rest: [] });
    } else {
      out[i]!.rest.push(h);
    }
  }
  return out;
}

export interface Row {
  hit: SearchHit;
  group: HitGroup;
  /** The group's first row (carries the "N more hits" toggle). */
  head: boolean;
}

export function flattenRows(groups: readonly HitGroup[], expanded: ReadonlySet<string>): Row[] {
  const rows: Row[] = [];
  for (const group of groups) {
    rows.push({ hit: group.head, group, head: true });
    if (expanded.has(group.key)) for (const hit of group.rest) rows.push({ hit, group, head: false });
  }
  return rows;
}

/** Rows `[start, end)` to render for a fixed row height, with `overscan` rows either side. */
export function visibleRange(
  scrollTop: number,
  viewport: number,
  rowHeight: number,
  count: number,
  overscan = 6,
): [number, number] {
  if (count <= 0 || rowHeight <= 0) return [0, 0];
  const first = Math.floor(Math.max(0, scrollTop) / rowHeight);
  const visible = Math.ceil(Math.max(0, viewport) / rowHeight) + 1;
  const start = Math.max(0, first - overscan);
  const end = Math.min(count, first + visible + overscan);
  return [Math.min(start, end), end];
}

/** The scrollTop that brings row `i` fully into view, or `null` if it already is. */
export function scrollToRow(i: number, scrollTop: number, viewport: number, rowHeight: number): number | null {
  const top = i * rowHeight;
  const bottom = top + rowHeight;
  if (top < scrollTop) return top;
  if (bottom > scrollTop + viewport) return Math.max(0, bottom - viewport);
  return null;
}

/** Groups without the hits `drop` matches; a group whose head goes is led by its next hit. */
export function removeHits(groups: readonly HitGroup[], drop: (h: SearchHit) => boolean): HitGroup[] {
  const out: HitGroup[] = [];
  for (const g of groups) {
    const [head, ...rest] = [g.head, ...g.rest].filter((h) => !drop(h));
    if (head) out.push({ key: g.key, head, rest });
  }
  return out;
}

export interface HitPage {
  hits: readonly Pick<SearchHit, 't'>[];
  next: string | null;
  partial: boolean;
  capped: boolean;
}

export interface HitTimes {
  /** Distinct hit times, ascending. */
  times: number[];
  /** The server or the page cap left hits out. */
  truncated: boolean;
}

/** Follows search cursors (at most `maxPages`) and collects the hit times of every page. */
export async function collectHitTimes(
  fetchPage: (cursor: string | undefined) => Promise<HitPage>,
  maxPages: number,
): Promise<HitTimes> {
  const times = new Set<number>();
  let truncated = false;
  let cursor: string | undefined;
  for (let page = 0; page < maxPages; page += 1) {
    const r = await fetchPage(cursor);
    for (const h of r.hits) times.add(h.t);
    truncated ||= r.partial || r.capped;
    if (!r.next) return { times: [...times].sort((a, b) => a - b), truncated };
    cursor = r.next;
  }
  return { times: [...times].sort((a, b) => a - b), truncated: true };
}
