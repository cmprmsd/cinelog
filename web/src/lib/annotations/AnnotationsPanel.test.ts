import { flushSync, mount, unmount } from 'svelte';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import type { Annotation, BusEvent, Command } from '../api/types';

// A bus that is open and lets the test publish.
const fakeBus = vi.hoisted(() => {
  const handlers = new Set<(e: unknown) => void>();
  return {
    status: 'open',
    subscribe(h: (e: unknown) => void) {
      handlers.add(h);
      return () => handlers.delete(h);
    },
    emit(e: unknown) {
      for (const h of handlers) h(e);
    },
  };
});
vi.mock('../api/events.svelte', () => ({ bus: fakeBus }));

const { principal } = await import('../principal.svelte');
const { default: AnnotationsPanel } = await import('./AnnotationsPanel.svelte');
const { default: AnnotationBody } = await import('./AnnotationBody.svelte');
const { default: TagInput } = await import('./TagInput.svelte');

const S = '0b9e4c1a-7d2e-4f00-9a51-0000000000a1';

function note(over: Partial<Annotation>): Annotation {
  return {
    uuid: '01900000-0000-7000-8000-000000000001',
    session: S,
    cmd_seq: 1,
    t_from: null,
    t_to: null,
    author: 'alice',
    body: 'mine',
    tags: [],
    created_at: 100,
    updated_at: 100,
    deleted_at: null,
    ...over,
  };
}

const commands = [
  { seq: 1, t_start: 1, t_end: 2, typed: 'echo hello', exit: 0 },
  { seq: 2, t_start: 4, t_end: 5, typed: 'make deploy', exit: 0 },
] as unknown as Command[];

type Call = { url: string; method: string; body: unknown };

function serve(routes: Record<string, (c: Call) => unknown>) {
  const calls: Call[] = [];
  const fetch = vi.fn(async (url: string, init?: RequestInit) => {
    const c: Call = {
      url,
      method: init?.method ?? 'GET',
      body: init?.body ? JSON.parse(String(init.body)) : undefined,
    };
    calls.push(c);
    const key = `${c.method} ${url.split('?')[0]}`;
    const handler = routes[key];
    if (!handler) return new Response(JSON.stringify({ error: 'not_found', message: 'no' }), { status: 404 });
    const out = handler(c);
    return out === undefined ? new Response(null, { status: 204 }) : new Response(JSON.stringify(out), { status: 200 });
  });
  vi.stubGlobal('fetch', fetch);
  return calls;
}

const settle = async () => {
  for (let i = 0; i < 5; i += 1) await Promise.resolve();
  await new Promise((r) => setTimeout(r, 0));
  flushSync();
};

describe('AnnotationBody', () => {
  it('renders text nodes and safe links only', () => {
    const target = document.createElement('div');
    const c = mount(AnnotationBody, {
      target,
      props: {
        body: '<img src=x onerror="alert(1)"> see https://example.com/x and javascript:alert(1)',
      },
    });
    flushSync();
    expect(target.querySelector('img')).toBeNull();
    const links = target.querySelectorAll('a');
    expect(links).toHaveLength(1);
    expect(links[0]!.getAttribute('href')).toBe('https://example.com/x');
    expect(links[0]!.getAttribute('rel')).toBe('noopener noreferrer');
    expect(links[0]!.getAttribute('target')).toBe('_blank');
    expect(target.textContent).toContain('<img src=x onerror="alert(1)">');
    unmount(c);
  });
});

describe('TagInput', () => {
  it('adds typed tags normalised and refuses bad ones', () => {
    const target = document.createElement('div');
    document.body.append(target);
    const c = mount(TagInput, {
      target,
      props: {
        tags: [],
        known: [{ name: 'prod', color: '#ff0000', count: 3 }],
      },
    });
    const input = target.querySelector('input')!;
    input.value = 'Ops';
    input.dispatchEvent(new Event('input', { bubbles: true }));
    input.dispatchEvent(new KeyboardEvent('keydown', { key: 'Enter', bubbles: true }));
    flushSync();
    expect([...target.querySelectorAll('.chip')].map((x) => x.textContent?.replace('×', ''))).toEqual(['#ops']);
    input.value = 'semi;colon';
    input.dispatchEvent(new Event('input', { bubbles: true }));
    expect(c.flush()).toBe(false);
    flushSync();
    expect(target.querySelector('[role="alert"]')?.textContent).toContain('not a valid tag');
    unmount(c);
    target.remove();
  });
});

