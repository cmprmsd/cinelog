import { describe, expect, it } from 'vitest';
import { applyTheme, nextTheme, parseTheme, playerTheme, prefs } from './prefs.svelte';

describe('prefs', () => {
  it('parses and cycles themes', () => {
    expect(parseTheme('dark')).toBe('dark');
    expect(parseTheme('bogus')).toBe('auto');
    expect(parseTheme(null)).toBe('auto');
    expect(nextTheme('auto')).toBe('light');
    expect(nextTheme('light')).toBe('dark');
    expect(nextTheme('dark')).toBe('auto');
  });

  it('picks the player theme, letting a cast theme win', () => {
    expect(playerTheme('dark')).toBe('auto/dracula');
    expect(playerTheme('light')).toBe('auto/solarized-light');
  });

  it('applies and persists the choice', () => {
    prefs.setTheme('dark');
    expect(document.documentElement.dataset.theme).toBe('dark');
    expect(localStorage.getItem('cinelog.theme')).toBe('dark');
    expect(prefs.resolved).toBe('dark');
    prefs.setTheme('auto');
    expect(document.documentElement.dataset.theme).toBeUndefined();
    expect(localStorage.getItem('cinelog.theme')).toBeNull();
    applyTheme('light');
    expect(document.documentElement.dataset.theme).toBe('light');
  });

  it('remembers the fit mode, screen by default', () => {
    expect(prefs.fit).toBe('screen');
    prefs.toggleFit();
    expect(prefs.fit).toBe('width');
    expect(localStorage.getItem('cinelog.fit')).toBe('width');
    prefs.toggleFit();
    expect(prefs.fit).toBe('screen');
    expect(localStorage.getItem('cinelog.fit')).toBeNull();
  });
});
