import { describe, expect, it } from 'vitest';
import { chips, clearFilter, dateBounds, EMPTY, normalizeExit, stateFromQuery, stateToQuery, toSearchParams } from './filters';

const q = (s: string) => new URLSearchParams(s);

describe('filters', () => {
  it('round-trips through the URL and leaves defaults out', () => {
    const s = stateFromQuery(q('q=needle&in=both&case=1&user=alice&exit=fail&cwd=%2Fsrv&pane=%253&hidden=1'));
    expect(s).toMatchObject({ q: 'needle', in: 'both', case: true, user: 'alice', exit: 'fail', cwd: '/srv', pane: '%3', hidden: true });
    expect(stateFromQuery(stateToQuery(s))).toEqual(s);
    expect(stateToQuery(EMPTY).toString()).toBe('');
  });

  it('rejects malformed values', () => {
    const s = stateFromQuery(q('in=bogus&date=yesterday&exit=abc&from=-5'));
    expect(s.in).toBe('cmd');
    expect(s.date).toBe('any');
    expect(s.exit).toBe('');
    expect(s.from).toBeNull();
  });

  it('keeps custom bounds only with date=custom', () => {
    expect(stateFromQuery(q('date=7d&from=10&to=20'))).toMatchObject({ date: '7d', from: null, to: null });
    expect(stateFromQuery(q('date=custom&from=10'))).toMatchObject({ date: 'custom', from: 10, to: null });
    expect(stateFromQuery(q('date=custom')).date).toBe('any');
  });

  it('carries non-search keys over', () => {
    const sp = stateToQuery({ ...EMPTY, q: 'x' }, q('cmd=3&q=old&exit=1'));
    expect(sp.get('cmd')).toBe('3');
    expect(sp.get('q')).toBe('x');
    expect(sp.has('exit')).toBe(false);
  });

  it('normalizes exit filters', () => {
    expect(normalizeExit(' OK ')).toBe('ok');
    expect(normalizeExit('007')).toBe('7');
    expect(normalizeExit('-1')).toBe('-1');
    expect(normalizeExit('1.5')).toBe('');
  });

  it('computes date bounds', () => {
    const now = new Date(2026, 10, 8, 15, 30, 0);
    const secs = Math.floor(now.getTime() / 1000);
    expect(dateBounds({ ...EMPTY, date: '7d' }, now)).toEqual({ from: secs - 7 * 86400 });
    expect(dateBounds({ ...EMPTY, date: 'today' }, now)).toEqual({ from: Math.floor(new Date(2026, 10, 8).getTime() / 1000) });
    expect(dateBounds({ ...EMPTY, date: 'custom', from: 5, to: null }, now)).toEqual({ from: 5, to: undefined });
    expect(dateBounds(EMPTY, now)).toEqual({});
  });

  it('builds API parameters', () => {
    const p = toSearchParams({ ...EMPTY, q: 'needle', in: 'out', exit: 'ok', pane: '%1' }, new Date(), { limit: 10 });
    expect(p).toEqual({ in: 'out', q: 'needle', exit: 'ok', pane: '%1', limit: 10 });
  });

  it('lists and clears chips', () => {
    const s = { ...EMPTY, user: 'bob', exit: 'fail', date: '30d' as const, hidden: true };
    expect(chips(s, String).map((c) => c.label)).toEqual(['user: bob', 'last 30 days', 'exit ≠0', 'incl. hidden']);
    expect(clearFilter(s, 'user').user).toBe('');
    expect(clearFilter(s, 'date')).toMatchObject({ date: 'any', from: null, to: null });
    expect(clearFilter(s, 'hidden').hidden).toBe(false);
  });
});