describe('AnnotationsPanel', () => {
  let target: HTMLElement;

  beforeEach(() => {
    principal.me = { principal: 'alice' } as never;
    target = document.createElement('div');
    document.body.append(target);
  });

  afterEach(() => {
    vi.unstubAllGlobals();
    target.remove();
    principal.me = null;
  });

  function mountPanel(over: Record<string, unknown> = {}) {
    const onseek = vi.fn();
    const c = mount(AnnotationsPanel, {
      target,
      props: {
        session: S,
        commands,
        current: commands[0]!,
        currentTime: 1.5,
        selection: null,
        onseek,
        ...over,
      },
    });
    return { c, onseek };
  }

  const items = () => [...target.querySelectorAll<HTMLElement>('[data-testid="annotation"]')];

  it('lists annotations with author-only actions and seeks to their target', async () => {
    const calls = serve({
      'GET /api/annotations': () => ({
        annotations: [
          note({ uuid: 'a1', tags: ['prod'] }),
          note({
            uuid: 'b1',
            author: 'bob',
            cmd_seq: null,
            t_from: 4,
            t_to: 6,
            body: 'theirs',
            created_at: 200,
          }),
        ],
        next: null,
      }),
      'GET /api/tags': () => ({
        tags: [{ name: 'prod', color: '#ff0000', count: 1 }],
      }),
      'PATCH /api/annotations/b1': (c) => ({
        ...note({
          uuid: 'b1',
          author: 'bob',
          cmd_seq: null,
          t_from: 4,
          t_to: 6,
          body: 'theirs',
          created_at: 200,
        }),
        tags: (c.body as { add_tags: string[] }).add_tags,
        updated_at: 300,
      }),
    });
    const { c, onseek } = mountPanel();
    await settle();
    expect(calls[0]!.url).toContain(`session=${S}`);
    const [mine, theirs] = items();
    expect(mine!.textContent).toContain('$ echo hello');
    expect(mine!.classList.contains('here')).toBe(true);
    expect([...mine!.querySelectorAll('button')].map((b) => b.textContent)).toEqual(
      expect.arrayContaining(['Edit', 'Delete']),
    );
    expect(theirs!.textContent).toContain('0:04–0:06');
    const theirButtons = [...theirs!.querySelectorAll('button')].map((b) => b.textContent);
    expect(theirButtons).not.toContain('Edit');
    expect(theirButtons).not.toContain('Delete');
    theirs!.querySelector<HTMLButtonElement>('.target-btn')!.click();
    expect(onseek).toHaveBeenCalledWith(4);

    [...theirs!.querySelectorAll('button')].find((b) => b.textContent === 'Add tags')!.click();
    await settle();
    const input = theirs!.querySelector<HTMLInputElement>('form input')!;
    input.value = 'reviewed';
    input.dispatchEvent(new Event('input', { bubbles: true }));
    theirs!.querySelector('form')!.dispatchEvent(new Event('submit', { cancelable: true }));
    await settle();
    expect(calls.at(-1)).toMatchObject({
      method: 'PATCH',
      body: { add_tags: ['reviewed'] },
    });
    expect(items()[1]!.textContent).toContain('#reviewed');
    unmount(c);
  });

  it('annotates the current command and follows bus events', async () => {
    const calls = serve({
      'GET /api/annotations': () => ({ annotations: [], next: null }),
      'GET /api/tags': () => ({ tags: [] }),
      'POST /api/annotations': (c) =>
        note({
          uuid: 'new',
          body: (c.body as { body: string }).body,
          cmd_seq: 1,
        }),
    });
    const { c } = mountPanel();
    await settle();
    expect(target.textContent).toContain('No annotations yet');
    expect(c.annotate()).toBe(true);
    await settle();
    const text = target.querySelector<HTMLTextAreaElement>('[data-testid="annotation-text"]')!;
    expect(document.activeElement).toBe(text);
    text.value = 'looks fine  ';
    text.dispatchEvent(new Event('input', { bubbles: true }));
    target.querySelector('[data-testid="annotation-form"]')!.dispatchEvent(new Event('submit', { cancelable: true }));
    await settle();
    expect(calls.find((x) => x.method === 'POST')!.body).toEqual({
      session_uuid: S,
      cmd_seq: 1,
      body: 'looks fine',
      tags: [],
    });
    expect(target.querySelector('[data-testid="annotation-form"]')).toBeNull();
    expect(items().map((x) => x.dataset.uuid)).toEqual(['new']);

    const other = note({
      uuid: 'zz',
      author: 'bob',
      body: 'from elsewhere',
      created_at: 500,
      updated_at: 500,
    });
    fakeBus.emit({
      type: 'annotation',
      data: { op: 'created', annotation: other },
    } satisfies BusEvent);
    fakeBus.emit({
      type: 'annotation',
      data: {
        op: 'created',
        annotation: { ...other, uuid: 'x', session: 'another' },
      },
    } satisfies BusEvent);
    flushSync();
    expect(items().map((x) => x.dataset.uuid)).toEqual(['new', 'zz']);
    fakeBus.emit({
      type: 'annotation',
      data: {
        op: 'deleted',
        annotation: { ...other, deleted_at: 600, updated_at: 600 },
      },
    } satisfies BusEvent);
    flushSync();
    expect(items().map((x) => x.dataset.uuid)).toEqual(['new']);
    // A racing add_tags published after the delete does not bring it back.
    fakeBus.emit({
      type: 'annotation',
      data: { op: 'updated', annotation: { ...other, tags: ['late'], updated_at: 550 } },
    } satisfies BusEvent);
    flushSync();
    expect(items().map((x) => x.dataset.uuid)).toEqual(['new']);
    unmount(c);
  });

  it('keeps an annotation deleted here out of a stale reload', async () => {
    // The server still lists it: the reload started before the delete committed.
    serve({
      'GET /api/annotations': () => ({
        annotations: [note({ uuid: 'a1' }), note({ uuid: 'a2', created_at: 200 })],
        next: null,
      }),
      'GET /api/tags': () => ({ tags: [] }),
      'DELETE /api/annotations/a1': () => undefined,
    });
    vi.stubGlobal('confirm', () => true);
    const { c } = mountPanel();
    await settle();
    expect(items().map((x) => x.dataset.uuid)).toEqual(['a1', 'a2']);
    [...items()[0]!.querySelectorAll('button')].find((b) => b.textContent === 'Delete')!.click();
    await settle();
    expect(items().map((x) => x.dataset.uuid)).toEqual(['a2']);
    fakeBus.emit({ type: 'hello', data: {} } as unknown as BusEvent);
    await settle();
    expect(items().map((x) => x.dataset.uuid)).toEqual(['a2']);
    unmount(c);
  });

  it('prefers the selection and has nothing to annotate without a target', async () => {
    const calls = serve({
      'GET /api/annotations': () => ({ annotations: [], next: null }),
      'GET /api/tags': () => ({ tags: [] }),
      'POST /api/annotations': () => note({ uuid: 'span', cmd_seq: null, t_from: 2, t_to: 3.5 }),
    });
    const empty = mountPanel({ current: null });
    await settle();
    expect(empty.c.annotate()).toBe(false);
    unmount(empty.c);

    const { c } = mountPanel({ selection: { from: 2.004, to: 3.5 } });
    await settle();
    expect(c.annotate()).toBe(true);
    await settle();
    const tagField = target.querySelector<HTMLInputElement>('[data-testid="tag-input"] input')!;
    tagField.value = 'Incident';
    tagField.dispatchEvent(new Event('input', { bubbles: true }));
    target.querySelector('[data-testid="annotation-form"]')!.dispatchEvent(new Event('submit', { cancelable: true }));
    await settle();
    expect(calls.find((x) => x.method === 'POST')!.body).toEqual({
      session_uuid: S,
      t_from: 2,
      t_to: 3.5,
      body: '',
      tags: ['incident'],
    });
    unmount(c);
  });
});
