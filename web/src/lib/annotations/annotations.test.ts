import { describe, expect, it } from 'vitest';
import type { Annotation, Command, Tag } from '../api/types';
import {
  applyEvent,
  bodyBytes,
  covers,
  dropDeleted,
  firstLine,
  linkify,
  normalizeTag,
  rangeOfAnnotation,
  sortTags,
  spansOf,
  suggestTags,
  tagColor,
  targetLabel,
  type Tombstones,
  upsert,
  upsertTag,
  withoutDeleted,
} from './annotations';

const S = '0b9e4c1a-7d2e-4f00-9a51-0000000000a1';

function note(over: Partial<Annotation>): Annotation {
  return {
    uuid: '01900000-0000-7000-8000-000000000001',
    session: S,
    cmd_seq: 1,
    t_from: null,
    t_to: null,
    author: 'alice',
    body: 'hello',
    tags: [],
    created_at: 100,
    updated_at: 100,
    deleted_at: null,
    ...over,
  };
}

function cmd(seq: number, t_start: number, t_end: number | null, typed = `cmd ${seq}`): Command {
  return {
    seq,
    parent_seq: null,
    depth: 0,
    t_start,
    t_end,
    ts_start: 1000 + t_start,
    ts_end: t_end === null ? null : 1000 + t_end,
    exit: 0,
    typed,
    cwd: '/',
    approx: false,
    confidence: null,
  } as unknown as Command;
}

describe('tags', () => {
  it('normalises like the server', () => {
    expect(normalizeTag('  Incident-42 ')).toBe('incident-42');
    expect(normalizeTag('ops/db:prod+1#@x_y.z')).toBe('ops/db:prod+1#@x_y.z');
    expect(normalizeTag('Ünïcode')).toBe('ünïcode');
    for (const bad of ['', '  ', 'a b', 'semi;colon', 'x'.repeat(41), '<script>']) expect(normalizeTag(bad)).toBeNull();
    expect(normalizeTag('x'.repeat(40))).toBe('x'.repeat(40));
    expect(sortTags(['b', 'a', 'b'])).toEqual(['a', 'b']);
  });

  it('derives stable colours unless one is set', () => {
    expect(tagColor('prod', '#ff8800')).toBe('#ff8800');
    expect(tagColor('prod', null)).toBe(tagColor('prod', undefined));
    expect(tagColor('prod', null)).toMatch(/^hsl\(\d+ 60% 48%\)$/);
    expect(tagColor('prod', null)).not.toBe(tagColor('dev', null));
  });

  it('suggests prefix matches first, then by use', () => {
    const all: Tag[] = [
      { name: 'deploy', color: null, count: 1 },
      { name: 'prod', color: null, count: 9 },
      { name: 'preprod', color: null, count: 5 },
      { name: 'incident', color: null, count: 2 },
    ];
    expect(suggestTags(all, 'pro', []).map((t) => t.name)).toEqual(['prod', 'preprod']);
    expect(suggestTags(all, 'p', ['prod']).map((t) => t.name)).toEqual(['preprod', 'deploy']);
    expect(suggestTags(all, '', [], 2).map((t) => t.name)).toEqual(['prod', 'preprod']);
    expect(suggestTags(all, 'zzz', [])).toEqual([]);
    expect(upsertTag(all, { name: 'deploy', color: '#000000', count: 1 })[0]).toEqual({
      name: 'deploy',
      color: '#000000',
      count: 1,
    });
  });
});

describe('bodies', () => {
  it('turns http(s) URLs into links and nothing else', () => {
    expect(linkify('see https://example.com/a?b=1, then (http://x.org/y) and <b>bold</b>')).toEqual([
      { text: 'see ', href: null },
      { text: 'https://example.com/a?b=1', href: 'https://example.com/a?b=1' },
      { text: ', then (', href: null },
      { text: 'http://x.org/y', href: 'http://x.org/y' },
      { text: ') and <b>bold</b>', href: null },
    ]);
    expect(linkify('javascript:alert(1) data:text/html,x ftp://a')).toEqual([
      { text: 'javascript:alert(1) data:text/html,x ftp://a', href: null },
    ]);
    expect(linkify('https://en.wikipedia.org/wiki/Foo_(bar).')).toEqual([
      {
        text: 'https://en.wikipedia.org/wiki/Foo_(bar)',
        href: 'https://en.wikipedia.org/wiki/Foo_(bar)',
      },
      { text: '.', href: null },
    ]);
    expect(linkify('')).toEqual([]);
    expect(linkify('a\nb')).toEqual([{ text: 'a\nb', href: null }]);
  });

  it('measures bytes and first lines', () => {
    expect(bodyBytes('ü  \n')).toBe(2);
    expect(firstLine('one\ntwo')).toBe('one');
    expect(firstLine('x'.repeat(100), 10)).toBe(`${'x'.repeat(9)}…`);
    expect(firstLine('a\tb\x1b')).toBe('a b␛');
  });
});

