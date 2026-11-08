import { describe, expect, it } from 'vitest';
import { fmtBytes, fmtClock, fmtDuration, fmtTs, keyLabel, oneLine, tzLabel } from './format';

describe('format', () => {
  it('formats epoch seconds in UTC', () => {
    expect(fmtTs(0, true)).toBe('1970-01-01 00:00:00');
    expect(fmtTs(1_700_000_000.9, true)).toBe('2023-11-14 22:13:20');
    expect(fmtTs(Number.NaN, true)).toBe('?');
  });

  it('labels the zone', () => {
    expect(tzLabel(true)).toBe('UTC');
    expect(tzLabel(false)).not.toBe('');
  });

  it('formats durations', () => {
    expect(fmtDuration(null)).toBe('–');
    expect(fmtDuration(0.25)).toBe('250ms');
    expect(fmtDuration(12.34)).toBe('12.3s');
    expect(fmtDuration(185)).toBe('3m 05s');
    expect(fmtDuration(3720)).toBe('1h 02m');
    expect(fmtDuration(2 * 86400 + 4 * 3600)).toBe('2d 4h');
  });

  it('formats clock positions and sizes', () => {
    expect(fmtClock(65.9)).toBe('1:05');
    expect(fmtClock(3725)).toBe('1:02:05');
    expect(fmtClock(-3)).toBe('0:00');
    expect(fmtBytes(512)).toBe('512 B');
    expect(fmtBytes(1536)).toBe('1.5 KiB');
    expect(fmtBytes(517 * 1024 * 1024)).toBe('517 MiB');
  });

  it('shows control characters visibly on one line', () => {
    expect(oneLine('a\nb\r\nc')).toBe('a ⏎ b ⏎ c');
    expect(oneLine('x\x1b[31my')).toBe('x␛[31my');
    expect(oneLine('del\x7f')).toBe('del␡');
    expect(oneLine('<img onerror=x>')).toBe('<img onerror=x>');
  });

  it('labels keystrokes', () => {
    expect(keyLabel('\r')).toBe('⏎');
    expect(keyLabel('\x03')).toBe('^C');
    expect(keyLabel('\x1b[A')).toBe('↑');
    expect(keyLabel('a')).toBe('a');
    expect(keyLabel('\x1bOP')).toBe('EscOP');
  });
});
