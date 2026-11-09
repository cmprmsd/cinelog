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
    expect(nested).toEqual([{ t: 6, child: 'abc', label: 'nested: root (sudo by alice), outer paused', external: null }]);
  });

  it('ignores malformed marker data', () => {
    const { bands, nested } = markerBands([m(1, 'gap', { from_t: 'x' }), m(2, 'nested', { child_uuid: 5 })], [], 10);
    expect(bands[0]).toMatchObject({ from: 1, to: 1 });
    expect(nested[0]).toMatchObject({ child: null, label: 'nested session' });
  });

  it('links a child of the other scope to the server that recorded it', () => {
    const team = { child_uuid: 'abc', scope: 'team', user: 'root', mode: 'pause' };
    const at = (data: Record<string, unknown>, scope: 'team' | 'personal' | null = 'personal') =>
      markerBands([m(6, 'nested', data)], [], 10, scope, 'box.lan').nested[0]!;
    // The URL the child's recorder reported, when its server ran.
    expect(at({ ...team, url: 'https://ops.lan:10000/s/abc' })).toMatchObject({ child: 'abc', external: 'https://ops.lan:10000/s/abc' });
    // Without one (or a bogus one): the TEAM default port on this host.
    expect(at(team).external).toBe('https://box.lan:10000/s/abc');
    expect(at({ ...team, url: 'javascript:alert(1)' }).external).toBe('https://box.lan:10000/s/abc');
    expect(at({ ...team, url: 'https://ops.lan:10000/s/other' }).external).toBe('https://box.lan:10000/s/abc');
    // Same scope: an in-app link.
    expect(at(team, 'team')).toMatchObject({ child: 'abc', external: null });
    // A PERSONAL child seen from the TEAM server without a URL: its port is unknown, no link.
    expect(at({ child_uuid: 'abc', scope: 'personal', user: 'bob' }, 'team')).toMatchObject({ child: null, external: null });
    // Unknown session scope (still loading): as before.
    expect(at(team, null)).toMatchObject({ child: 'abc', external: null });
  });
});
