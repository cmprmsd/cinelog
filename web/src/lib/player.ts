// asciinema-player 3.18 setup for the two playback modes of API.md §6.

import * as AsciinemaPlayer from 'asciinema-player';
import type { Options, Player, Source, UrlSource } from 'asciinema-player';
import type { PlayInfo } from './api/types';

export type { Player };

export interface PlayerOptions {
  theme: string;
  startAt: number;
  keystrokeOverlay: boolean;
}

/** The source object: segmented casts must not carry parser options (the player rejects them). */
export function playerSource(info: Pick<PlayInfo, 'mode' | 'url'>): Source {
  const src: UrlSource & { format?: 'segmented' } = { url: info.url };
  if (info.mode === 'segmented') src.format = 'segmented';
  return src;
}

/**
 * Player options. `idleTimeLimit` and `markers` stay unset: segmented mode rejects them, and
 * an idle limit would shift the timeline away from the server's hit times.
 */
export function playerOptions(info: Pick<PlayInfo, 'cols' | 'rows'>, opts: PlayerOptions): Options {
  return {
    cols: info.cols > 0 ? info.cols : undefined,
    rows: info.rows > 0 ? info.rows : undefined,
    theme: opts.theme,
    preload: true,
    startAt: Math.max(0, opts.startAt),
    fit: 'width',
    controls: true,
    keystrokeOverlay: opts.keystrokeOverlay,
  };
}

export function createPlayer(info: PlayInfo, el: HTMLElement, opts: PlayerOptions): Player {
  return AsciinemaPlayer.create(playerSource(info), el, playerOptions(info, opts));
}

/** The core's `error` event (load or driver failure), which the typings leave out. */
export function onPlayerError(player: Player, handler: (message: string) => void): void {
  const add = player.addEventListener as unknown as (name: string, fn: (e: { message?: unknown }) => void) => void;
  add.call(player, 'error', (e) => handler(typeof e?.message === 'string' ? e.message : 'playback failed'));
}
