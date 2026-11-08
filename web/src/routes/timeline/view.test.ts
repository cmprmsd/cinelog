import { describe, expect, it } from 'vitest';
import {
  DAY,
  MAX_SPAN,
  MIN_SPAN,
  clampWindow,
  dayOf,
  dayWindow,
  panWindow,
  viewFromQuery,
  viewToQuery,
  withOpen,
  zoomWindow,
} from './view';

const q = (s: string) => new URLSearchParams(s);

describe('timeline view in the URL', () => {
  it('follows the last 24 h without a valid window', () => {
    expect(viewFromQuery(q(''))).toEqual({ from: null, to: null, group: 'user' });
    expect(viewFromQuery(q('from=10&to=5&group=pane'))).toEqual({ from: null, to: null, group: 'pane' });
    expect(viewFromQuery(q('from=x&to=5'))).toEqual({ from: null, to: null, group: 'user' });
    expect(viewFromQuery(q('from=10'))).toEqual({ from: null, to: null, group: 'user' });
    expect(viewFromQuery(q('group=lane'))).toEqual({ from: null, to: null, group: 'user' });
  });

  it('round-trips and keeps the other keys', () => {
    const v = { from: 1000.12345, to: 2000, group: 'pane' as const };
    const sp = viewToQuery(v, q('open=abc&from=1'));
    expect(sp.get('open')).toBe('abc');
    expect(sp.toString()).toBe('open=abc&from=1000.123&to=2000&group=pane');
    expect(viewFromQuery(sp)).toEqual({ ...v, from: 1000.123 });
    expect(viewToQuery({ from: null, to: null, group: 'user' }, q('from=1&to=2')).toString()).toBe('');
  });

  it('sets and clears the opened command', () => {
    expect(withOpen(q('from=1&to=2'), 'u1', 3).toString()).toBe('from=1&to=2&open=u1&cmd=3');
    expect(withOpen(q('open=u1&cmd=3'), 'u2', null).toString()).toBe('open=u2');
    expect(withOpen(q('open=u1&cmd=3&group=pane'), null, 4).toString()).toBe('group=pane');
  });
});

describe('window arithmetic', () => {
  it('zooms around an anchor within the limits', () => {
    expect(zoomWindow(0, 1000, 0.5)).toEqual({ from: 250, to: 750 });
    expect(zoomWindow(0, 1000, 0.5, 0)).toEqual({ from: 0, to: 500 });
    expect(zoomWindow(0, 1000, 2, 1000)).toEqual({ from: -1000, to: 1000 });
    const tiny = zoomWindow(0, 100, 0.1);
    expect(tiny.to - tiny.from).toBe(MIN_SPAN);
    expect(zoomWindow(0, 30, 0.5)).toEqual({ from: 0, to: 30 });
    const huge = zoomWindow(0, 30 * DAY, 4);
    expect(huge.to - huge.from).toBe(MAX_SPAN);
  });

  it('pans and clamps', () => {
    expect(panWindow(100, 200, -0.5)).toEqual({ from: 50, to: 150 });
    expect(clampWindow(0, 10)).toEqual({ from: -25, to: 35 });
    expect(clampWindow(0, 40 * DAY).to - clampWindow(0, 40 * DAY).from).toBe(MAX_SPAN);
  });

  it('turns a picked day into a window', () => {
    expect(dayWindow('2026-11-08', true)).toEqual({ from: Date.UTC(2026, 10, 8) / 1000, to: Date.UTC(2026, 10, 9) / 1000 });
    const local = dayWindow('2026-11-08', false)!;
    expect(local.from).toBe(new Date(2026, 10, 8).getTime() / 1000);
    expect(dayOf(local.from, false)).toBe('2026-11-08');
    expect(dayOf(Date.UTC(2026, 0, 31, 23, 59) / 1000, true)).toBe('2026-01-31');
    expect(dayWindow('08.11.2026', true)).toBeNull();
  });
});
