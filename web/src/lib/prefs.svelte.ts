// UI preferences: theme (auto follows prefers-color-scheme), UTC display, keystroke overlay,
// how the terminal fits the player box.

import { parseFit, type FitMode } from './fit';
import { readPref, writePref } from './storage';

export type ThemeChoice = 'auto' | 'light' | 'dark';
export type Resolved = 'light' | 'dark';

const KEYS = { theme: 'cinelog.theme', utc: 'cinelog.utc', overlay: 'cinelog.keystrokes', fit: 'cinelog.fit' } as const;

export function parseTheme(v: string | null): ThemeChoice {
  return v === 'light' || v === 'dark' ? v : 'auto';
}

export function nextTheme(t: ThemeChoice): ThemeChoice {
  return t === 'auto' ? 'light' : t === 'light' ? 'dark' : 'auto';
}

/**
 * The player does not follow the page theme by itself, so the app picks one. `auto/` makes
 * a theme embedded in the cast win over ours.
 */
export function playerTheme(resolved: Resolved): string {
  return resolved === 'dark' ? 'auto/dracula' : 'auto/solarized-light';
}

function systemDark(): MediaQueryList | null {
  return typeof globalThis.matchMedia === 'function' ? globalThis.matchMedia('(prefers-color-scheme: dark)') : null;
}

class Prefs {
  theme = $state<ThemeChoice>(parseTheme(readPref(KEYS.theme)));
  systemDark = $state(systemDark()?.matches ?? false);
  utc = $state(readPref(KEYS.utc) === '1');
  keystrokeOverlay = $state(readPref(KEYS.overlay) === '1');
  fit = $state<FitMode>(parseFit(readPref(KEYS.fit)));
  resolved = $derived<Resolved>(this.theme === 'auto' ? (this.systemDark ? 'dark' : 'light') : this.theme);

  constructor() {
    systemDark()?.addEventListener('change', (e) => (this.systemDark = e.matches));
  }

  setTheme(t: ThemeChoice): void {
    this.theme = t;
    writePref(KEYS.theme, t === 'auto' ? null : t);
    applyTheme(t);
  }

  setUtc(on: boolean): void {
    this.utc = on;
    writePref(KEYS.utc, on ? '1' : null);
  }

  setKeystrokeOverlay(on: boolean): void {
    this.keystrokeOverlay = on;
    writePref(KEYS.overlay, on ? '1' : null);
  }

  setFit(mode: FitMode): void {
    this.fit = mode;
    writePref(KEYS.fit, mode === 'screen' ? null : mode);
  }

  toggleFit(): void {
    this.setFit(this.fit === 'screen' ? 'width' : 'screen');
  }
}

/** `data-theme` on `<html>`: absent for auto (CSS follows the media query). */
export function applyTheme(t: ThemeChoice): void {
  const root = globalThis.document?.documentElement;
  if (!root) return;
  if (t === 'auto') delete root.dataset.theme;
  else root.dataset.theme = t;
}

export const prefs = new Prefs();
