import { describe, expect, it } from 'vitest';
import { dispatch, useKeys } from './keymap';
import { focusKind, shortcutFor, type KeyLike } from './keys';

const k = (key: string, mods: Partial<KeyLike> = {}): KeyLike => ({ key, ctrlKey: false, metaKey: false, altKey: false, ...mods });

describe('shortcuts', () => {
  it('maps plain keys', () => {
    expect(shortcutFor(k('/'), 'none')).toBe('focus-search');
    expect(shortcutFor(k('j'), 'none')).toBe('next-result');
    expect(shortcutFor(k('N'), 'none')).toBe('prev-hit');
    expect(shortcutFor(k(']'), 'none')).toBe('next-chapter');
    expect(shortcutFor(k(' '), 'link')).toBe('toggle-play');
    expect(shortcutFor(k('x'), 'none')).toBeNull();
  });

  it('leaves typing, modifiers and control activation alone', () => {
    expect(shortcutFor(k('j'), 'typing')).toBeNull();
    expect(shortcutFor(k('Escape'), 'typing')).toBe('escape');
    expect(shortcutFor(k('j', { ctrlKey: true }), 'none')).toBeNull();
    expect(shortcutFor(k('Enter'), 'button')).toBeNull();
    expect(shortcutFor(k('Enter'), 'link')).toBeNull();
    expect(shortcutFor(k(' '), 'button')).toBeNull();
    expect(shortcutFor(k('Enter'), 'none')).toBe('open-result');
  });

  it('classifies focus targets', () => {
    const el = (html: string) => {
      document.body.innerHTML = html;
      return document.body.firstElementChild;
    };
    expect(focusKind(el('<input type="search">'))).toBe('typing');
    expect(focusKind(el('<input type="checkbox">'))).toBe('button');
    expect(focusKind(el('<select></select>'))).toBe('typing');
    expect(focusKind(el('<button></button>'))).toBe('button');
    expect(focusKind(el('<a href="/x">x</a>'))).toBe('link');
    expect(focusKind(el('<div></div>'))).toBe('none');
    expect(focusKind(null)).toBe('none');
  });

  it('dispatches to the newest handler and falls through on false', () => {
    const calls: string[] = [];
    const offA = useKeys({ escape: () => void calls.push('a'), help: () => void calls.push('help') });
    const offB = useKeys({ escape: () => (calls.push('b'), false) });
    expect(dispatch('escape')).toBe(true);
    expect(calls).toEqual(['b', 'a']);
    offB();
    offA();
    expect(dispatch('help')).toBe(false);
  });
});
