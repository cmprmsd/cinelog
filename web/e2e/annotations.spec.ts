import type { Page } from '@playwright/test';
import type { Annotation, AnnotationPage, Me } from '../src/lib/api/types.ts';
import { expect, FIX, test, waitForPlayer } from './helpers.ts';

// Annotations (plan §8.4, M4 Accept 3; docs/API.md §9.4). Every spec writes text unique to
// its run, so it also works against a real server that keeps earlier annotations.

const unique = (what: string) => `${what}-${Date.now().toString(36)}${Math.floor(Math.random() * 1e4)}`;

const panel = (page: Page) => page.getByTestId('annotations-panel');
const item = (page: Page, text: string) => panel(page).getByTestId('annotation').filter({ hasText: text });
const byUuid = (page: Page, uuid: string | null) => panel(page).locator(`[data-testid="annotation"][data-uuid="${uuid}"]`);

/** The signed-in principal (the mock's is `alice`; a real server names it after its token). */
async function principal(page: Page): Promise<string> {
  return ((await (await page.request.get('/api/me')).json()) as Me).principal;
}

async function openSession(page: Page, query = '?cmd=1'): Promise<void> {
  await page.goto(`/s/${FIX.needle}${query}`);
  await waitForPlayer(page);
  await expect(page.getByTestId('current-command')).toContainText('echo hello');
}

/** Opens the composer with `a` (focus must not be in a field) and saves `body` + `tags`. */
async function annotate(page: Page, body: string, tags: string[] = []): Promise<void> {
  await page.locator('body').click({ position: { x: 1, y: 1 } });
  await page.keyboard.press('a');
  const form = panel(page).getByTestId('annotation-form');
  await expect(form).toBeVisible();
  await expect(form.getByTestId('annotation-text')).toBeFocused();
  await page.keyboard.type(body);
  if (tags.length) {
    await form.getByTestId('tag-input').locator('input').fill(tags.join(' '));
  }
  const saved = page.waitForResponse((r) => r.url().endsWith('/api/annotations') && r.request().method() === 'POST');
  await form.getByTestId('annotation-save').click();
  expect((await saved).status()).toBe(201);
  await expect(form).toHaveCount(0);
}

