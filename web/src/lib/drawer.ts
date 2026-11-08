// Swipe gestures of the sidebar drawer (narrow and touch layouts): the drawer follows a
// horizontal drag and settles open or closed on release by distance or flick speed; a
// vertical move is left to the browser (the session tree scrolls).

/** Movement (px) before a touch counts as a horizontal drag or a vertical scroll. */
export const SLOP = 10;
/** Width (px) of the left screen edge where a swipe opens the drawer. */
export const EDGE = 24;
/** Fraction of the drawer width a drag must cover to toggle it. */
const DISTANCE = 0.35;
/** Release speed (px/ms) that toggles the drawer whatever the distance. */
const FLICK = 0.4;
/** Time window (ms) the release speed is measured over. */
const WINDOW_MS = 100;

export type Axis = 'x' | 'y';

/** One touch from its start: its direction, once decided, and its recent speed. */
export class Swipe {
  readonly x0: number;
  readonly y0: number;
  axis: Axis | null = null;
  dx = 0;
  private samples: { x: number; t: number }[];

  constructor(x: number, y: number, t: number) {
    this.x0 = x;
    this.y0 = y;
    this.samples = [{ x, t }];
  }

  /** Records a move and returns the direction (decided once, past the slop). */
  move(x: number, y: number, t: number): Axis | null {
    this.dx = x - this.x0;
    const dy = y - this.y0;
    if (!this.axis && (Math.abs(this.dx) >= SLOP || Math.abs(dy) >= SLOP)) {
      this.axis = Math.abs(this.dx) > Math.abs(dy) ? 'x' : 'y';
    }
    this.samples.push({ x, t });
    while (this.samples.length > 2 && t - this.samples[0]!.t > WINDOW_MS) this.samples.shift();
    return this.axis;
  }

  /** Horizontal speed (px/ms) over the last moves; positive to the right. */
  velocity(): number {
    const a = this.samples[0]!;
    const b = this.samples[this.samples.length - 1]!;
    return b.t > a.t ? (b.x - a.x) / (b.t - a.t) : 0;
  }
}

/** The drawer's offset (px; 0 open, -width closed) while dragged by `dx` from `open`. */
export function dragOffset(open: boolean, dx: number, width: number): number {
  return Math.max(-width, Math.min(0, (open ? 0 : -width) + dx));
}

/** Whether a drag that started `open` ends with the drawer open. */
export function settle(open: boolean, dx: number, velocity: number, width: number): boolean {
  if (velocity <= -FLICK) return false;
  if (velocity >= FLICK) return true;
  const at = dragOffset(open, dx, width);
  return open ? at > -width * DISTANCE : at > -width * (1 - DISTANCE);
}
