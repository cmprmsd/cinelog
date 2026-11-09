import { describe, expect, it } from 'vitest';
import { onTermSize, playerOptions, playerSource, type Player } from './player';

describe('player setup', () => {
  it('marks segmented sources and nothing else', () => {
    expect(playerSource({ mode: 'cast', url: '/api/sessions/u/cast' })).toEqual({ url: '/api/sessions/u/cast' });
    expect(playerSource({ mode: 'segmented', url: '/api/sessions/u/play/index.json' })).toEqual({
      url: '/api/sessions/u/play/index.json',
      format: 'segmented',
    });
  });

  it('never sets options the segmented driver rejects', () => {
    const o = playerOptions({ theme: 'auto/dracula', startAt: -2, keystrokeOverlay: true });
    expect(o).not.toHaveProperty('idleTimeLimit');
    expect(o).not.toHaveProperty('markers');
    expect(o).toMatchObject({ startAt: 0, theme: 'auto/dracula', keystrokeOverlay: true, preload: true, fit: 'width' });
  });

  it('leaves the terminal size to the recording', () => {
    // A size option would pin the terminal and ignore the recording's resize events.
    const o = playerOptions({ theme: 't', startAt: 3, keystrokeOverlay: false });
    expect(o).not.toHaveProperty('cols');
    expect(o).not.toHaveProperty('rows');
  });

  it('reports the terminal size from reset and resize events', () => {
    const handlers = new Map<string, (e: unknown) => void>();
    const player = { addEventListener: (name: string, fn: (e: unknown) => void) => void handlers.set(name, fn) } as unknown as Player;
    const seen: string[] = [];
    onTermSize(player, (c, r) => seen.push(`${c}x${r}`));
    handlers.get('reset')!({ size: { cols: 80, rows: 24 }, theme: null });
    handlers.get('resize')!({ cols: 121, rows: 70 });
    handlers.get('resize')!({ cols: 0, rows: 70 });
    handlers.get('reset')!({});
    expect(seen).toEqual(['80x24', '121x70']);
  });
});
