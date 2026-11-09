// How the terminal fits the player box. The box is as tall as the window leaves it (the rail
// and the command strip stay visible); the terminal either fits into it whole ("screen") or
// fills its width and scrolls inside it ("width"). Pure, so the rule is unit-tested without
// a browser.

export type FitMode = 'screen' | 'width';

/**
 * A terminal cell at the player's base font and the terminal's borders, in px, as
 * asciinema-player measures them when it mounts.
 */
export interface CellMetrics {
  charW: number;
  charH: number;
  bordersW: number;
  bordersH: number;
}

/** The player's base font size: its scale is relative to this. */
export const BASE_FONT_PX = 15;
/** Height of the player's control bar (asciinema-player's `CONTROL_BAR_HEIGHT`). */
export const CONTROL_BAR_PX = 32;
/** Below this font size "screen" gives up and fits the width instead. */
export const MIN_FONT_PX = 10;

/** A typical monospace cell at 15 px, used until the real one is measured. */
export const DEFAULT_CELL: CellMetrics = { charW: 9, charH: 20, bordersW: 0, bordersH: 0 };

export interface FitLayout {
  /** The mode in effect: "screen" falls back to "width" when the font would be too small. */
  effective: FitMode;
  /** Width of the player's mount in px; with "width" the whole box. */
  width: number;
  /** The terminal's font size in px. */
  fontPx: number;
}

export function parseFit(v: string | null): FitMode {
  return v === 'width' ? 'width' : 'screen';
}

/**
 * The layout of a `cols`×`rows` terminal in a box of `width`×`height` px. The player always
 * fits the width of its mount, so "screen" narrows the mount until the terminal and the
 * control bar fit the box's height. Below `MIN_FONT_PX` the terminal fits the width and the
 * box scrolls: a whole screen is no use when it cannot be read.
 */
export function fitLayout(
  box: { width: number; height: number },
  term: { cols: number; rows: number },
  mode: FitMode,
  cell: CellMetrics = DEFAULT_CELL,
  controlBar: number = CONTROL_BAR_PX,
): FitLayout {
  const cols = term.cols > 0 ? term.cols : 80;
  const rows = term.rows > 0 ? term.rows : 24;
  const termW = cell.charW * cols + cell.bordersW;
  const termH = cell.charH * rows + cell.bordersH;
  const width = Math.max(0, Math.floor(box.width));
  const byWidth = width / termW;
  // The box size comes rounded to whole pixels: a pixel of slack keeps a rounded-up height
  // from cutting the control bar.
  const byHeight = Math.max(0, box.height - 1 - controlBar) / termH;
  const wide: FitLayout = { effective: 'width', width, fontPx: BASE_FONT_PX * byWidth };
  // Not laid out yet (hidden, or before the first measurement): nothing to decide.
  if (mode === 'width' || width === 0 || byHeight === 0) return wide;
  // The width binds first: the whole terminal fits at full width.
  if (byWidth <= byHeight) return { ...wide, effective: 'screen' };
  if (BASE_FONT_PX * byHeight < MIN_FONT_PX) return wide;
  return { effective: 'screen', width: Math.floor(termW * byHeight), fontPx: BASE_FONT_PX * byHeight };
}
