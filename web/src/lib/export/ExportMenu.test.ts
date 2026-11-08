import { flushSync, mount, unmount } from 'svelte';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import type { Command } from '../api/types';
import ExportMenu from './ExportMenu.svelte';

const UUID = '0b9e4c1a-7d2e-4f00-9a51-0000000000a1';

const command = {
  seq: 2,
  t_start: 4,
  t_end: 5.5,
  typed: 'grep -n needle haystack.txt',
} as Command;

let fetchMock: ReturnType<typeof vi.fn>;

beforeEach(() => {
  fetchMock = vi.fn(
    async () =>
      new Response('text', { status: 200, headers: { 'Content-Disposition': 'attachment; filename="cinelog-0b9e4c1a-4-6.txt"' } }),
  );
  vi.stubGlobal('fetch', fetchMock);
  URL.createObjectURL = vi.fn(() => 'blob:x');
  URL.revokeObjectURL = vi.fn();
  // jsdom does not implement navigation; the download click must not throw.
  vi.spyOn(HTMLAnchorElement.prototype, 'click').mockImplementation(() => {});
});

afterEach(() => {
  vi.unstubAllGlobals();
  vi.restoreAllMocks();
  document.body.replaceChildren();
});

function setup(props: { command: Command | null; selection: { from: number; to: number } | null }) {
  const target = document.createElement('div');
  document.body.append(target);
  const notices: string[] = [];
  const c = mount(ExportMenu, { target, props: { uuid: UUID, ...props, onnotice: (t: string) => notices.push(t) } });
  flushSync();
  const items = () => [...target.querySelectorAll<HTMLButtonElement>('[role="menuitem"]')];
  const item = (name: string) => items().find((b) => b.getAttribute('aria-label') === name)!;
  return { target, c, notices, items, item };
}

const settle = () => new Promise((r) => setTimeout(r, 0));

describe('ExportMenu', () => {
  it('offers three formats for each available source', () => {
    const { c, items, item } = setup({ command, selection: { from: 1, to: 9 } });
    expect(items().map((b) => b.getAttribute('aria-label'))).toEqual([
      'Current command as .cast',
      'Current command as .txt',
      'Current command as .html',
      'Selection as .cast',
      'Selection as .txt',
      'Selection as .html',
      'Whole session as .cast',
      'Whole session as .txt',
      'Whole session as .html',
    ]);
    expect(items().every((b) => !b.disabled)).toBe(true);
    expect(item('Current command as .txt').textContent).toBe('.txt');
    unmount(c);
  });

  it('disables the command entries without a command and hides the selection row without one', () => {
    const { c, items, target } = setup({ command: null, selection: null });
    expect(items().filter((b) => b.disabled)).toHaveLength(3);
    expect(target.querySelector('[data-source="selection"]')).toBeNull();
    expect(target.textContent).toContain('no command at the playhead');
    unmount(c);
  });

  it('downloads each source with the right query, through fetch with the cookie', async () => {
    const { c, item, notices } = setup({ command, selection: { from: 1.23456, to: 9 } });
    item('Current command as .cast').click();
    await settle();
    item('Selection as .txt').click();
    await settle();
    item('Whole session as .html').click();
    await settle();
    const urls = fetchMock.mock.calls.map(([u]) => String(u));
    expect(urls).toEqual([
      `/api/sessions/${UUID}/export?format=cast&cmd=2`,
      `/api/sessions/${UUID}/export?format=txt&from=1.235&to=9`,
      `/api/sessions/${UUID}/export?format=html`,
    ]);
    expect(fetchMock.mock.calls.every(([, init]) => (init as RequestInit).credentials === 'same-origin')).toBe(true);
    expect(notices).toEqual(Array(3).fill('Downloaded cinelog-0b9e4c1a-4-6.txt.'));
    unmount(c);
  });

  it('reports a failed export with the server message', async () => {
    fetchMock.mockImplementation(async () => Response.json({ error: 'unavailable', message: 'index busy' }, { status: 503 }));
    const { c, item, notices } = setup({ command, selection: null });
    item('Whole session as .txt').click();
    await settle();
    await settle();
    expect(notices).toEqual(['Export failed: index busy']);
    unmount(c);
  });

  it('open() focuses the first enabled entry, arrows move, close() returns focus', () => {
    const { c, target, item } = setup({ command: null, selection: { from: 0, to: 3 } });
    const menu = target.querySelector('details')!;
    expect(c.open()).toBe(true);
    flushSync();
    expect(menu.open).toBe(true);
    expect(document.activeElement).toBe(item('Selection as .cast'));
    document.activeElement!.dispatchEvent(new KeyboardEvent('keydown', { key: 'ArrowDown', bubbles: true }));
    expect(document.activeElement).toBe(item('Whole session as .cast'));
    document.activeElement!.dispatchEvent(new KeyboardEvent('keydown', { key: 'ArrowRight', bubbles: true }));
    expect(document.activeElement).toBe(item('Whole session as .txt'));
    expect(c.close()).toBe(true);
    expect(menu.open).toBe(false);
    expect(document.activeElement).toBe(target.querySelector('summary'));
    expect(c.close()).toBe(false);
    unmount(c);
  });

  it('Esc inside the menu closes it and does not reach the app', () => {
    const { c, target } = setup({ command, selection: null });
    const seen: string[] = [];
    const onWindow = (e: KeyboardEvent) => seen.push(e.key);
    window.addEventListener('keydown', onWindow);
    c.open();
    flushSync();
    const esc = new KeyboardEvent('keydown', { key: 'Escape', bubbles: true, cancelable: true });
    document.activeElement!.dispatchEvent(esc);
    expect(target.querySelector('details')!.open).toBe(false);
    expect(esc.defaultPrevented).toBe(true);
    expect(seen).toEqual([]);
    // Closed, Esc is the app's again.
    target.querySelector('summary')!.dispatchEvent(new KeyboardEvent('keydown', { key: 'Escape', bubbles: true }));
    expect(seen).toEqual(['Escape']);
    window.removeEventListener('keydown', onWindow);
    unmount(c);
  });

  it('a click outside closes it', () => {
    const { c, target } = setup({ command, selection: null });
    c.open();
    flushSync();
    document.body.dispatchEvent(new PointerEvent('pointerdown', { bubbles: true }));
    expect(target.querySelector('details')!.open).toBe(false);
    unmount(c);
  });
});
