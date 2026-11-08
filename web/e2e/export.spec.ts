import type { Download, Page } from '@playwright/test';
import { expect, FIX, test, waitForPlayer } from './helpers.ts';

// Export menu (plan §8.4, §12.3; docs/API.md §7, §9.5; M4 Accept 4): the .cast slice plays on
// its own, the text export holds the rendered output, the HTML export is escaped and has no
// script. Runs against the mock and a real server alike.

async function body(d: Download): Promise<string> {
  return Buffer.concat(await (await d.createReadStream()).toArray()).toString('utf8');
}

async function exportVia(page: Page, entry: string): Promise<Download> {
  const menu = page.getByTestId('export-menu');
  if (!(await menu.evaluate((el) => (el as HTMLDetailsElement).open))) await menu.locator('summary').click();
  const [d] = await Promise.all([page.waitForEvent('download'), page.getByRole('menuitem', { name: entry, exact: true }).click()]);
  return d;
}

async function openCommand(page: Page, seq: number, typed: string): Promise<void> {
  await page.goto(`/s/${FIX.needle}?cmd=${seq}`);
  await waitForPlayer(page);
  await expect(page.getByTestId('current-command')).toContainText(typed);
}

test.describe('export', () => {
  test('e opens the menu; the keyboard picks the command slice; Esc closes', async ({ page }) => {
    await openCommand(page, 2, 'grep -n needle');
    await page.locator('body').click({ position: { x: 1, y: 1 } });
    await page.keyboard.press('e');
    const first = page.getByRole('menuitem', { name: 'Current command as .cast', exact: true });
    await expect(first).toBeFocused();
    await page.keyboard.press('ArrowRight');
    await expect(page.getByRole('menuitem', { name: 'Current command as .txt', exact: true })).toBeFocused();
    await page.keyboard.press('ArrowLeft');
    await expect(first).toBeFocused();
    const [d] = await Promise.all([page.waitForEvent('download'), page.keyboard.press('Enter')]);
    expect(d.suggestedFilename()).toMatch(/^cinelog-0b9e4c1a-.*\.cast$/);
    await expect(page.getByRole('status').filter({ hasText: 'Downloaded' })).toBeVisible();

    await page.keyboard.press('e');
    await expect(first).toBeFocused();
    await page.keyboard.press('Escape');
    await expect(page.getByTestId('export-menu')).not.toHaveAttribute('open');
    await expect(page.getByTestId('export-menu').locator('summary')).toBeFocused();
  });

  test('the .cast slice of a command is standalone and plays in asciinema-player', async ({ page }) => {
    await openCommand(page, 2, 'grep -n needle');
    const cast = await body(await exportVia(page, 'Current command as .cast'));
    const [head, ...lines] = cast.trimEnd().split('\n');
    const header = JSON.parse(head!) as { version: number; term: { cols: number; rows: number } };
    expect(header.version).toBe(3);
    expect(header.term.cols).toBeGreaterThan(0);
    const events = lines.map((l) => JSON.parse(l) as [number, string, string]);
    // Starts with the screen at the slice start, at t=0.
    expect(events[0]![0]).toBe(0);
    expect(events[0]![1]).toBe('o');
    expect(events.every(([dt, code]) => dt >= 0 && ['o', 'i', 'r', 'm', 'x'].includes(code))).toBe(true);
    expect(cast).toContain('a needle in the haystack');

    // Served as another session's recording, the slice plays on its own.
    await page.route(`**/api/sessions/${FIX.forget}/cast`, (r) => r.fulfill({ status: 200, contentType: 'application/x-asciicast', body: cast }));
    await page.goto(`/s/${FIX.forget}?t=1.2`);
    await waitForPlayer(page);
    await expect(page.locator('.ap-term')).toContainText('a needle in the haystack');
  });

  test('the whole session as text holds the output; as HTML it is escaped and script-free', async ({ page }) => {
    await openCommand(page, 3, 'cat evil.html');
    const txtDl = await exportVia(page, 'Whole session as .txt');
    expect(txtDl.suggestedFilename()).toMatch(/^cinelog-0b9e4c1a-.*\.txt$/);
    const txt = await body(txtDl);
    expect(txt).toContain('a needle in the haystack');
    expect(txt).toContain('false-command');
    expect(txt).toContain('onerror');

    const html = await body(await exportVia(page, 'Whole session as .html'));
    expect(html).toMatch(/<meta http-equiv="Content-Security-Policy" content="default-src 'none'; style-src 'unsafe-inline'">/);
    expect(html).not.toMatch(/<script/i);
    expect(html).not.toContain('<img');
    expect(html).toMatch(/&lt;img|&#60;img/);
    expect(html).toContain('needle');
  });

  test('a failed export shows the server message and leaves the page usable', async ({ page }) => {
    await page.route('**/api/sessions/*/export?*', (r) => r.fulfill({ status: 503, json: { error: 'unavailable', message: 'export is busy' } }));
    await openCommand(page, 2, 'grep -n needle');
    const menu = page.getByTestId('export-menu');
    await menu.locator('summary').click();
    await page.getByRole('menuitem', { name: 'Current command as .txt', exact: true }).click();
    await expect(page.getByRole('status').filter({ hasText: 'Export failed: export is busy' })).toBeVisible();
    await expect(menu).not.toHaveAttribute('open');
    await page.unroute('**/api/sessions/*/export?*');
    const d = await exportVia(page, 'Current command as .txt');
    expect(await body(d)).toContain('a needle in the haystack');
  });

  test('without a command at the playhead only the session entries are enabled', async ({ page }) => {
    await page.goto(`/s/${FIX.needle}?t=0`);
    await waitForPlayer(page);
    await page.getByTestId('export-menu').locator('summary').click();
    await expect(page.getByRole('menuitem', { name: 'Current command as .txt', exact: true })).toBeDisabled();
    await expect(page.getByRole('menuitem', { name: 'Whole session as .txt', exact: true })).toBeEnabled();
    await page.locator('body').click({ position: { x: 1, y: 1 } });
    await expect(page.getByTestId('export-menu')).not.toHaveAttribute('open');
  });
});
