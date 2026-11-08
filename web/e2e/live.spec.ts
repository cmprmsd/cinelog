import type { Page } from '@playwright/test';
import type { LiveControl } from './mock/m4/live.ts';
import { expect, FIX, test } from './helpers.ts';

// Live view (plan §8.4, §12.3; docs/API.md §9.1, §9.2; M4 Accept 1 and 5). The tests that
// change the recorder (grow the cast, pause, crash, end, refuse the stream) drive the mock's
// control endpoint; the others also run against a real server.

const mock = !!process.env.CINELOG_E2E_MOCK;

/** POSTs JSON from inside the page (same origin, same cookie and User-Agent). */
async function post(page: Page, path: string, body: unknown): Promise<{ status: number; json: unknown }> {
  return page.evaluate(
    async ([p, b]) => {
      const res = await fetch(p, { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify(b) });
      return { status: res.status, json: await res.json().catch(() => null) };
    },
    [path, body] as const,
  );
}

/** Sends a control to the mock; answers its JSON. */
async function control(page: Page, body: LiveControl, uuid: string = FIX.live): Promise<Record<string, unknown>> {
  const { status, json } = await post(page, `/api/_mock/live/${uuid}`, body);
  expect(status, `mock live control ${body.op}`).toBe(200);
  return json as Record<string, unknown>;
}

/** The recorder status a fresh bus connection's `hello` reports for `uuid`. */
async function helloStatus(page: Page, uuid: string): Promise<string | null> {
  return page.evaluate(
    (id) =>
      new Promise<string | null>((resolve) => {
        const es = new EventSource('/api/events');
        const done = (v: string | null) => {
          es.close();
          resolve(v);
        };
        es.addEventListener('hello', (e) => {
          const live = (JSON.parse((e as MessageEvent<string>).data) as { live: { session: { uuid: string }; status: string }[] }).live;
          done(live.find((l) => l.session.uuid === id)?.status ?? null);
        });
        es.addEventListener('error', () => done(null));
        setTimeout(() => done(null), 5000);
      }),
    uuid,
  );
}

const badge = (page: Page) => page.getByTestId('live-badge');
const term = (page: Page) => page.locator('[data-testid="live-player"] .ap-term');

async function openLive(page: Page, uuid: string = FIX.live): Promise<void> {
  await page.goto(`/live/${uuid}`);
  await expect(page.getByTestId('live-page')).toHaveAttribute('data-uuid', uuid);
}

