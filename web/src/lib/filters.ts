// Search state <-> URL query (shareable links) <-> API `SearchParams`.

import type { SearchIn, SearchParams } from './api/types';

export type DatePreset = 'any' | 'today' | '7d' | '30d' | 'custom';

export interface SearchState {
  q: string;
  in: SearchIn;
  case: boolean;
  user: string;
  host: string;
  date: DatePreset;
  /** Custom range bounds (epoch seconds), only with `date = custom`. */
  from: number | null;
  to: number | null;
  /** `''`, `ok`, `fail` or an integer. */
  exit: string;
  cwd: string;
  pane: string;
  hidden: boolean;
}

export const EMPTY: SearchState = {
  q: '',
  in: 'cmd',
  case: false,
  user: '',
  host: '',
  date: 'any',
  from: null,
  to: null,
  exit: '',
  cwd: '',
  pane: '',
  hidden: false,
};

const MODES: readonly SearchIn[] = ['cmd', 'out', 'both'];
const PRESETS: readonly DatePreset[] = ['any', 'today', '7d', '30d', 'custom'];

export function normalizeExit(v: string | null | undefined): string {
  const s = (v ?? '').trim().toLowerCase();
  if (s === 'ok' || s === 'fail') return s;
  return /^-?\d{1,10}$/.test(s) ? String(Number(s)) : '';
}

function epoch(v: string | null): number | null {
  if (v === null || v.trim() === '') return null;
  const n = Number(v);
  return Number.isFinite(n) && n >= 0 ? n : null;
}

export function stateFromQuery(sp: URLSearchParams): SearchState {
  const mode = sp.get('in') as SearchIn | null;
  const date = sp.get('date') as DatePreset | null;
  const s: SearchState = {
    q: sp.get('q') ?? '',
    in: mode && MODES.includes(mode) ? mode : 'cmd',
    case: sp.get('case') === '1',
    user: sp.get('user') ?? '',
    host: sp.get('host') ?? '',
    date: date && PRESETS.includes(date) ? date : 'any',
    from: epoch(sp.get('from')),
    to: epoch(sp.get('to')),
    exit: normalizeExit(sp.get('exit')),
    cwd: sp.get('cwd') ?? '',
    pane: sp.get('pane') ?? '',
    hidden: sp.get('hidden') === '1',
  };
  if (s.date !== 'custom') {
    s.from = null;
    s.to = null;
  } else if (s.from === null && s.to === null) {
    s.date = 'any';
  }
  return s;
}

/**
 * The query string for a state; defaults are left out. Keys of `keep` that are not search
 * keys (for example `cmd` or `t` on a session page) are carried over.
 */
export function stateToQuery(s: SearchState, keep?: URLSearchParams): URLSearchParams {
  const sp = new URLSearchParams();
  if (keep) {
    for (const [k, v] of keep) if (!SEARCH_KEYS.has(k)) sp.append(k, v);
  }
  if (s.q) sp.set('q', s.q);
  if (s.in !== 'cmd') sp.set('in', s.in);
  if (s.case) sp.set('case', '1');
  if (s.user) sp.set('user', s.user);
  if (s.host) sp.set('host', s.host);
  if (s.date !== 'any') sp.set('date', s.date);
  if (s.date === 'custom') {
    if (s.from !== null) sp.set('from', String(s.from));
    if (s.to !== null) sp.set('to', String(s.to));
  }
  if (s.exit) sp.set('exit', s.exit);
  if (s.cwd) sp.set('cwd', s.cwd);
  if (s.pane) sp.set('pane', s.pane);
  if (s.hidden) sp.set('hidden', '1');
  return sp;
}

const SEARCH_KEYS = new Set(['q', 'in', 'case', 'user', 'host', 'date', 'from', 'to', 'exit', 'cwd', 'pane', 'hidden']);

/** Absolute `ts` bounds for the date filter, relative to `now`. */
export function dateBounds(s: SearchState, now: Date): { from?: number; to?: number } {
  const secs = (d: Date) => Math.floor(d.getTime() / 1000);
  switch (s.date) {
    case 'today': {
      const start = new Date(now);
      start.setHours(0, 0, 0, 0);
      return { from: secs(start) };
    }
    case '7d':
      return { from: secs(now) - 7 * 86400 };
    case '30d':
      return { from: secs(now) - 30 * 86400 };
    case 'custom':
      return { from: s.from ?? undefined, to: s.to ?? undefined };
    default:
      return {};
  }
}

export function toSearchParams(s: SearchState, now: Date, extra: Partial<SearchParams> = {}): SearchParams {
  const p: SearchParams = { in: s.in, ...dateBounds(s, now), ...extra };
  if (s.q) p.q = s.q;
  if (s.case) p.case = true;
  if (s.user) p.user = s.user;
  if (s.host) p.host = s.host;
  if (s.exit) p.exit = s.exit;
  if (s.cwd) p.cwd = s.cwd;
  if (s.pane) p.pane = s.pane;
  if (s.hidden) p.hidden = true;
  return p;
}

export type FilterKey = 'user' | 'host' | 'date' | 'exit' | 'cwd' | 'pane' | 'hidden';

export interface Chip {
  key: FilterKey;
  label: string;
}

const DATE_LABELS: Record<DatePreset, string> = {
  any: 'any time',
  today: 'today',
  '7d': 'last 7 days',
  '30d': 'last 30 days',
  custom: 'custom range',
};

export function exitLabel(exit: string): string {
  if (exit === 'ok') return 'exit 0';
  if (exit === 'fail') return 'exit ≠0';
  return `exit ${exit}`;
}

/** The active filters as removable chips, in a fixed order. */
export function chips(s: SearchState, fmt: (ts: number) => string): Chip[] {
  const out: Chip[] = [];
  if (s.user) out.push({ key: 'user', label: `user: ${s.user}` });
  if (s.host) out.push({ key: 'host', label: `host: ${s.host}` });
  if (s.date === 'custom') {
    const a = s.from !== null ? fmt(s.from) : '…';
    const b = s.to !== null ? fmt(s.to) : '…';
    out.push({ key: 'date', label: `${a} – ${b}` });
  } else if (s.date !== 'any') {
    out.push({ key: 'date', label: DATE_LABELS[s.date] });
  }
  if (s.exit) out.push({ key: 'exit', label: exitLabel(s.exit) });
  if (s.cwd) out.push({ key: 'cwd', label: `cwd: ${s.cwd}` });
  if (s.pane) out.push({ key: 'pane', label: `pane: ${s.pane}` });
  if (s.hidden) out.push({ key: 'hidden', label: 'incl. hidden' });
  return out;
}

export function clearFilter(s: SearchState, key: FilterKey): SearchState {
  switch (key) {
    case 'date':
      return { ...s, date: 'any', from: null, to: null };
    case 'hidden':
      return { ...s, hidden: false };
    default:
      return { ...s, [key]: '' };
  }
}

export { DATE_LABELS };
