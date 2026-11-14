import { describe, expect, it } from 'vitest';
import type { TmuxLocation } from './api/types';
import { paneLabel, shellLabel, tmuxSessionLabel, whereLabel, windowLabel } from './tmux';

const none: TmuxLocation = { server: null, session_id: null, session: null, window: null, window_name: null, pane: null, pane_index: null };

describe('tmux labels', () => {
  it('names a pane by window and pane index', () => {
    const t = { ...none, server: '1:2', session_id: '$3', session: 'ops', window: '2', window_name: 'build', pane: '%7', pane_index: '1' };
    expect(paneLabel(t)).toBe('2 build · 1');
    expect(whereLabel({ tmux: t, holder_comm: 'tmux: server', tty: '/dev/pts/4' })).toBe('ops › 2 build · 1');
    expect(windowLabel('2', null)).toBe('2');
  });

  it('falls back to the ids a recording always has', () => {
    const t = { ...none, session_id: '$3', pane: '%7' };
    expect(paneLabel(t)).toBe('%7');
    expect(paneLabel({ ...t, window: '0' })).toBe('0 · %7');
    expect(tmuxSessionLabel(t)).toBe('$3');
    expect(tmuxSessionLabel({ server: '1:2', session_id: '$3', session: 'ops' })).toBe('ops');
  });

  it('names a shell outside tmux by holder and terminal', () => {
    expect(whereLabel({ tmux: none, holder_comm: 'sshd-session', tty: '/dev/pts/7' })).toBe('sshd-session pts/7');
    expect(shellLabel({ holder_comm: null, tty: null })).toBe('');
  });
});