test.describe('annotations', () => {
  test('annotate the current command: text with links, tags, rail band, edit and delete', async ({ page }) => {
    await openSession(page);
    const word = unique('note');
    const tag = unique('tag');
    await annotate(page, `${word} <img src=x onerror="document.title='pwned'"> see https://example.com/runbook.`, [tag]);

    const a = item(page, word);
    await expect(a).toHaveCount(1);
    await expect(a.locator('.target-btn')).toHaveText('$ echo hello');
    await expect(a.getByTestId('annotation-author')).toHaveText(await principal(page));
    await expect(a.getByTestId('annotation-tag')).toHaveText(`#${tag}`);
    // Plain text: the markup stays text; only the URL is a link, without the full stop.
    await expect(a.locator('img')).toHaveCount(0);
    await expect(a.getByTestId('annotation-body')).toContainText(`<img src=x onerror="document.title='pwned'">`);
    const link = a.getByRole('link', { name: 'https://example.com/runbook' });
    await expect(link).toHaveAttribute('href', 'https://example.com/runbook');
    await expect(link).toHaveAttribute('rel', 'noopener noreferrer');
    await expect(link).toHaveAttribute('target', '_blank');
    expect(await page.title()).not.toBe('pwned');

    const uuid = await a.getAttribute('data-uuid');
    await expect(page.locator(`[data-testid="rail-annotation"][data-uuid="${uuid}"]`)).toHaveCount(1);

    // Edit by the author; Esc in the form cancels without closing the session.
    const it = byUuid(page, uuid);
    await it.getByRole('button', { name: 'Edit' }).click();
    const form = it.getByTestId('annotation-form');
    await expect(form.getByTestId('annotation-text')).toBeFocused();
    await page.keyboard.press('Escape');
    await expect(form).toHaveCount(0);
    await expect(page.getByTestId('session-view')).toBeVisible();
    await it.getByRole('button', { name: 'Edit' }).click();
    await it.getByTestId('annotation-text').fill(`${word} edited`);
    await page.keyboard.press('Control+Enter');
    await expect(it.getByTestId('annotation-body')).toHaveText(`${word} edited`);
    await expect(it.locator('.meta')).toContainText('edited');

    page.once('dialog', (d) => void d.accept());
    await it.getByRole('button', { name: 'Delete' }).click();
    await expect(byUuid(page, uuid)).toHaveCount(0);
    await expect(page.locator(`[data-testid="rail-annotation"][data-uuid="${uuid}"]`)).toHaveCount(0);
  });

  test('drag on the rail selects a span and `a` annotates it', async ({ page }) => {
    await openSession(page);
    const rail = page.getByTestId('rail');
    await rail.scrollIntoViewIfNeeded();
    const box = (await rail.boundingBox())!;
    const y = box.y + box.height / 2;
    // The needle session is 20 s long: 25 %…50 % is about 0:05–0:10.
    await page.mouse.move(box.x + box.width * 0.25, y);
    await page.mouse.down();
    await page.mouse.move(box.x + box.width * 0.4, y, { steps: 4 });
    await page.mouse.move(box.x + box.width * 0.5, y, { steps: 4 });
    await page.mouse.up();
    const sel = page.getByTestId('rail-selection');
    await expect(sel).toBeVisible();
    const from = Number(await sel.getAttribute('data-from'));
    const to = Number(await sel.getAttribute('data-to'));
    expect(from).toBeGreaterThan(4);
    expect(from).toBeLessThan(6);
    expect(to).toBeGreaterThan(9);
    expect(to).toBeLessThan(11);
    // The drag does not seek: the playhead stays on the first command.
    await expect(page.getByTestId('current-command')).toContainText('echo hello');

    const word = unique('span');
    await page.keyboard.press('a');
    const form = panel(page).getByTestId('annotation-form');
    await expect(form.getByRole('radio', { name: 'span' })).toBeChecked();
    await page.keyboard.type(word);
    await page.keyboard.press('Control+Enter');
    const a = item(page, word);
    await expect(a.locator('.target-btn')).toHaveText(/^0:0[45]–0:(09|10)$/);
    const uuid = await a.getAttribute('data-uuid');
    await expect(page.locator(`[data-testid="rail-annotation"][data-uuid="${uuid}"]`)).toHaveCount(1);

    // Esc clears the selection; a plain click still seeks.
    await page.locator('body').click({ position: { x: 1, y: 1 } });
    await page.keyboard.press('Escape');
    await expect(sel).toHaveCount(0);
    await rail.click({ position: { x: box.width * 0.62, y: box.height / 2 } });
    await expect(page.getByTestId('current-command')).toContainText('ls /srv');
  });

  test('two browsers stay in sync within 2 s', async ({ page, browser }) => {
    const other = await browser.newContext({
      storageState: 'e2e/.auth/state.json',
      ignoreHTTPSErrors: true,
      baseURL: process.env.CINELOG_E2E_URL,
    });
    try {
      const watcher = await other.newPage();
      await openSession(page);
      await openSession(watcher);
      const word = unique('sync');
      const tag = unique('shared');
      await annotate(page, word, [tag]);
      const there = item(watcher, word);
      await expect(there).toHaveCount(1, { timeout: 2000 });
      await expect(there.getByTestId('annotation-tag')).toHaveText(`#${tag}`);
      const uuid = await there.getAttribute('data-uuid');
      await expect(watcher.locator(`[data-testid="rail-annotation"][data-uuid="${uuid}"]`)).toHaveCount(1, { timeout: 2000 });

      const mine = byUuid(page, uuid);
      await mine.getByRole('button', { name: 'Edit' }).click();
      await mine.getByTestId('annotation-text').fill(`${word} v2`);
      await page.keyboard.press('Control+Enter');
      await expect(item(watcher, `${word} v2`)).toHaveCount(1, { timeout: 2000 });

      // The new tag reached the other browser's autocomplete as well.
      await watcher.locator('body').click({ position: { x: 1, y: 1 } });
      await watcher.keyboard.press('a');
      const field = panel(watcher).getByTestId('tag-input').locator('input');
      await field.fill(tag.slice(0, 6));
      await expect(panel(watcher).locator(`datalist option[value="${tag}"]`)).toHaveCount(1);
      await watcher.keyboard.press('Escape');

      page.once('dialog', (d) => void d.accept());
      await mine.getByRole('button', { name: 'Delete' }).click();
      await expect(byUuid(watcher, uuid)).toHaveCount(0, { timeout: 2000 });
    } finally {
      await other.close();
    }
  });

  test("another principal's annotation offers only adding tags", async ({ page }) => {
    const foreign: Annotation = {
      uuid: '0193a1b2-0000-7000-8000-00000000f00d',
      session: FIX.needle,
      cmd_seq: 2,
      t_from: null,
      t_to: null,
      author: 'mallory',
      body: 'written by someone else',
      tags: ['theirs'],
      created_at: 1,
      updated_at: 1,
      deleted_at: null,
    };
    await page.route('**/api/annotations?*', async (route) => {
      if (route.request().method() !== 'GET') return route.fallback();
      const res = await route.fetch();
      const body = (await res.json()) as AnnotationPage;
      await route.fulfill({ response: res, json: { ...body, annotations: [foreign, ...body.annotations] } });
    });
    await openSession(page);
    const a = item(page, 'written by someone else');
    await expect(a).toHaveCount(1);
    await expect(a.getByTestId('annotation-author')).toHaveText('mallory');
    await expect(a.getByRole('button', { name: 'Edit' })).toHaveCount(0);
    await expect(a.getByRole('button', { name: 'Delete' })).toHaveCount(0);
    await expect(a.getByRole('button', { name: 'Add tags' })).toBeVisible();
    // Its target seeks the player.
    await a.locator('.target-btn').click();
    await expect(page.getByTestId('current-command')).toContainText('grep -n needle');
  });

  test('the API refuses edits by others and keeps deleted ones out of lists', async ({ page }) => {
    await page.goto('/');
    const origin = new URL(page.url()).origin;
    const post = (path: string, data: unknown, method = 'POST') =>
      page.request.fetch(path, { method, data, headers: { Origin: origin, 'Content-Type': 'application/json' } });
    const bad = await post('/api/annotations', { session_uuid: FIX.needle, cmd_seq: 1, t_from: 1, t_to: 2, body: 'x' });
    expect(bad.status()).toBe(400);
    expect((await post('/api/annotations', { session_uuid: FIX.needle, cmd_seq: 999, body: 'x' })).status()).toBe(404);
    const word = unique('api');
    const res = await post('/api/annotations', { session_uuid: FIX.needle, t_from: 1, t_to: 2, body: word, tags: ['Api'] });
    expect(res.status()).toBe(201);
    const a = (await res.json()) as Annotation;
    expect(a).toMatchObject({ session: FIX.needle, t_from: 1, t_to: 2, author: await principal(page), body: word, tags: ['api'], deleted_at: null });
    // A body-less DELETE; the Content-Type only keeps the contract mock happy (see jsonDeletes).
    const del = await page.request.fetch(`/api/annotations/${a.uuid}`, {
      method: 'DELETE',
      headers: { Origin: origin, ...(process.env.CINELOG_E2E_MOCK ? { 'Content-Type': 'application/json' } : {}) },
    });
    expect(del.status()).toBe(204);
    const live = (await (await page.request.get(`/api/annotations?session=${FIX.needle}`)).json()) as AnnotationPage;
    expect(live.annotations.map((x) => x.uuid)).not.toContain(a.uuid);
    const all = (await (await page.request.get(`/api/annotations?session=${FIX.needle}&deleted=true`)).json()) as AnnotationPage;
    expect(all.annotations.find((x) => x.uuid === a.uuid)?.deleted_at).not.toBeNull();
    expect((await post(`/api/annotations/${a.uuid}`, { body: 'again' }, 'PATCH')).status()).toBe(404);
  });
});
