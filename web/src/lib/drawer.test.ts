import { describe, expect, it } from 'vitest';
import { dragOffset, settle, SLOP, Swipe } from './drawer';

describe('Swipe', () => {
  it('stays undecided within the slop', () => {
    const s = new Swipe(100, 100, 0);
    expect(s.move(100 + SLOP - 1, 100 - SLOP + 1, 10)).toBeNull();
  });

  it('decides the axis once, by the dominant direction', () => {
    const s = new Swipe(100, 100, 0);
    expect(s.move(80, 104, 10)).toBe('x');
    // A later vertical drift does not turn a drag into a scroll.
    expect(s.move(70, 160, 20)).toBe('x');
    expect(s.dx).toBe(-30);
    const v = new Swipe(100, 100, 0);
    expect(v.move(104, 130, 10)).toBe('y');
  });

  it('measures the speed over the last moves only', () => {
    const s = new Swipe(300, 0, 0);
    s.move(290, 0, 500);
    s.move(280, 0, 1000);
    s.move(200, 0, 1050);
    // The slow start is outside the window: 80 px in 50 ms.
    expect(s.velocity()).toBeCloseTo(-1.6);
    expect(new Swipe(0, 0, 0).velocity()).toBe(0);
  });
});

describe('drawer drag', () => {
  it('follows the finger within the drawer width', () => {
    expect(dragOffset(true, -50, 300)).toBe(-50);
    expect(dragOffset(true, 40, 300)).toBe(0);
    expect(dragOffset(true, -400, 300)).toBe(-300);
    expect(dragOffset(false, 120, 300)).toBe(-180);
    expect(dragOffset(false, -20, 300)).toBe(-300);
  });

  it('closes past a third of the width or on a flick', () => {
    expect(settle(true, -60, 0, 300)).toBe(true);
    expect(settle(true, -120, 0, 300)).toBe(false);
    expect(settle(true, -30, -0.8, 300)).toBe(false);
    // A flick back keeps it open even after a long drag.
    expect(settle(true, -200, 0.8, 300)).toBe(true);
  });

  it('opens from the edge past a third of the width or on a flick', () => {
    expect(settle(false, 60, 0, 300)).toBe(false);
    expect(settle(false, 120, 0, 300)).toBe(true);
    expect(settle(false, 30, 0.8, 300)).toBe(true);
  });
});
