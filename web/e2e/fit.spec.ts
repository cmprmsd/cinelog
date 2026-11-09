import type { Page } from '@playwright/test';
import { expect, FIX, test, waitForPlayer } from './helpers.ts';

// The player box is bounded by the window: the session header, the terminal, the rail and the
// command strip are all in view without scrolling the page. A tall terminal (121×70 is one of
// the commonest recorded sizes) would be unreadable fitted whole, so "screen" falls back to
// the width and the terminal scrolls inside its box. Specs reshape the needle session with
// `page.route`, so they run unchanged against a real server.

const VIEWPORT = { width: 1280, height: 720 };

type Size = { cols: number; rows: number };

/**
 * Serves the needle session as recorded on a `size` terminal; with `resize`, the recording
 * resizes to that after its first command (before t=3).
 */
async function reshape(page: Page, size: Size, resize?: Size): Promise<void> {
  await page.route(`**/api/sessions/${FIX.needle}/play`, async (route) => {
    const info = await (await route.fetch()).json();
    await route.fulfill({ json: { ...info, cols: size.cols, rows: size.rows } });
  });
  await page.route(`**/api/sessions/${FIX.needle}/cast`, async (route) => {
    const lines = (await (await route.fetch()).text()).split('\n').filter(Boolean);
    const header = JSON.parse(lines[0]!);
    // asciicast v3 keeps intervals and `term`, v2 absolute times and `width`/`height`.
    const v3 = header.version === 3;
    if (v3) header.term = { ...header.term, cols: size.cols, rows: size.rows };
    else Object.assign(header, { width: size.cols, height: size.rows });
    const events: [number, string, string][] = lines.slice(1).map((l) => JSON.parse(l));
    if (resize) {
      let t = 0;
      const at = events.findIndex((e) => (t = v3 ? t + e[0] : e[0]) >= 3);
      const prev = events[at - 1]?.[0] ?? 0;
      events.splice(at, 0, [v3 ? 0 : prev, 'r', `${resize.cols}x${resize.rows}`]);
    }
    const body = [header, ...events].map((l) => JSON.stringify(l)).join('\n') + '\n';
    await route.fulfill({ status: 200, contentType: 'application/x-asciicast', body });
  });
}

const box = (page: Page) => page.getByTestId('fit-box');

async function bottomOf(page: Page, testId: string): Promise<number> {
  const b = (await page.getByTestId(testId).boundingBox())!;
  return b.y + b.height;
}

/**
 * Header, terminal box, rail, command strip and the current command line are inside the
 * window, the page unscrolled.
 */
async function expectAllInView(page: Page): Promise<void> {
  const head = (await page.getByTestId('session-view').locator('.head').boundingBox())!;
  expect(head.y).toBeGreaterThanOrEqual(0);
  for (const id of ['fit-box', 'rail', 'command-strip', 'session-foot']) {
    expect(await bottomOf(page, id), `${id} below the window`).toBeLessThanOrEqual(VIEWPORT.height);
  }
  expect(await page.evaluate(() => document.scrollingElement!.scrollTop)).toBe(0);
  expect(await page.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth)).toBe(true);
}

