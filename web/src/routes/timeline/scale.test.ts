import { describe, expect, it } from 'vitest';
import { BREAK_PX, TimeScale, tickLabel, ticks } from './scale';

describe('TimeScale', () => {
  it('is linear without breaks', () => {
    const s = new TimeScale(1000, 2000, 500);
    expect(s.x(1000)).toBe(0);
    expect(s.x(1500)).toBe(250);
    expect(s.x(2000)).toBe(500);
    expect(s.x(2100)).toBe(550);
    expect(s.ts(250)).toBe(1500);
    expect(s.ts(-5)).toBe(1000);
    expect(s.breaks()).toEqual([]);
  });

  it('collapses idle spans to breaks and inverts', () => {
    // 10 000 s, 8000 of them idle: the remaining 2000 s share 1000 - 14 px.
    const s = new TimeScale(0, 10_000, 1000, [{ from: 1000, to: 9000 }]);
    const [b] = s.breaks();
    expect(b).toBeDefined();
    expect(b!.x1 - b!.x0).toBe(BREAK_PX);
    expect(s.pxPerSec).toBeCloseTo((1000 - BREAK_PX) / 2000);
    expect(s.x(1000)).toBeCloseTo(b!.x0);
    expect(s.x(9000)).toBeCloseTo(b!.x1);
    expect(s.x(10_000)).toBeCloseTo(1000);
    for (const ts of [0, 500, 999, 5000, 9001, 9999]) expect(s.ts(s.x(ts))).toBeCloseTo(ts, 6);
    expect(s.x(5000)).toBeGreaterThan(b!.x0);
    expect(s.x(5000)).toBeLessThan(b!.x1);
  });

  it('keeps short or overwhelming idle spans linear', () => {
    expect(new TimeScale(0, 100_000, 1000, [{ from: 0, to: 100_000 }]).breaks()).toEqual([]);
    // 10 px of natural width is not worth a 14 px break.
    expect(new TimeScale(0, 100_000, 1000, [{ from: 10_000, to: 11_000 }]).breaks()).toEqual([]);
    const spans = Array.from({ length: 60 }, (_, i) => ({ from: i * 1000 + 100, to: i * 1000 + 900 }));
    expect(new TimeScale(0, 60_000, 1000, spans).breaks()).toEqual([]);
    expect(new TimeScale(0, 10_000, 1000, [{ from: 1000, to: 9000 }], false).breaks()).toEqual([]);
  });

  it('clips idle spans to the window', () => {
    const s = new TimeScale(0, 10_000, 1000, [{ from: -500, to: 4000 }]);
    expect(s.pieces[0]).toMatchObject({ from: 0, to: 4000, idle: true, x0: 0 });
  });
});

describe('ticks', () => {
  it('are round, ordered and spaced', () => {
    const s = new TimeScale(Date.UTC(2026, 10, 8, 10, 7) / 1000, Date.UTC(2026, 10, 8, 16, 0) / 1000, 1200);
    const t = ticks(s, true);
    expect(t.length).toBeGreaterThan(2);
    for (const k of t) expect(k.ts % 1800).toBe(0);
    for (let i = 1; i < t.length; i += 1) expect(t[i]!.x - t[i - 1]!.x).toBeGreaterThanOrEqual(45);
    expect(t[0]!.label).toBe('10:30');
  });

  it('go down to seconds in short windows', () => {
    const s = new TimeScale(Date.UTC(2026, 10, 8, 10, 0, 3) / 1000, Date.UTC(2026, 10, 8, 10, 0, 33) / 1000, 1000);
    const t = ticks(s, true);
    expect(t.map((k) => k.label)).toEqual(['10:00:05', '10:00:10', '10:00:15', '10:00:20', '10:00:25', '10:00:30']);
  });

  it('mark days', () => {
    expect(tickLabel(Date.UTC(2026, 10, 8) / 1000, 3600, true)).toEqual({ label: 'Sun 11-08', major: true });
    expect(tickLabel(Date.UTC(2026, 10, 8, 13, 5) / 1000, 300, true)).toEqual({ label: '13:05', major: false });
    const week = new TimeScale(Date.UTC(2026, 10, 1) / 1000, Date.UTC(2026, 10, 8) / 1000, 1400);
    const t = ticks(week, true);
    expect(t.filter((k) => k.major).length).toBeGreaterThanOrEqual(6);
  });

  it('skip breaks', () => {
    const s = new TimeScale(0, 86_400, 1000, [{ from: 3600, to: 80_000 }]);
    const [b] = s.breaks();
    for (const k of ticks(s, true)) expect(k.ts <= 3600 || k.ts >= 80_000).toBe(true);
    expect(b).toBeDefined();
  });
});
