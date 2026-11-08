import { describe, expect, it } from 'vitest';
import { rangeOf } from './spans';

describe('spans', () => {
  it('orders and clamps a selection', () => {
    expect(rangeOf(5, 2, 10)).toEqual({ from: 2, to: 5 });
    expect(rangeOf(-1, 12, 10)).toEqual({ from: 0, to: 10 });
  });
});
