import { describe, expect, it } from 'vitest';
import { filenameFromDisposition } from './download';

describe('filenameFromDisposition', () => {
  it('reads plain and RFC 5987 names', () => {
    expect(filenameFromDisposition('attachment; filename="cinelog-0b9e4c1a-0-12.cast"', 'f')).toBe('cinelog-0b9e4c1a-0-12.cast');
    expect(filenameFromDisposition("attachment; filename*=UTF-8''r%C3%A9sum%C3%A9.txt", 'f')).toBe('résumé.txt');
    expect(filenameFromDisposition('attachment; filename=plain.txt', 'f')).toBe('plain.txt');
  });

  it('never returns a path or an empty name', () => {
    expect(filenameFromDisposition('attachment; filename="../../etc/passwd"', 'f')).toBe('passwd');
    expect(filenameFromDisposition('attachment; filename=".."', 'f')).toBe('f');
    expect(filenameFromDisposition('attachment', 'f')).toBe('f');
    expect(filenameFromDisposition(null, 'f')).toBe('f');
  });
});
