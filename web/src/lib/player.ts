// asciinema-player 3.18 setup for the two playback modes of API.md §6.

import * as AsciinemaPlayer from 'asciinema-player';
import type { Options, Player, Source, UrlSource } from 'asciinema-player';
import type { PlayInfo } from './api/types';
import { BASE_FONT_PX, DEFAULT_CELL, type CellMetrics } from './fit';

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
 * an idle limit would shift the timeline away from the server's hit times. `cols` and `rows`
 * stay unset too: they pin the terminal to the header's size, so a terminal that was resized
 * while recording would play back garbled; the cast header and its resize events give the size.
 * The player always fits the width of its mount; `FitBox` sizes the mount.
 */
export function playerOptions(opts: PlayerOptions): Options {
  return {
    theme: opts.theme,
    preload: true,
    startAt: Math.max(0, opts.startAt),
    fit: 'width',
    controls: true,
    keystrokeOverlay: opts.keystrokeOverlay,
  };
}

export function createPlayer(info: PlayInfo, el: HTMLElement, opts: PlayerOptions): Player {
  return AsciinemaPlayer.create(playerSource(info), el, playerOptions(opts));
}

/**
 * Calls `handler` with the terminal's size whenever the player (re)sets or resizes it: the
 * cast header, a seek, a segment snapshot, `LiveInit`, and the recording's resize events.
 * Both events are left out of the typings.
 */
export function onTermSize(player: Player, handler: (cols: number, rows: number) => void): void {
  const add = player.addEventListener as unknown as (name: string, fn: (e: unknown) => void) => void;
  const report = (size: unknown) => {
    const s = size as { cols?: unknown; rows?: unknown } | null;
    if (typeof s?.cols === 'number' && typeof s.rows === 'number' && s.cols > 0 && s.rows > 0) handler(s.cols, s.rows);
  };
  add.call(player, 'reset', (e) => report((e as { size?: unknown } | null)?.size));
  add.call(player, 'resize', report);
}

let cell: CellMetrics | null = null;

/**
 * The player's cell at its base font, measured the way asciinema-player measures it when it
 * mounts (an 80×24 `.ap-term` in the player's font). Needs the player's stylesheet; cached.
 */
export function cellMetrics(): CellMetrics {
  if (cell) return cell;
  const doc = globalThis.document;
  if (!doc?.body) return DEFAULT_CELL;
  const outer = doc.createElement('div');
  outer.className = 'ap-default-term-ff';
  outer.style.cssText = `height:0;overflow:hidden;font-size:${BASE_FONT_PX}px`;
  const term = doc.createElement('div');
  term.className = 'ap-term';
  term.style.cssText = 'width:80ch;height:32em;font-size:100%';
  outer.appendChild(term);
  doc.body.appendChild(outer);
  const measured = {
    charW: term.clientWidth / 80,
    charH: term.clientHeight / 24,
    bordersW: term.offsetWidth - term.clientWidth,
    bordersH: term.offsetHeight - term.clientHeight,
  };
  outer.remove();
  // Not laid out (no stylesheet yet, or a test DOM): keep the estimate and measure again later.
  if (!(measured.charW > 0 && measured.charH > 0)) return DEFAULT_CELL;
  cell = measured;
  return cell;
}

/** The core's `error` event (load or driver failure), which the typings leave out. */
export function onPlayerError(player: Player, handler: (message: string) => void): void {
  const add = player.addEventListener as unknown as (name: string, fn: (e: { message?: unknown }) => void) => void;
  add.call(player, 'error', (e) => handler(typeof e?.message === 'string' ? e.message : 'playback failed'));
}
