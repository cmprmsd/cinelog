import { describe, expect, it } from 'vitest';
import type { Annotation } from '../api/types';
import {
  appendPage,
  belongsLoaded,
  clearOverview,
  compareDesc,
  insertDesc,
  isFiltered,
  markText,
  matches,
  OVERVIEW_EMPTY,
  overviewChips,
  overviewFromQuery,
  overviewToQuery,
  preview,
  toAnnotationParams,
} from './overview';

const q = (s: string) => new URLSearchParams(s);

function note(n: number, over: Partial<Annotation> = {}): Annotation {
  return {
    uuid: `01900000-0000-7000-8000-${String(n).padStart(12, '0')}`,
    session: '0b9e4c1a-7d2e-4f00-9a51-0000000000a1',
    cmd_seq: 1,
    t_from: null,
    t_to: null,
    author: 'alice',
    body: 'Deploy went fine',
    tags: ['deploy', 'ops'],
    created_at: 1000 + n,
    updated_at: 1000 + n,
    deleted_at: null,
    ...over,
  };
}

describe('annotations overview', () => {
  it('round-trips the filters through the URL and leaves defaults out', () => {
    const s = overviewFromQuery(q('tag=Ops,deploy&author=%20bob%20&q=disk%20full&date=custom&from=10&to=20'));
    expect(s).toEqual({ tags: ['deploy', 'ops'], author: 'bob', q: 'disk full', date: 'custom', from: 10, to: 20 });
    expect(overviewFromQuery(overviewToQuery(s))).toEqual(s);
    expect(overviewToQuery(OVERVIEW_EMPTY).toString()).toBe('');
    expect(overviewFromQuery(q('date=7d&from=10'))).toMatchObject({ date: '7d', from: null, to: null });
    expect(overviewFromQuery(q('date=custom')).date).toBe('any');
    expect(overviewFromQuery(q('date=bogus&tag=bad%20tag')).date).toBe('any');
    expect(overviewFromQuery(q(`q=${'x'.repeat(300)}`)).q).toHaveLength(200);
    expect(isFiltered(OVERVIEW_EMPTY)).toBe(false);
    expect(isFiltered({ ...OVERVIEW_EMPTY, q: '  ' })).toBe(false);
    expect(isFiltered({ ...OVERVIEW_EMPTY, author: 'bob' })).toBe(true);
  });

  it('builds the request: newest first with context, filters as the API takes them', () => {
    const now = new Date(2026, 10, 8, 12);
    const secs = Math.floor(now.getTime() / 1000);
    expect(toAnnotationParams(OVERVIEW_EMPTY, now)).toEqual({ order: 'desc', context: true, limit: 50 });
    expect(toAnnotationParams({ tags: ['a', 'b'], author: 'bob', q: ' disk ', date: '7d', from: null, to: null }, now, { cursor: 'c' })).toEqual({
      order: 'desc',
      context: true,
      limit: 50,
      from: secs - 7 * 86400,
      tag: 'a,b',
      author: 'bob',
      q: 'disk',
      cursor: 'c',
    });
    // A custom range typed backwards is put in order (the API refuses from > to).
    expect(toAnnotationParams({ ...OVERVIEW_EMPTY, date: 'custom', from: 20, to: 10 }, now)).toMatchObject({ from: 10, to: 20 });
  });

  it('matches bus events locally like the server', () => {
    const now = new Date(2000 * 1000);
    const a = note(1);
    expect(matches(a, OVERVIEW_EMPTY, now)).toBe(true);
    expect(matches(a, { ...OVERVIEW_EMPTY, tags: ['ops'] }, now)).toBe(true);
    expect(matches(a, { ...OVERVIEW_EMPTY, tags: ['ops', 'db'] }, now)).toBe(false);
    expect(matches(a, { ...OVERVIEW_EMPTY, author: 'bob' }, now)).toBe(false);
    expect(matches(a, { ...OVERVIEW_EMPTY, q: '  WENT ' }, now)).toBe(true);
    expect(matches(a, { ...OVERVIEW_EMPTY, q: 'tags' }, now)).toBe(false);
    expect(matches(a, { ...OVERVIEW_EMPTY, date: 'custom', from: 1001, to: 1001 }, now)).toBe(true);
    expect(matches(a, { ...OVERVIEW_EMPTY, date: 'custom', from: 1002, to: null }, now)).toBe(false);
    expect(matches(note(1, { deleted_at: 5 }), OVERVIEW_EMPTY, now)).toBe(false);
  });

  it('keeps the list newest first, uuid descending on ties', () => {
    const [a, b, c] = [note(1), note(2), note(3, { created_at: 1002 })];
    expect([a, b, c].sort(compareDesc).map((x) => x.uuid)).toEqual([c.uuid, b.uuid, a.uuid]);
    expect(appendPage([c, b], [b, a]).map((x) => x.uuid)).toEqual([c.uuid, b.uuid, a.uuid]);
    expect(insertDesc([c, a], b).map((x) => x.uuid)).toEqual([c.uuid, b.uuid, a.uuid]);
    const edited = { ...b, body: 'edited', updated_at: 5000 };
    expect(insertDesc([c, b, a], edited)[1]!.body).toBe('edited');
    // An older copy does not overwrite a newer one.
    expect(insertDesc([c, edited, a], b)[1]!.body).toBe('edited');
  });

  it('inserts announced rows only within the loaded part', () => {
    const list = [note(9), note(5)];
    expect(belongsLoaded(list, note(7), true)).toBe(true);
    expect(belongsLoaded(list, note(3), true)).toBe(false);
    expect(belongsLoaded(list, note(3), false)).toBe(true);
    expect(belongsLoaded([], note(3), true)).toBe(true);
  });

  it('previews the first lines and marks the text filter', () => {
    expect(preview('one\ntwo\r\nthree\nfour')).toEqual({ text: 'one\ntwo\nthree', more: true });
    expect(preview('one\ntwo\n\n\n')).toEqual({ text: 'one\ntwo', more: false });
    expect(preview('x'.repeat(500)).text).toHaveLength(400);
    expect(markText('Disk full, DISK!', 'disk')).toEqual([
      { text: 'Disk', hit: true },
      { text: ' full, ', hit: false },
      { text: 'DISK', hit: true },
      { text: '!', hit: false },
    ]);
    expect(markText('plain', '')).toEqual([{ text: 'plain', hit: false }]);
    // Lowercasing changes the length here: nothing is marked rather than the wrong part.
    expect(markText('İstanbul disk', 'disk')).toEqual([{ text: 'İstanbul disk', hit: false }]);
  });

  it('lists and clears filter chips', () => {
    const s = { tags: ['a', 'b'], author: 'bob', q: 'disk', date: '30d' as const, from: null, to: null };
    expect(overviewChips(s, String).map((c) => c.label)).toEqual(['#a', '#b', 'by bob', '“disk”', 'last 30 days']);
    expect(clearOverview(s, 'tag', 'a').tags).toEqual(['b']);
    expect(clearOverview(s, 'author').author).toBe('');
    expect(clearOverview(s, 'date')).toMatchObject({ date: 'any', from: null, to: null });
    expect(overviewChips({ ...OVERVIEW_EMPTY, date: 'custom', from: 1, to: null }, (t) => `t${t}`)[0]!.label).toBe('t1 – …');
  });
});
