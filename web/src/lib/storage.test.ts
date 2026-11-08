import { afterEach, describe, expect, it, vi } from 'vitest';
import { readPref, writePref } from './storage';

describe('storage', () => {
  afterEach(() => vi.unstubAllGlobals());

  it('reads and writes', () => {
    writePref('cinelog.test', 'x');
    expect(readPref('cinelog.test')).toBe('x');
    writePref('cinelog.test', null);
    expect(readPref('cinelog.test')).toBeNull();
  });

  it('survives storage that throws', () => {
    const broken = {
      getItem: () => {
        throw new Error('SecurityError');
      },
      setItem: () => {
        throw new Error('QuotaExceeded');
      },
      removeItem: () => {
        throw new Error('SecurityError');
      },
    };
    vi.stubGlobal('localStorage', broken);
    expect(readPref('k')).toBeNull();
    expect(() => writePref('k', 'v')).not.toThrow();
  });
});