describe('lists', () => {
  const a = note({ uuid: 'a', created_at: 1, updated_at: 1 });
  const b = note({ uuid: 'b', created_at: 2, updated_at: 2 });

  it('upserts in order and ignores older versions', () => {
    expect(upsert([b], a).map((x) => x.uuid)).toEqual(['a', 'b']);
    const edited = { ...a, body: 'edited', updated_at: 5 };
    expect(upsert([a, b], edited)[0]!.body).toBe('edited');
    expect(upsert([edited, b], a)[0]!.body).toBe('edited');
    expect(upsert([a, b], { ...a, deleted_at: 6, updated_at: 6 }).map((x) => x.uuid)).toEqual(['b']);
    expect(upsert([b], { ...a, deleted_at: 6, updated_at: 6 }).map((x) => x.uuid)).toEqual(['b']);
  });

  it('applies only events of its session', () => {
    expect(applyEvent([a], { op: 'created', annotation: { ...b, session: 'other' } }, S)).toEqual([a]);
    expect(applyEvent([a], { op: 'created', annotation: b }, S)).toEqual([a, b]);
    expect(applyEvent([a, b], { op: 'deleted', annotation: { ...b, deleted_at: 9, updated_at: 9 } }, S)).toEqual([a]);
  });

  it('never brings a deleted annotation back', () => {
    // The bus delivers a racing add_tags after the delete, and a poll that started before it.
    const gone: Tombstones = new Set();
    let list = applyEvent([a, b], { op: 'deleted', annotation: { ...b, deleted_at: 9, updated_at: 9 } }, S, gone);
    expect(gone.has('b')).toBe(true);
    list = applyEvent(list, { op: 'updated', annotation: { ...b, tags: ['late'], updated_at: 8 } }, S, gone);
    expect(list).toEqual([a]);
    list = upsert(list, { ...b, updated_at: 10 }, gone);
    expect(list).toEqual([a]);
    expect(withoutDeleted([a, b], gone)).toEqual([a]);

    // Deleted here (the DELETE answers without a body): a stale load result stays filtered.
    const mine: Tombstones = new Set();
    list = dropDeleted([a, b], 'a', mine);
    expect(list).toEqual([b]);
    expect(withoutDeleted([a, b], mine)).toEqual([b]);
    expect(upsert(list, a, mine)).toEqual([b]);
  });
});

describe('targets and spans', () => {
  const commands = [cmd(1, 1, 2, 'echo hi'), cmd(2, 4, null, 'sleep 100')];

  it('maps commands and spans to session seconds', () => {
    expect(rangeOfAnnotation(note({ cmd_seq: 1 }), commands)).toEqual({
      from: 1,
      to: 2,
    });
    expect(rangeOfAnnotation(note({ cmd_seq: 2 }), commands)).toEqual({
      from: 4,
      to: 4,
    });
    expect(rangeOfAnnotation(note({ cmd_seq: 9 }), commands)).toBeNull();
    expect(rangeOfAnnotation(note({ cmd_seq: null, t_from: 3, t_to: 7 }), commands)).toEqual({ from: 3, to: 7 });
    expect(targetLabel(note({ cmd_seq: 1 }), commands)).toBe('$ echo hi');
    expect(targetLabel(note({ cmd_seq: 9 }), commands)).toBe('$ command 9');
    expect(targetLabel(note({ cmd_seq: null, t_from: 3, t_to: 75 }), commands)).toBe('0:03–1:15');
  });

  it('knows what the playhead is on', () => {
    expect(covers(note({ cmd_seq: 1 }), commands, 0, 1)).toBe(true);
    expect(covers(note({ cmd_seq: 1 }), commands, 1.5, 2)).toBe(false);
    const span = note({ cmd_seq: null, t_from: 3, t_to: 7 });
    expect(covers(span, commands, 5, null)).toBe(true);
    expect(covers(span, commands, 8, null)).toBe(false);
  });

  it('builds rail spans with tag colours and labels', () => {
    const colors = new Map<string, string | null>([['prod', '#ff0000']]);
    const spans = spansOf(
      [
        note({
          uuid: 'x',
          cmd_seq: 1,
          body: 'line one\nline two',
          tags: ['prod', 'zz'],
        }),
        note({
          uuid: 'y',
          cmd_seq: null,
          t_from: 3,
          t_to: 7,
          tags: ['zz'],
          body: '',
        }),
        note({ uuid: 'gone', cmd_seq: 9 }),
        note({ uuid: 'del', deleted_at: 3 }),
        note({ uuid: 'plain', cmd_seq: 2, body: 'p' }),
      ],
      commands,
      colors,
    );
    expect(spans).toEqual([
      {
        uuid: 'x',
        from: 1,
        to: 2,
        color: '#ff0000',
        label: 'line one · #prod #zz (alice)',
      },
      {
        uuid: 'y',
        from: 3,
        to: 7,
        color: tagColor('zz', null),
        label: '#zz (alice)',
      },
      {
        uuid: 'plain',
        from: 4,
        to: 4,
        color: 'var(--accent)',
        label: 'p (alice)',
      },
    ]);
  });
});
