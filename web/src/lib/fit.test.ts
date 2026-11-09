import { describe, expect, it } from 'vitest';
import { CONTROL_BAR_PX, DEFAULT_CELL, fitLayout, MIN_FONT_PX, parseFit } from './fit';

// A 1080p window leaves about 1000×700 px for the terminal.
const box = { width: 1000, height: 700 };

describe('fit', () => {
  it('parses the stored mode, screen by default', () => {
    expect(parseFit('width')).toBe('width');
    expect(parseFit('screen')).toBe('screen');
    expect(parseFit(null)).toBe('screen');
    expect(parseFit('both')).toBe('screen');
  });

  it('fits a short terminal into the box by its height', () => {
    const wide = { width: 1400, height: 700 };
    const l = fitLayout(wide, { cols: 80, rows: 24 }, 'screen');
    expect(l.effective).toBe('screen');
    // 667 px (a pixel of slack) for 24 rows of 20 px at 15 px: about 20.8 px.
    expect(l.fontPx).toBeCloseTo((15 * (700 - 1 - CONTROL_BAR_PX)) / (24 * 20), 5);
    // The mount narrows so that the width-fitting player keeps that scale.
    expect(l.width).toBe(Math.floor(80 * 9 * ((700 - 1 - CONTROL_BAR_PX) / (24 * 20))));
    expect(l.width).toBeLessThan(wide.width);
  });

  it('keeps the full width when the width binds first', () => {
    const l = fitLayout(box, { cols: 373, rows: 24 }, 'screen');
    expect(l).toMatchObject({ effective: 'screen', width: 1000 });
    expect(l.fontPx).toBeCloseTo((15 * 1000) / (373 * 9), 5);
    // 80×24 in 1000×700 just fits by width too.
    expect(fitLayout(box, { cols: 80, rows: 24 }, 'screen')).toMatchObject({ effective: 'screen', width: 1000 });
  });

  it('falls back to the width when the screen font would be too small', () => {
    // 121×70 by height: 667 / 1400 × 15 ≈ 7.1 px.
    const l = fitLayout(box, { cols: 121, rows: 70 }, 'screen');
    expect(l.effective).toBe('width');
    expect(l.width).toBe(1000);
    expect(l.fontPx).toBeCloseTo((15 * 1000) / (121 * 9), 5);
    // 121×30 by height: 667 / 600 × 15 ≈ 16.7 px, fine.
    expect(fitLayout(box, { cols: 121, rows: 30 }, 'screen').effective).toBe('screen');
  });

  it('decides at the minimum font size', () => {
    // Rows that give exactly MIN_FONT_PX by height.
    const rows = ((box.height - 1 - CONTROL_BAR_PX) * 15) / (MIN_FONT_PX * DEFAULT_CELL.charH);
    expect(fitLayout(box, { cols: 40, rows: Math.floor(rows) }, 'screen').effective).toBe('screen');
    expect(fitLayout(box, { cols: 40, rows: Math.ceil(rows) + 1 }, 'screen').effective).toBe('width');
  });

  it('follows a resize of the recorded terminal', () => {
    expect(fitLayout(box, { cols: 80, rows: 24 }, 'screen').effective).toBe('screen');
    expect(fitLayout(box, { cols: 123, rows: 78 }, 'screen').effective).toBe('width');
  });

  it('fits the width when asked to', () => {
    expect(fitLayout(box, { cols: 80, rows: 24 }, 'width')).toEqual({ effective: 'width', width: 1000, fontPx: (15 * 1000) / 720 });
  });

  it('counts the control bar only when there is one', () => {
    const tight = { width: 1000, height: 24 * 20 + 1 };
    expect(fitLayout(tight, { cols: 80, rows: 24 }, 'screen', DEFAULT_CELL, 0).fontPx).toBeCloseTo(15, 5);
    expect(fitLayout(tight, { cols: 80, rows: 24 }, 'screen').fontPx).toBeLessThan(15);
  });

  it('uses the measured cell and borders', () => {
    const cell = { charW: 8, charH: 18, bordersW: 2, bordersH: 2 };
    const l = fitLayout({ width: 2000, height: 18 * 24 + 2 + 1 + CONTROL_BAR_PX }, { cols: 80, rows: 24 }, 'screen', cell);
    expect(l.fontPx).toBeCloseTo(15, 5);
    expect(l.width).toBe(80 * 8 + 2);
  });

  it('defaults an unknown size and an unmeasured box', () => {
    expect(fitLayout(box, { cols: 0, rows: 0 }, 'screen')).toEqual(fitLayout(box, { cols: 80, rows: 24 }, 'screen'));
    expect(fitLayout({ width: 0, height: 0 }, { cols: 80, rows: 24 }, 'screen').effective).toBe('width');
    expect(fitLayout({ width: 800, height: 10 }, { cols: 80, rows: 24 }, 'screen')).toMatchObject({ effective: 'width', width: 800 });
  });
});
