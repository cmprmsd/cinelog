import { expect, FIX, search, test } from './helpers.ts';

test.describe('search', () => {
  test.beforeEach(async ({ page }) => {
    await page.goto('/');
  });

  test('highlights command hits with <mark>', async ({ page }) => {
    await search(page, 'needle');
    const rows = page.getByTestId('results').getByRole('listitem');
    await expect(rows.first()).toBeVisible();
    const marks = page.getByTestId('results').locator('mark');
    await expect(marks.first()).toHaveText('needle');
    await expect(rows.filter({ hasText: 'grep -n needle haystack.txt' })).toHaveCount(1);
  });

  test('finds output and renders markup as text', async ({ page }) => {
    await search(page, 'onerror', 'out');
    const row = page.getByTestId('results').getByRole('listitem').first();
    await expect(row).toContainText(FIX.evil);
    await expect(row.locator('mark')).toHaveText('onerror');
    await expect(page.getByTestId('results').locator('img')).toHaveCount(0);
    expect(await page.title()).toBe('cinelog');
  });

  test('groups hits of one command', async ({ page }) => {
    await search(page, 'needle', 'both');
    const head = page.getByTestId('results').getByRole('listitem').filter({ hasText: 'grep -n needle' }).first();
    const more = head.getByRole('button', { name: /1 more hit/ });
    await expect(more).toBeVisible();
    await more.click();
    await expect(page.getByTestId('results').getByRole('listitem').filter({ hasText: 'a needle in the haystack' })).toHaveCount(1);
  });

  test('keeps filters in the URL', async ({ page }) => {
    await search(page, 'e');
    // The mock always flags a one-letter scan; a real server only when the scan hit its cap.
    if (process.env.CINELOG_E2E_MOCK) await expect(page.getByTestId('partial')).toBeVisible();
    await page.getByRole('button', { name: 'Filters' }).click();
    await page.getByLabel('exit').selectOption('fail');
    await expect(page).toHaveURL(/exit=fail/);
    await expect(page.getByRole('list', { name: 'Active filters' })).toContainText('exit ≠0');
    await page.reload();
    await expect(page.getByTestId('search-input')).toHaveValue('e');
    await expect(page.getByRole('list', { name: 'Active filters' })).toContainText('exit ≠0');
    const rows = page.getByTestId('results').getByRole('listitem');
    await expect(rows.filter({ hasText: 'cat evil.html' })).toHaveCount(1);
    await expect(rows.filter({ hasText: 'echo hello' })).toHaveCount(0);
    const count = page.getByTestId('result-count');
    await expect(count).not.toContainText('…');
    const failed = (await count.textContent())!;
    await page.getByRole('button', { name: 'Remove filter exit ≠0' }).click();
    await expect(page).not.toHaveURL(/exit=/);
    // The list is virtualised and `echo hello` may sit below the rendered rows: compare counts.
    await expect(count).not.toContainText('…');
    await expect(count).not.toHaveText(failed);
    await expect(count).toHaveText(/^\d+\+? hits$/);
  });

  test('marks legacy and approximate hits', async ({ page }) => {
    await search(page, 'legacy-target');
    const row = page.getByTestId('results').getByRole('listitem').first();
    await expect(row.locator('.badge.approx')).toHaveText('approx');
    await expect(row.locator('.badge.legacy')).toBeVisible();
  });

  test('a failing next page is requested once, then again only on retry', async ({ page }) => {
    let cursorCalls = 0;
    let fail = true;
    await page.route('**/api/search?*', async (route) => {
      const url = new URL(route.request().url());
      // The page that beforeEach opened may still be loading its own (empty) search.
      if (url.searchParams.has('session') || url.searchParams.get('q') !== 'needle') return route.continue();
      if (url.searchParams.get('cursor')) {
        cursorCalls += 1;
        if (fail) return route.fulfill({ status: 503, json: { error: 'unavailable', message: 'index rebuild in progress' } });
        return route.fulfill({ json: { hits: [], next: null, partial: false, capped: false } });
      }
      const r = await (await route.fetch()).json();
      const hits = Array.from({ length: 5 }, (_, i) => ({ ...r.hits[0], id: `c:paged:${i}`, cmd_seq: 1000 + i }));
      return route.fulfill({ json: { hits, next: 'p2', partial: false, capped: false } });
    });
    await page.goto('/?q=needle');
    await expect(page.getByRole('status').filter({ hasText: 'index rebuild in progress' })).toBeVisible();
    await page.waitForTimeout(1500);
    expect(cursorCalls).toBe(1);
    // Scrolling the failed list does not retry on its own either.
    await page.getByTestId('results').hover();
    await page.mouse.wheel(0, 400);
    await page.waitForTimeout(300);
    expect(cursorCalls).toBe(1);
    fail = false;
    await page.getByTestId('retry-more').click();
    await expect(page.getByTestId('result-count')).toHaveText('5 hits');
    expect(cursorCalls).toBe(2);
  });
});
