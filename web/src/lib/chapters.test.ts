import { describe, expect, it } from 'vitest';
import { chapterAt, exitClass, nextChapter, pct, prevChapter, stepHit } from './chapters';
import { cmd } from './test-data';

const cmds = [cmd(1, 1), cmd(2, 4), cmd(3, 8), cmd(4, 12)];

describe('chapters', () => {
  it('finds the chapter playing at a time', () => {
    expect(chapterAt(cmds, 0)).toBe(-1);
    expect(chapterAt(cmds, 1)).toBe(0);
    expect(chapterAt(cmds, 3.99)).toBe(1);
    expect(chapterAt(cmds, 100)).toBe(3);
    expect(chapterAt([], 5)).toBe(-1);
  });

  it('steps to the next chapter even right after a seek', () => {
    expect(nextChapter(cmds, 0)?.seq).toBe(1);
    expect(nextChapter(cmds, 4.01)?.seq).toBe(3);
    expect(nextChapter(cmds, 12)).toBeUndefined();
  });

  it('restarts the current chapter or goes one back', () => {
    expect(prevChapter(cmds, 6)?.seq).toBe(2);
    expect(prevChapter(cmds, 4.2)?.seq).toBe(1);
    expect(prevChapter(cmds, 1.1)?.seq).toBe(1);
    expect(prevChapter(cmds, 0.5)).toBeUndefined();
  });

  it('classifies exits', () => {
    expect(exitClass(0)).toBe('ok');
    expect(exitClass(2)).toBe('fail');
    expect(exitClass(null)).toBe('unknown');
  });

  it('steps between hits', () => {
    const hits = [2, 5, 9];
    expect(stepHit(hits, 0, 1)).toBe(2);
    expect(stepHit(hits, 2.05, 1)).toBe(5);
    expect(stepHit(hits, 9, 1)).toBeUndefined();
    expect(stepHit(hits, 9, -1)).toBe(5);
    expect(stepHit(hits, 2, -1)).toBeUndefined();
  });

  it('maps times to rail percentages', () => {
    expect(pct(5, 10)).toBe(50);
    expect(pct(20, 10)).toBe(100);
    expect(pct(1, 0)).toBe(0);
  });
});
