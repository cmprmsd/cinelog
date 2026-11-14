// How a tmux location is named everywhere in the UI: `<session> › <window> <window name> ·
// <pane index>`. The timeline's row labels come from the server in the same form
// (`slot` in crates/cinelog-web/src/api/timeline.rs).

import type { Session, TmuxAttach, TmuxLocation } from './api/types';

type Pane = Pick<TmuxLocation, 'window' | 'window_name' | 'pane' | 'pane_index'>;

/** A window: its index and name (`2 build`); `?` when unknown. */
export function windowLabel(window: string | null, name: string | null): string {
  return [window ?? '?', name].filter(Boolean).join(' ');
}

/** A pane as tmux shows it: its index in the window, else its id (`%7`). */
export function paneIndexLabel(index: string | null | undefined, pane: string | null): string {
  return index || pane || '?';
}

/** `<window> <window name> · <pane>`; the pane alone when the window is unknown. */
export function paneLabel(t: Pane): string {
  const pane = paneIndexLabel(t.pane_index, t.pane);
  return t.window ? `${windowLabel(t.window, t.window_name)} · ${pane}` : pane;
}

/** Name of a tmux session: its name, else its id. */
export function tmuxSessionLabel(t: Pick<TmuxLocation, 'session' | 'session_id'> | TmuxAttach): string {
  return t.session ?? t.session_id ?? 'tmux';
}

/** Where a session runs: its tmux pane with its session, else its holder or terminal. */
export function whereLabel(s: Pick<Session, 'tmux' | 'holder_comm' | 'tty'>): string {
  if (s.tmux.pane) return `${tmuxSessionLabel(s.tmux)} › ${paneLabel(s.tmux)}`;
  return shellLabel(s);
}

/** A shell outside tmux: `<holder> <tty>`. */
export function shellLabel(s: Pick<Session, 'holder_comm' | 'tty'>): string {
  return [s.holder_comm, s.tty?.replace(/^\/dev\//, '')].filter(Boolean).join(' ');
}
