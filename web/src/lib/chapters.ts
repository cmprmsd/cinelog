// Chapter (command) navigation and hit stepping inside one session.

import type { Command } from './api/types';

/** Seeking lands a little after a target; this keeps `]` from finding the same chapter. */
const EPS = 0.05;
/** `[` within this many seconds of a chapter start goes to the previous chapter. */
const RESTART_WINDOW = 1.5;

/** Index of the chapter playing at `t` (last start ≤ t), or -1 before the first. */
export function chapterAt(cmds: readonly Command[], t: number): number {
  let lo = 0;
  let hi = cmds.length - 1;
  let found = -1;
  while (lo <= hi) {
    const mid = (lo + hi) >> 1;
    if (cmds[mid]!.t_start <= t + EPS) {
      found = mid;
      lo = mid + 1;
    } else {
      hi = mid - 1;
    }
  }
  return found;
}

export function nextChapter(cmds: readonly Command[], t: number): Command | undefined {
  return cmds.find((c) => c.t_start > t + EPS);
}

/** Like a media player's "previous": restart the current chapter, or go one back. */
export function prevChapter(cmds: readonly Command[], t: number): Command | undefined {
  const i = chapterAt(cmds, t);
  if (i < 0) return undefined;
  const cur = cmds[i]!;
  if (t - cur.t_start > RESTART_WINDOW) return cur;
  return i > 0 ? cmds[i - 1] : cur;
}

export type ExitClass = 'ok' | 'fail' | 'unknown';

export function exitClass(exit: number | null): ExitClass {
  if (exit === null) return 'unknown';
  return exit === 0 ? 'ok' : 'fail';
}

/** The next (`dir = 1`) or previous hit time from `t` in a sorted list. */
export function stepHit(times: readonly number[], t: number, dir: 1 | -1): number | undefined {
  if (dir === 1) return times.find((x) => x > t + EPS + 0.45);
  for (let i = times.length - 1; i >= 0; i -= 1) if (times[i]! < t - EPS - 0.45) return times[i];
  return undefined;
}

/** Position of `t` on a 0…100 % rail. */
export function pct(t: number, duration: number): number {
  if (!(duration > 0)) return 0;
  return Math.min(100, Math.max(0, (t / duration) * 100));
}