test.describe('player fit', () => {
  test.use({ viewport: VIEWPORT });

  test('an 80x24 terminal fits the window whole', async ({ page }) => {
    await page.goto(`/s/${FIX.needle}`);
    await waitForPlayer(page);
    await expect(box(page)).toHaveAttribute('data-term', '80x24');
    await expect(box(page)).toHaveAttribute('data-fit-mode', 'screen');
    await expect(box(page)).toHaveAttribute('data-fit', 'screen');
    await expect(page.getByTestId('fit-fallback')).toHaveCount(0);
    await expectAllInView(page);
    // The terminal and its control bar end inside the box.
    const player = (await page.locator('.ap-player').boundingBox())!;
    expect(player.y + player.height).toBeLessThanOrEqual((await bottomOf(page, 'fit-box')) + 1);
    expect(Number(await box(page).getAttribute('data-font-px'))).toBeGreaterThanOrEqual(10);
  });

  test('a tall terminal keeps the rail in view and scrolls inside its box', async ({ page }) => {
    await reshape(page, { cols: 121, rows: 70 });
    await page.goto(`/s/${FIX.needle}?cmd=2`);
    await waitForPlayer(page);
    await expect(box(page)).toHaveAttribute('data-term', '121x70');
    // Fitted whole the font would be about 5 px: the width wins, shown in the header.
    await expect(box(page)).toHaveAttribute('data-fit-mode', 'screen');
    await expect(box(page)).toHaveAttribute('data-fit', 'width');
    await expect(page.getByTestId('fit-fallback')).toBeVisible();
    await expect(page.getByTestId('fit-toggle')).toHaveAttribute('data-fit', 'width');
    expect(Number(await box(page).getAttribute('data-font-px'))).toBeGreaterThanOrEqual(10);
    await expectAllInView(page);
    // The terminal is taller than its box and scrolls there.
    const scroller = box(page).locator('.scroller');
    await expect.poll(() => scroller.evaluate((el) => el.scrollHeight > el.clientHeight + 50)).toBe(true);
    await expect(page.locator('.ap-term')).toContainText('grep -n needle haystack.txt');
    // The control bar stays at the bottom of the box, not of the terminal.
    const bar = (await page.locator('.ap-control-bar').boundingBox())!;
    const fitBottom = await bottomOf(page, 'fit-box');
    expect(bar.y + bar.height).toBeLessThanOrEqual(fitBottom + 1);
    expect(bar.y + bar.height).toBeGreaterThan(fitBottom - 25);
    await page.locator('.ap-control-bar .ap-playback-button').click();
    await expect.poll(() => page.getByTestId('player').getAttribute('data-player-time').then(Number)).toBeGreaterThan(4.5);
  });

  test('a resize in the recording does not move the page', async ({ page }) => {
    await reshape(page, { cols: 80, rows: 24 }, { cols: 121, rows: 70 });
    await page.goto(`/s/${FIX.needle}?cmd=1`);
    await waitForPlayer(page);
    await expect(box(page)).toHaveAttribute('data-term', '80x24');
    await expect(box(page)).toHaveAttribute('data-fit', 'screen');
    const before = await Promise.all(['fit-box', 'rail', 'command-strip'].map((id) => page.getByTestId(id).boundingBox()));
    await page.getByTestId('command-strip').getByRole('button', { name: '$ ls /srv' }).click();
    await expect(page.getByTestId('current-command')).toContainText('ls /srv');
    await expect(box(page)).toHaveAttribute('data-term', '121x70');
    await expect(box(page)).toHaveAttribute('data-fit', 'width');
    const after = await Promise.all(['fit-box', 'rail', 'command-strip'].map((id) => page.getByTestId(id).boundingBox()));
    for (const [i, b] of after.entries()) {
      expect(Math.abs(b!.y - before[i]!.y)).toBeLessThanOrEqual(1);
      expect(Math.abs(b!.height - before[i]!.height)).toBeLessThanOrEqual(1);
    }
    await expectAllInView(page);
    // Back before the resize: the header's size again, fitted whole.
    await page.getByTestId('command-strip').getByRole('button', { name: '$ echo hello' }).click();
    await expect(box(page)).toHaveAttribute('data-term', '80x24');
    await expect(box(page)).toHaveAttribute('data-fit', 'screen');
  });

  test('the fit toggle and w switch the mode, which persists', async ({ page }) => {
    await page.goto(`/s/${FIX.needle}`);
    await waitForPlayer(page);
    const toggle = page.getByTestId('fit-toggle');
    await expect(toggle).toHaveText('Fit: screen');
    const narrow = (await page.locator('.ap-player').boundingBox())!.width;
    await toggle.click();
    await expect(toggle).toHaveText('Fit: width');
    await expect(box(page)).toHaveAttribute('data-fit', 'width');
    // The terminal takes the full width of the box (less a scrollbar gutter).
    const full = (await box(page).boundingBox())!.width;
    await expect.poll(async () => (await page.locator('.ap-player').boundingBox())!.width).toBeGreaterThan(full - 20);
    expect(full - 20).toBeGreaterThan(narrow);
    // The rail stays where the box ends: the page does not grow.
    await expectAllInView(page);

    await page.reload();
    await waitForPlayer(page);
    await expect(page.getByTestId('fit-toggle')).toHaveText('Fit: width');
    await expect(box(page)).toHaveAttribute('data-fit-mode', 'width');

    await page.locator('body').click({ position: { x: 1, y: 1 } });
    await page.keyboard.press('w');
    await expect(page.getByTestId('fit-toggle')).toHaveText('Fit: screen');
    await page.reload();
    await waitForPlayer(page);
    await expect(box(page)).toHaveAttribute('data-fit-mode', 'screen');
  });

  test('the live view fits its box too', async ({ page }) => {
    await page.goto(`/live/${FIX.live}`);
    await expect(page.locator('[data-testid="live-player"] .ap-player')).toBeVisible();
    await expect(box(page)).not.toHaveAttribute('data-term', '0x0');
    await expect(box(page)).toHaveAttribute('data-fit', 'screen');
    expect(await bottomOf(page, 'fit-box')).toBeLessThanOrEqual(VIEWPORT.height);
    const player = (await page.locator('.ap-player').boundingBox())!;
    expect(player.y + player.height).toBeLessThanOrEqual((await bottomOf(page, 'fit-box')) + 1);
    await page.getByTestId('fit-toggle').click();
    await expect(box(page)).toHaveAttribute('data-fit', 'width');
  });
});
