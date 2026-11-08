import { expect, FIX, playerTime, settledPlayerTime, test, waitForPlayer } from './helpers.ts';

test.describe('shell', () => {
  test('sidebar lists live sessions and the session tree', async ({ page }) => {
    await page.goto('/');
    const live = page.getByTestId('live-list');
    await expect(live.locator('.live-dot')).toHaveCount(1);
    await expect(live).toContainText('alice@lab1');
    const tree = page.getByTestId('session-tree');
    await expect(tree.getByText('lab1', { exact: true })).toBeVisible();
    await tree.getByText('work', { exact: true }).click();
    await expect(tree.getByText('%1')).toBeVisible();
    // The nested sudo session sits under its parent.
    const parent = tree.locator(`li:has(> a[href="/s/${FIX.needle}"])`);
    await expect(parent.locator(`a[href="/s/${FIX.nested}"]`)).toBeVisible();
    // A live session opens the live view.
    await live.getByRole('link').first().click();
    await expect(page).toHaveURL(new RegExp(`/live/${FIX.live}$`));
    await expect(page.getByTestId('live-page')).toHaveAttribute('data-uuid', FIX.live);
  });

  test('theme toggle switches and persists; both themes render', async ({ page }) => {
    await page.emulateMedia({ colorScheme: 'light' });
    await page.goto(`/s/${FIX.needle}`);
    await waitForPlayer(page);
    const bg = () => page.evaluate(() => getComputedStyle(document.body).backgroundColor);
    const light = await bg();
    const toggle = page.getByTestId('theme-toggle');
    await expect(toggle).toHaveAttribute('data-theme-choice', 'auto');
    await toggle.click();
    await expect(toggle).toHaveAttribute('data-theme-choice', 'light');
    await toggle.click();
    await expect(toggle).toHaveAttribute('data-theme-choice', 'dark');
    await expect(page.locator('html')).toHaveAttribute('data-theme', 'dark');
    const dark = await bg();
    expect(dark).not.toBe(light);
    await expect(page.locator('.ap-player')).toBeVisible();
    await page.reload();
    await expect(page.locator('html')).toHaveAttribute('data-theme', 'dark');
    await waitForPlayer(page);
    // Auto follows the system scheme.
    await toggle.click();
    await expect(toggle).toHaveAttribute('data-theme-choice', 'auto');
    await page.emulateMedia({ colorScheme: 'dark' });
    await expect.poll(bg).toBe(dark);
  });

  test('keyboard only: search, pick a result, play, chapters, help', async ({ page }) => {
    await page.goto('/');
    await page.keyboard.press('/');
    await expect(page.getByTestId('search-input')).toBeFocused();
    await page.keyboard.type('needle');
    await expect(page).toHaveURL(/q=needle/);
    await expect(page.getByTestId('result-count')).not.toContainText('…');
    await page.keyboard.press('Escape');
    await expect(page.getByTestId('search-input')).not.toBeFocused();
    await page.keyboard.press('j');
    await expect(page.locator('[data-testid="results"] .row.active')).toHaveCount(1);
    const first = await page.locator('[data-testid="results"] .row.active').innerText();
    await page.keyboard.press('j');
    await page.keyboard.press('k');
    await expect(page.locator('[data-testid="results"] .row.active')).toHaveText(first);
    await page.keyboard.press('Enter');
    await waitForPlayer(page);
    const start = await settledPlayerTime(page);
    await page.keyboard.press('Space');
    await expect.poll(() => playerTime(page), { timeout: 5000 }).toBeGreaterThan(start + 0.3);
    await page.keyboard.press('Space');
    await page.keyboard.press(']');
    await expect(page.getByTestId('current-command')).toBeVisible();
    await page.keyboard.press('?');
    await expect(page.getByRole('dialog', { name: 'Keyboard shortcuts' })).toBeVisible();
    await page.keyboard.press('Escape');
    await expect(page.getByRole('dialog', { name: 'Keyboard shortcuts' })).toBeHidden();
    // Esc on the result list closes the player.
    await page.keyboard.press('Escape');
    await expect(page.getByTestId('session-view')).toBeHidden();
  });

  test('copy link addresses the current command and opens it', async ({ page, context }) => {
    await context.grantPermissions(['clipboard-read', 'clipboard-write']);
    await page.goto(`/s/${FIX.needle}?cmd=4`);
    await waitForPlayer(page);
    await expect(page.getByTestId('current-command')).toContainText('ls /srv');
    await page.getByRole('button', { name: 'Copy link' }).click();
    const link = await page.evaluate(() => navigator.clipboard.readText());
    expect(link).toMatch(new RegExp(`/s/${FIX.needle}\\?cmd=4$`));
    await page.goto(link);
    await waitForPlayer(page);
    await expect.poll(() => playerTime(page)).toBeGreaterThanOrEqual(11.9);
    expect(await settledPlayerTime(page)).toBeLessThan(12.5);
    await expect(page.locator('.ap-term')).toContainText(FIX.evil);
    await expect(page.locator('.ap-term')).not.toContainText('false-command');
  });

  test('narrow screens stack the player under the results and fold the sidebar', async ({ page }) => {
    await page.setViewportSize({ width: 600, height: 900 });
    await page.goto('/?q=needle');
    await expect(page.getByTestId('session-tree')).toBeHidden();
    await page.getByRole('button', { name: 'Sessions' }).click();
    await expect(page.getByTestId('session-tree')).toBeVisible();
    await page.keyboard.press('Escape');
    await expect(page.getByTestId('session-tree')).toBeHidden();
    await page.getByTestId('results').getByRole('link').first().click();
    await waitForPlayer(page);
    const list = await page.getByTestId('results').boundingBox();
    const view = await page.getByTestId('session-view').boundingBox();
    expect(view!.y).toBeGreaterThanOrEqual(list!.y + list!.height - 1);
    expect(await page.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth)).toBe(true);
  });

  test('/link/<uuid> redirects to the session page', async ({ page }) => {
    await page.goto(`/link/${FIX.needle}`);
    await expect(page).toHaveURL(new RegExp(`/s/${FIX.needle}$`));
    await expect(page.getByTestId('session-view')).toBeVisible();
  });
});
