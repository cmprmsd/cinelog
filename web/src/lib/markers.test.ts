import { describe, expect, it } from 'vitest';
import type { Marker } from './api/types';
import { markerBands } from './markers';

const m = (t: number, kind: string, data: Record<string, unknown> = {}): Marker => ({ t, ts: 1e9 + t, kind, data });

describe('markerBands', () => {
  it('pairs pause and resume, and closes an open pause at the end', () => {
    const { bands } = markerBands(
      [m(2, 'pause', { reason: 'multiplexer', detail: 'tmux: client' }), m(5, 'resume'), m(8, 'pause', { reason: 'user' })],
      [],
      10,
    );
    expect(bands).toEqual([
      { kind: 'pause', from: 2, to: 5, label: 'paused (multiplexer: tmux: client)' },
      { kind: 'pause', from: 8, to: 10, label: 'paused (user)' },
    ]);
  });

  it('shows gaps, nested children and forgotten ranges', () => {
    const { bands, nested } = markerBands(
      [m(3, 'gap', { from_t: 3, to_t: 3.5, dropped_bytes: 4096 }), m(6, 'nested', { child_uuid: 'abc', user: 'root', sudo_user: 'alice', mode: 'pause' }), m(7, 'ctx')],
      [
        { t_from: 4, t_to: 5, by: 'alice' },
        { t_from: 0.5, t_to: 1, by: 'bob' },
        { t_from: 8, t_to: null, by: 'carol' },
      ],
      10,
    );
    expect(bands.map((b) => [b.kind, b.from, b.to])).toEqual([
      ['forgotten', 0.5, 1],
      ['gap', 3, 3.5],
      ['forgotten', 4, 5],
      ['forgotten', 8, 10],
    ]);
    expect(bands[1]!.label).toBe('gap (4096 bytes dropped)');
    expect(bands[2]!.label).toBe('forgotten by alice');
    expect(nested).toEqual([{ t: 6, child: 'abc', label: 'nested: root (sudo by alice), outer paused' }]);
  });

  it('ignores malformed marker data', () => {
    const { bands, nested } = markerBands([m(1, 'gap', { from_t: 'x' }), m(2, 'nested', { child_uuid: 5 })], [], 10);
    expect(bands[0]).toMatchObject({ from: 1, to: 1 });
    expect(nested[0]).toMatchObject({ child: null, label: 'nested session' });
  });
});
