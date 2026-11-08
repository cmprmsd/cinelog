import { describe, expect, it } from 'vitest';
import { appendHits, collectHitTimes, flattenRows, groupKey, removeHits, scrollToRow, visibleRange, type HitPage } from './results';
import { hit } from './test-data';

describe('results', () => {
  it('groups hits of one command and keeps first-appearance order', () => {
    const a1 = hit({ id: 'o:1', kind: 'output', cmd_seq: 4 });
    const b = hit({ id: 'c:2', cmd_seq: 5 });
    const a2 = hit({ id: 'o:2', kind: 'output', cmd_seq: 4 });
    const gap = hit({ id: 'o:9', kind: 'output', cmd_seq: null });
    const groups = appendHits([], [a1, b, a2, gap]);
    expect(groups.map((g) => g.key)).toEqual([groupKey(a1), groupKey(b), 'o:9']);
    expect(groups[0]!.rest.map((h) => h.id)).toEqual(['o:2']);
  });

  it('merges a later page and drops duplicates', () => {
    const first = appendHits([], [hit({ id: 'c:1', cmd_seq: 1 })]);
    const merged = appendHits(first, [hit({ id: 'o:3', cmd_seq: 1 }), hit({ id: 'c:1', cmd_seq: 1 })]);
    expect(merged).toHaveLength(1);
    expect(merged[0]!.rest.map((h) => h.id)).toEqual(['o:3']);
    expect(first[0]!.rest).toHaveLength(0);
  });

  it('flattens expanded groups', () => {
    const groups = appendHits([], [hit({ id: 'a', cmd_seq: 1 }), hit({ id: 'b', cmd_seq: 1 }), hit({ id: 'c', cmd_seq: 2 })]);
    expect(flattenRows(groups, new Set()).map((r) => r.hit.id)).toEqual(['a', 'c']);
    const rows = flattenRows(groups, new Set([groups[0]!.key]));
    expect(rows.map((r) => [r.hit.id, r.head])).toEqual([
      ['a', true],
      ['b', false],
      ['c', true],
    ]);
  });

  it('computes the rendered window', () => {
    expect(visibleRange(0, 560, 56, 1000, 6)).toEqual([0, 17]);
    expect(visibleRange(5600, 560, 56, 1000, 6)).toEqual([94, 117]);
    expect(visibleRange(56_000, 560, 56, 1000, 6)).toEqual([994, 1000]);
    expect(visibleRange(0, 560, 56, 0)).toEqual([0, 0]);
  });

  it('scrolls a row into view only when needed', () => {
    expect(scrollToRow(0, 0, 560, 56)).toBeNull();
    expect(scrollToRow(20, 0, 560, 56)).toBe(21 * 56 - 560);
    expect(scrollToRow(2, 300, 560, 56)).toBe(112);
  });

  it('removes forgotten hits and promotes the next hit of a group', () => {
    const groups = appendHits([], [hit({ id: 'c:1', cmd_seq: 1 }), hit({ id: 'o:1', kind: 'output', cmd_seq: 1 }), hit({ id: 'c:2', cmd_seq: 2 })]);
    const headGone = removeHits(groups, (h) => h.id === 'c:1');
    expect(headGone.map((g) => [g.head.id, g.rest.length])).toEqual([
      ['o:1', 0],
      ['c:2', 0],
    ]);
    expect(removeHits(groups, (h) => h.cmd_seq === 1).map((g) => g.head.id)).toEqual(['c:2']);
    expect(groups[0]!.rest).toHaveLength(1);
  });

  it('follows every cursor page for the hit times', async () => {
    const pages: Record<string, HitPage> = {
      '': { hits: [{ t: 50 }, { t: 40 }], next: 'p2', partial: false, capped: false },
      p2: { hits: [{ t: 40 }, { t: 10 }], next: 'p3', partial: false, capped: false },
      p3: { hits: [{ t: 5 }], next: null, partial: false, capped: false },
    };
    const seen: (string | undefined)[] = [];
    const r = await collectHitTimes(async (c) => (seen.push(c), pages[c ?? '']!), 10);
    expect(seen).toEqual([undefined, 'p2', 'p3']);
    expect(r).toEqual({ times: [5, 10, 40, 50], truncated: false });
  });

  it('marks hit times truncated at the page cap or on a partial page', async () => {
    let n = 0;
    const endless = await collectHitTimes(async () => ({ hits: [{ t: (n += 1) }], next: 'more', partial: false, capped: false }), 3);
    expect(endless).toEqual({ times: [1, 2, 3], truncated: true });
    const partial = await collectHitTimes(async () => ({ hits: [{ t: 1 }], next: null, partial: true, capped: false }), 3);
    expect(partial.truncated).toBe(true);
  });
});
