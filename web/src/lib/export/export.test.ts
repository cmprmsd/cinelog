import { describe, expect, it } from 'vitest';
import type { Command } from '../api/types';
import { entryLabel, exportChoices, exportParams, fallbackName, moveFocus, usableSelection } from './export';

const UUID = '0b9e4c1a-7d2e-4f00-9a51-0000000000a1';

function cmd(over: Partial<Command> = {}): Command {
  return {
    seq: 3,
    parent_seq: null,
    depth: 0,
    t_start: 8,
    t_end: 9.5,
    ts_start: 1008,
    ts_end: 1009.5,
    exit: 0,
    pipestatus: [0],
    cwd: '/srv',
    user: 'alice',
    typed: 'cat evil.html',
    expanded: null,
    typed_trunc: false,
    approx: false,
    confidence: null,
    out_bytes: 10,
    out_truncated: false,
    ...over,
  };
}

describe('usableSelection', () => {
  it('orders, clamps at 0 and rounds to milliseconds', () => {
    expect(usableSelection({ from: 12.34567, to: 3.0001 })).toEqual({ from: 3, to: 12.346 });
    expect(usableSelection({ from: -2, to: 1 })).toEqual({ from: 0, to: 1 });
  });

  it('rejects empty, sub-millisecond and non-finite selections', () => {
    expect(usableSelection(null)).toBeNull();
    expect(usableSelection({ from: 5, to: 5 })).toBeNull();
    expect(usableSelection({ from: 5, to: 5.0002 })).toBeNull();
    expect(usableSelection({ from: 0, to: Number.NaN })).toBeNull();
    expect(usableSelection({ from: 0, to: Infinity })).toBeNull();
  });
});

describe('exportChoices', () => {
  it('offers the command, the selection and the whole session', () => {
    const rows = exportChoices(cmd(), { from: 2, to: 70 }, 120);
    expect(rows.map((r) => r.source)).toEqual(['command', 'selection', 'session']);
    expect(rows[0]).toMatchObject({ query: { cmd: 3 }, range: { from: 8, to: 9.5 }, detail: '$ cat evil.html (0:08–0:09)' });
    expect(rows[1]).toMatchObject({ query: { from: 2, to: 70 }, detail: '0:02–1:10' });
    expect(rows[2]).toMatchObject({ query: {}, range: { from: 0, to: 120 }, detail: '0:00–2:00' });
  });

  it('keeps the command row, disabled, when no command is at the playhead', () => {
    const rows = exportChoices(null, null, null);
    expect(rows.map((r) => r.source)).toEqual(['command', 'session']);
    expect(rows[0]!.query).toBeNull();
    expect(rows[1]).toMatchObject({ query: {}, range: null, detail: 'the full recording' });
  });

  it('marks a running command and an empty selection', () => {
    const rows = exportChoices(cmd({ t_end: null }), { from: 4, to: 4 }, null);
    expect(rows[0]).toMatchObject({ query: { cmd: 3 }, range: null });
    expect(rows[0]!.detail).toMatch(/\(running\)$/);
    expect(rows[1]).toMatchObject({ source: 'selection', query: null, detail: 'empty selection' });
  });

  it('shows the command on one line and shortens it', () => {
    const rows = exportChoices(cmd({ typed: `for i in 1 2; do\n  echo ${'x'.repeat(80)}\ndone` }), null, null);
    expect(rows[0]!.detail).not.toContain('\n');
    expect(rows[0]!.detail).toContain('…');
    expect(rows[0]!.detail.length).toBeLessThan(80);
  });
});

describe('exportParams', () => {
  it('adds the format to the source query', () => {
    const [c, s, w] = exportChoices(cmd(), { from: 1, to: 2 }, 10);
    expect(exportParams(c!, 'cast')).toEqual({ format: 'cast', cmd: 3 });
    expect(exportParams(s!, 'txt')).toEqual({ format: 'txt', from: 1, to: 2 });
    expect(exportParams(w!, 'html')).toEqual({ format: 'html' });
  });

  it('is null for an unavailable source', () => {
    const [c] = exportChoices(null, null, null);
    expect(exportParams(c!, 'txt')).toBeNull();
  });
});

describe('names and labels', () => {
  it('builds the server-style fallback name', () => {
    expect(fallbackName(UUID, 'cast', { from: 8, to: 9.5 })).toBe('cinelog-0b9e4c1a-8-10.cast');
    expect(fallbackName(UUID, 'html', null)).toBe('cinelog-0b9e4c1a.html');
  });

  it('labels every entry by source and format', () => {
    const [c] = exportChoices(cmd(), null, null);
    expect(entryLabel(c!, 'txt')).toBe('Current command as .txt');
  });
});

describe('moveFocus', () => {
  it('moves through a grid of three columns', () => {
    expect(moveFocus('ArrowRight', 0, 9, 3)).toBe(1);
    expect(moveFocus('ArrowRight', 8, 9, 3)).toBe(0);
    expect(moveFocus('ArrowLeft', 0, 9, 3)).toBe(8);
    expect(moveFocus('ArrowDown', 1, 9, 3)).toBe(4);
    expect(moveFocus('ArrowDown', 7, 9, 3)).toBe(8);
    expect(moveFocus('ArrowUp', 1, 9, 3)).toBe(0);
    expect(moveFocus('Home', 5, 9, 3)).toBe(0);
    expect(moveFocus('End', 0, 9, 3)).toBe(8);
  });

  it('ignores other keys and empty menus', () => {
    expect(moveFocus('Enter', 0, 9, 3)).toBeNull();
    expect(moveFocus('ArrowDown', 0, 0, 3)).toBeNull();
  });
});
