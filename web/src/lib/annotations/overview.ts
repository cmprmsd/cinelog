// The annotations overview (`/annotations`, docs/API.md §9.4 "Overview", §9.6): filter state
// <-> URL query <-> `AnnotationParams`, the local filter match for bus events, and the
// newest-first list the page keeps.

import type { Annotation, AnnotationParams, SnippetPart } from '../api/types';
import { dateBounds, DATE_LABELS, parseTags, type DatePreset } from '../filters';

export interface OverviewState {
  /** Normalised, sorted; an annotation must carry all of them. */
  tags: string[];
  /** Exact principal name. */
  author: string;
  /** Body text. */
  q: string;
  date: DatePreset;
  /** `created_at` bounds (epoch seconds), only with `date = custom`. */
  from: number | null;
  to: number | null;
}

export const OVERVIEW_EMPTY: OverviewState = { tags: [], author: '', q: '', date: 'any', from: null, to: null };

/** Longest `q` the server takes. */
export const MAX_Q = 200;
export const PAGE_SIZE = 50;

const PRESETS: readonly DatePreset[] = ['any', 'today', '7d', '30d', 'custom'];

function epoch(v: string | null): number | null {
  if (v === null || v.trim() === '') return null;
  const n = Number(v);
  return Number.isFinite(n) && n >= 0 ? n : null;
}

export function overviewFromQuery(sp: URLSearchParams): OverviewState {
  const date = sp.get('date') as DatePreset | null;
  const s: OverviewState = {
    tags: parseTags(sp.get('tag')),
    author: (sp.get('author') ?? '').trim(),
    q: (sp.get('q') ?? '').slice(0, MAX_Q),
    date: date && PRESETS.includes(date) ? date : 'any',
    from: epoch(sp.get('from')),
    to: epoch(sp.get('to')),
  };
  if (s.date !== 'custom') {
    s.from = null;
    s.to = null;
  } else if (s.from === null && s.to === null) {
    s.date = 'any';
  }
  return s;
}

/** The query string for a state; empty values and defaults are left out. */
export function overviewToQuery(s: OverviewState): URLSearchParams {
  const sp = new URLSearchParams();
  if (s.tags.length) sp.set('tag', s.tags.join(','));
  if (s.author) sp.set('author', s.author);
  if (s.q) sp.set('q', s.q);
  if (s.date !== 'any') sp.set('date', s.date);
  if (s.date === 'custom') {
    if (s.from !== null) sp.set('from', String(s.from));
    if (s.to !== null) sp.set('to', String(s.to));
  }
  return sp;
}

/** True when any filter is set. */
export const isFiltered = (s: OverviewState) => s.tags.length > 0 || !!s.author || !!s.q.trim() || s.date !== 'any';

/** `created_at` bounds of the state, relative to `now`; a custom range is put in order. */
function bounds(s: OverviewState, now: Date): { from?: number; to?: number } {
  const b = dateBounds(s, now);
  if (b.from !== undefined && b.to !== undefined && b.from > b.to) return { from: b.to, to: b.from };
  return b;
}

/** The first page's request (newest first, with context). */
export function toAnnotationParams(s: OverviewState, now: Date, extra: Partial<AnnotationParams> = {}): AnnotationParams {
  const p: AnnotationParams = { order: 'desc', context: true, limit: PAGE_SIZE, ...bounds(s, now), ...extra };
  if (s.tags.length) p.tag = s.tags.join(',');
  if (s.author) p.author = s.author;
  if (s.q.trim()) p.q = s.q.trim();
  return p;
}

/** Whether `a` passes the filters, as the server decides it (the bus recipe of §9.4). */
export function matches(a: Annotation, s: OverviewState, now: Date): boolean {
  if (a.deleted_at !== null) return false;
  if (!s.tags.every((t) => a.tags.includes(t))) return false;
  if (s.author && a.author !== s.author) return false;
  const q = s.q.trim().toLowerCase();
  if (q && !a.body.toLowerCase().includes(q)) return false;
  const b = bounds(s, now);
  if (b.from !== undefined && a.created_at < b.from) return false;
  if (b.to !== undefined && a.created_at > b.to) return false;
  return true;
}