test.describe('live view', () => {
  test.afterEach(async ({ page }) => {
    if (!mock) return;
    if (!page.url().startsWith('https:')) await page.goto('/');
    await control(page, { op: 'reset' });
  });

  test('a late joiner sees the current screen at once', async ({ page }) => {
    await openLive(page);
    await expect(badge(page)).toHaveAttribute('data-kind', 'live');
    await expect(badge(page)).toHaveText('LIVE');
    await expect(term(page)).toContainText('up 4 days');
    await expect(page.getByTestId('live-view')).toHaveAttribute('data-stream', 'open');
  });

  test('appended output shows within 2 s; a second viewer joins late; the view is read-only', async ({ page, context }) => {
    test.skip(!mock, 'grows the cast through the mock');
    await openLive(page);
    await expect(badge(page)).toHaveText('LIVE');
    await expect(term(page)).toContainText('up 4 days');

    await control(page, { op: 'append', data: 'tail-line-7f3\r\n' });
    await expect(term(page)).toContainText('tail-line-7f3', { timeout: 2000 });
    await control(page, { op: 'input', data: 'q' });
    await expect(page.getByTestId('live-keystrokes')).toContainText('q', { timeout: 2000 });

    const late = await context.newPage();
    await openLive(late);
    await expect(term(late)).toContainText('tail-line-7f3');
    await expect(term(late)).toContainText('up 4 days');
    await late.close();

    // Typing into the terminal sends nothing anywhere and changes nothing.
    const writes: string[] = [];
    page.on('request', (r) => {
      if (r.method() !== 'GET') writes.push(`${r.method()} ${r.url()}`);
    });
    await page.locator('[data-testid="live-player"] .ap-player').click();
    await page.keyboard.type('rm -rf zzz');
    await page.keyboard.press('Enter');
    await page.waitForTimeout(500);
    expect(writes).toEqual([]);
    await expect(term(page)).not.toContainText('rm -rf zzz');
    await expect(page).toHaveURL(new RegExp(`/live/${FIX.live}$`));
  });

  test('paused (tmux) and nested pauses show on the badge, resume shows LIVE again', async ({ page }) => {
    test.skip(!mock, 'pauses the recorder through the mock');
    await openLive(page);
    await expect(badge(page)).toHaveText('LIVE');
    await control(page, { op: 'pause', reason: 'multiplexer', detail: 'tmux: client' });
    await expect(badge(page)).toHaveText('paused (tmux)', { timeout: 2000 });
    await expect(badge(page)).toHaveAttribute('data-kind', 'paused');
    await control(page, { op: 'pause', reason: 'nested', detail: FIX.nested });
    await expect(badge(page)).toHaveText('paused (nested)');
    await expect(page.getByTestId('live-child')).toHaveAttribute('href', `/live/${FIX.nested}`);
    await control(page, { op: 'resume' });
    await expect(badge(page)).toHaveText('LIVE', { timeout: 2000 });
    await expect(page.getByTestId('live-child')).toHaveCount(0);
    // The screen stayed and keeps updating.
    await control(page, { op: 'append', data: 'after-resume\r\n' });
    await expect(term(page)).toContainText('after-resume', { timeout: 2000 });
  });

  test('a pause that began before an in-app navigation shows at once', async ({ page }) => {
    test.skip(!mock, 'pauses the recorder through the mock');
    await page.goto('/');
    const inSidebar = page.getByTestId('live-list').locator(`a[href="/live/${FIX.live}"]`);
    await expect(inSidebar).toHaveCount(1);
    await control(page, { op: 'pause', reason: 'multiplexer', detail: 'tmux: client' });
    await expect(inSidebar).toHaveAttribute('data-status', 'paused', { timeout: 2000 });
    // The shared bus's `hello` went by long ago; the view must still show the pause.
    expect(await helloStatus(page, FIX.live)).toBe('paused');

    // From the sidebar.
    await inSidebar.click();
    await expect(page.getByTestId('live-page')).toHaveAttribute('data-uuid', FIX.live);
    await expect(badge(page)).toHaveText('paused (tmux)', { timeout: 2000 });
    // From the recording with l.
    await page.getByTestId('open-recording').click();
    await expect(page).toHaveURL(new RegExp(`/s/${FIX.live}$`));
    await page.locator('body').click({ position: { x: 1, y: 1 } });
    await page.keyboard.press('l');
    await expect(page).toHaveURL(new RegExp(`/live/${FIX.live}$`));
    await expect(badge(page)).toHaveText('paused (tmux)', { timeout: 2000 });
  });

  test('a forgotten command is masked on the live screen', async ({ page }) => {
    test.skip(!mock, 'runs a command through the mock');
    await openLive(page);
    await expect(badge(page)).toHaveText('LIVE');
    const { seq } = await control(page, { op: 'command', typed: 'cat live-secret.txt', output: 'live-secret-9c1' });
    await expect(term(page)).toContainText('live-secret-9c1', { timeout: 2000 });

    const res = await post(page, '/api/forget', { session_uuid: FIX.live, cmd_seq: seq });
    expect(res.status).toBe(200);
    // The `forgotten` bus event reopens the stream; the fresh join gets the masked screen.
    await expect(term(page)).toContainText('[redacted by', { timeout: 3000 });
    await expect(term(page)).not.toContainText('live-secret-9c1');
    await expect(term(page)).toContainText('up 4 days');
    await expect(badge(page)).toHaveText('LIVE');
  });

  test('a recorder crash shows offline; the sidebar drops and regains the session within 2 s', async ({ page }) => {
    test.skip(!mock, 'crashes the recorder through the mock');
    await openLive(page);
    await expect(badge(page)).toHaveText('LIVE');
    const inSidebar = page.getByTestId('live-list').locator(`a[href="/live/${FIX.live}"]`);
    await expect(inSidebar).toHaveCount(1);
    await control(page, { op: 'append', data: 'last-words\r\n' });
    await expect(term(page)).toContainText('last-words', { timeout: 2000 });

    await control(page, { op: 'crash' });
    await expect(badge(page)).toHaveText('offline', { timeout: 2000 });
    await expect(page.getByTestId('live-view')).toHaveAttribute('data-stream', 'offline');
    await expect(inSidebar).toHaveCount(0, { timeout: 2000 });
    // The last screen stays visible.
    await expect(term(page)).toContainText('last-words');

    // A viewer arriving after the crash sees the last screen and offline.
    await page.reload();
    await expect(badge(page)).toHaveText('offline');
    await expect(term(page)).toContainText('last-words');

    // A session (re)appearing on the bus shows up again.
    await control(page, { op: 'reset' });
    await expect(inSidebar).toHaveCount(1, { timeout: 2000 });
  });

  test('a normal end shows ended and links to the recording', async ({ page }) => {
    test.skip(!mock, 'ends the session through the mock');
    await openLive(page);
    await expect(badge(page)).toHaveText('LIVE');
    await control(page, { op: 'end' });
    await expect(badge(page)).toHaveText('ended', { timeout: 2000 });
    await expect(term(page)).toContainText('up 4 days');
    await page.getByRole('link', { name: 'Watch the recording' }).click();
    await expect(page).toHaveURL(new RegExp(`/s/${FIX.live}$`));
    await expect(page.getByTestId('session-view')).toBeVisible();
  });

  test('an ended session opened live shows its final screen', async ({ page }) => {
    await openLive(page, FIX.needle);
    await expect(badge(page)).toHaveText('ended');
    await expect(term(page)).toContainText('false-command');
  });

  test('an unknown session says so', async ({ page }) => {
    await openLive(page, '0b9e4c1a-7d2e-4f00-9a51-00000000ffff');
    await expect(badge(page)).toHaveAttribute('data-kind', 'missing');
    await expect(page.getByRole('alert')).toContainText('does not exist or was forgotten');
  });

  /**
   * The indexer lags the recorder by a few seconds: until it catches up, the metadata of a
   * fresh live session is 404 while the bus lists it and its stream works.
   */
  async function unindexedFor(page: Page, ms: number): Promise<{ misses: () => number }> {
    const until = Date.now() + ms;
    let misses = 0;
    await page.route(`**/api/sessions/${FIX.live}`, async (route) => {
      if (Date.now() >= until) return route.continue();
      misses += 1;
      return route.fulfill({ status: 404, json: { error: 'not_found', message: 'no such session' } });
    });
    return { misses: () => misses };
  }

  for (const via of ['the sidebar', 'a cold load without the bus'] as const) {
    test(`a live session that is not indexed yet streams from ${via} and gets its metadata later`, async ({ page }) => {
      if (via === 'the sidebar') {
        await page.goto('/');
      } else {
        // Without the bus, only a probe of the stream tells a fresh session from a missing one.
        await page.route('**/api/events', (route) => route.abort());
      }
      const unindexed = await unindexedFor(page, 3000);
      const indexed = page.waitForResponse((r) => new URL(r.url()).pathname === `/api/sessions/${FIX.live}` && r.status() === 200, {
        timeout: 10_000,
      });
      if (via === 'the sidebar') {
        await page.getByTestId('live-list').locator(`a[href="/live/${FIX.live}"]`).click();
        await expect(page.getByTestId('live-page')).toHaveAttribute('data-uuid', FIX.live);
      } else {
        await openLive(page);
      }
      await expect(page.getByTestId('live-view')).toHaveAttribute('data-stream', 'open');
      await expect(term(page)).toContainText('up 4 days');
      await expect(badge(page)).toHaveAttribute('data-kind', 'live');
      await expect(page.getByRole('alert')).toHaveCount(0);
      expect(unindexed.misses()).toBeGreaterThan(0);
      // Once indexed, the view fetches the metadata; it never said `not found`.
      await indexed;
      await expect(page.getByTestId('live-view').locator('.meta')).toContainText('started');
      await expect(page.getByTestId('live-view')).toHaveAttribute('data-stream', 'open');
      await expect(badge(page)).not.toHaveText('not found');
    });
  }

  test('a refused stream shows the recording so far and switches to live when it is back', async ({ page }) => {
    test.skip(!mock, 'refuses the stream through the mock');
    await page.goto('/');
    await control(page, { op: 'unavailable', on: true });
    await openLive(page);
    await expect(page.getByTestId('live-unavailable')).toBeVisible({ timeout: 6000 });
    await expect(badge(page)).toHaveText('unavailable');
    const fallback = page.getByTestId('live-fallback');
    await expect(fallback.getByTestId('session-view')).toHaveAttribute('data-uuid', FIX.live);
    await expect(fallback.locator('.ap-player')).toBeVisible();

    await control(page, { op: 'unavailable', on: false });
    await expect(badge(page)).toHaveText('LIVE', { timeout: 6000 });
    await expect(page.getByTestId('live-fallback')).toHaveCount(0);
    await expect(term(page)).toContainText('up 4 days');
  });

  test('l switches between the recording and the live view', async ({ page }) => {
    await page.goto(`/s/${FIX.live}`);
    await expect(page.getByTestId('watch-live')).toBeVisible();
    await page.locator('body').click({ position: { x: 1, y: 1 } });
    await page.keyboard.press('l');
    await expect(page).toHaveURL(new RegExp(`/live/${FIX.live}$`));
    await expect(badge(page)).toHaveText('LIVE');
    await page.locator('body').click({ position: { x: 1, y: 1 } });
    await page.keyboard.press('l');
    await expect(page).toHaveURL(new RegExp(`/s/${FIX.live}$`));
  });
});
