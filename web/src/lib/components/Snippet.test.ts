import { flushSync, mount, unmount } from 'svelte';
import { describe, expect, it } from 'vitest';
import Snippet from './Snippet.svelte';

describe('Snippet', () => {
  it('renders untrusted parts as text, hits inside <mark>', () => {
    const target = document.createElement('div');
    const parts = [
      { text: '<img src=x onerror="alert(1)">', hit: false },
      { text: 'needle', hit: true },
      { text: '\x1b[31m\nnext', hit: false },
    ];
    const c = mount(Snippet, { target, props: { parts } });
    flushSync();
    expect(target.querySelector('img')).toBeNull();
    expect(target.querySelectorAll('mark')).toHaveLength(1);
    expect(target.querySelector('mark')?.textContent).toBe('needle');
    expect(target.textContent).toContain('<img src=x onerror="alert(1)">');
    expect(target.textContent).toContain('␛[31m ⏎ next');
    unmount(c);
  });
});
