import { describe, expect, it } from 'vitest';
import { playerOptions, playerSource } from './player';

describe('player setup', () => {
  it('marks segmented sources and nothing else', () => {
    expect(playerSource({ mode: 'cast', url: '/api/sessions/u/cast' })).toEqual({ url: '/api/sessions/u/cast' });
    expect(playerSource({ mode: 'segmented', url: '/api/sessions/u/play/index.json' })).toEqual({
      url: '/api/sessions/u/play/index.json',
      format: 'segmented',
    });
  });

  it('never sets options the segmented driver rejects', () => {
    const o = playerOptions({ cols: 80, rows: 24 }, { theme: 'auto/dracula', startAt: -2, keystrokeOverlay: true });
    expect(o).not.toHaveProperty('idleTimeLimit');
    expect(o).not.toHaveProperty('markers');
    expect(o).toMatchObject({ cols: 80, rows: 24, startAt: 0, theme: 'auto/dracula', keystrokeOverlay: true, preload: true });
    expect(playerOptions({ cols: 0, rows: 0 }, { theme: 't', startAt: 3, keystrokeOverlay: false })).toMatchObject({
      cols: undefined,
      rows: undefined,
    });
  });
});