/** Newest first, uuid descending breaking ties (the API's `order=desc`). */
export function compareDesc(a: Annotation, b: Annotation): number {
  return b.created_at - a.created_at || (a.uuid < b.uuid ? 1 : a.uuid > b.uuid ? -1 : 0);
}

/** `list` (newest first) with the rows of a later page appended; rows already listed are skipped. */
export function appendPage(list: readonly Annotation[], page: readonly Annotation[]): Annotation[] {
  const have = new Set(list.map((a) => a.uuid));
  return [...list, ...page.filter((a) => !have.has(a.uuid))];
}

/**
 * Where a matching annotation that is not listed belongs: true when it sorts within the
 * loaded part of the list (or everything is loaded); false when a later page brings it.
 */
export function belongsLoaded(list: readonly Annotation[], a: Annotation, more: boolean): boolean {
  const last = list.at(-1);
  return !more || last === undefined || compareDesc(a, last) < 0;
}

/** `list` with `a` inserted in order or replacing its listed version (an older `a` changes nothing). */
export function insertDesc(list: readonly Annotation[], a: Annotation): Annotation[] {
  const i = list.findIndex((x) => x.uuid === a.uuid);
  if (i >= 0) {
    if (list[i]!.updated_at > a.updated_at) return [...list];
    const out = [...list];
    out[i] = a;
    return out;
  }
  return [...list, a].sort(compareDesc);
}

/** The first `lines` lines of a body (trailing blank lines dropped) and whether more follows. */
export function preview(body: string, lines = 3, chars = 400): { text: string; more: boolean } {
  const all = body.replace(/\r\n?/g, '\n').split('\n');
  let text = all.slice(0, lines).join('\n').trimEnd();
  let more = all.length > lines && all.slice(lines).some((l) => l.trim() !== '');
  if (text.length > chars) {
    text = `${text.slice(0, chars - 1)}…`;
    more = true;
  }
  return { text, more };
}

/** `text` split on case-insensitive matches of `q` (plain text parts for `Snippet`-like rendering). */
export function markText(text: string, q: string): SnippetPart[] {
  const needle = q.trim().toLowerCase();
  const hay = text.toLowerCase();
  // Lowercasing can change the length (`İ`): then indices do not line up, so mark nothing.
  if (!needle || hay.length !== text.length) return [{ text, hit: false }];
  const parts: SnippetPart[] = [];
  let at = 0;
  for (let i = hay.indexOf(needle); i >= 0; i = hay.indexOf(needle, i + needle.length)) {
    if (i > at) parts.push({ text: text.slice(at, i), hit: false });
    parts.push({ text: text.slice(i, i + needle.length), hit: true });
    at = i + needle.length;
  }
  if (at < text.length) parts.push({ text: text.slice(at), hit: false });
  return parts.length ? parts : [{ text, hit: false }];
}

export type OverviewKey = 'tag' | 'author' | 'q' | 'date';

export interface OverviewChip {
  key: OverviewKey;
  label: string;
  /** The tag of a `tag` chip. */
  value?: string;
}

/** The active filters as removable chips. */
export function overviewChips(s: OverviewState, fmt: (ts: number) => string): OverviewChip[] {
  const out: OverviewChip[] = s.tags.map((t) => ({ key: 'tag', label: `#${t}`, value: t }));
  if (s.author) out.push({ key: 'author', label: `by ${s.author}` });
  if (s.q) out.push({ key: 'q', label: `“${s.q}”` });
  if (s.date === 'custom') {
    out.push({ key: 'date', label: `${s.from !== null ? fmt(s.from) : '…'} – ${s.to !== null ? fmt(s.to) : '…'}` });
  } else if (s.date !== 'any') {
    out.push({ key: 'date', label: DATE_LABELS[s.date] });
  }
  return out;
}

export function clearOverview(s: OverviewState, key: OverviewKey, value?: string): OverviewState {
  switch (key) {
    case 'tag':
      return { ...s, tags: value === undefined ? [] : s.tags.filter((t) => t !== value) };
    case 'date':
      return { ...s, date: 'any', from: null, to: null };
    default:
      return { ...s, [key]: '' };
  }
}
