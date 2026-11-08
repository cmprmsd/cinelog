import { test as base, expect, type Page } from '@playwright/test';

export { FIX } from './mock/fixtures.ts';

/** Every spec runs against the server the global setup chose. */
export const test = base.extend<{ cspViolations: string[] }>({
  // eslint-disable-next-line no-empty-pattern -- Playwright fixtures take a destructured first argument
  baseURL: async ({}, use) => use(process.env.CINELOG_E2E_URL),
  // Fails a test when the page reports a CSP violation (the player must work under the
  // server's policy, plan §7.3 verify item).
  cspViolations: [
    async ({ page }, use) => {
      const seen: string[] = [];
      page.on('console', (m) => {
        if (/Content Security Policy|Refused to/i.test(m.text())) seen.push(m.text());
      });
      await use(seen);
      expect(seen, 'CSP violations').toEqual([]);
    },
    { auto: true },
  ],
});

export { expect };

export const token = () => process.env.CINELOG_E2E_TOKEN ?? '';

/** Searches from the top bar and waits for the result list to settle. */
export async function search(page: Page, q: string, mode: 'cmd' | 'out' | 'both' = 'cmd'): Promise<void> {
  if (!page.url().includes('/')) await page.goto('/');
  await page.getByRole('radio', { name: mode, exact: true }).click();
  await page.getByTestId('search-input').fill(q);
  await expect(page).toHaveURL(new RegExp(`[?&]q=${encodeURIComponent(q).replace(/%20/g, '\\+')}`));
  await expect(page.getByTestId('result-count')).not.toContainText('…');
}

/** The time the player itself reports (seconds), not the position the app asked for. */
export async function playerTime(page: Page): Promise<number> {
  return Number(await page.getByTestId('player').getAttribute('data-player-time'));
}

/** Waits until the player has caught up with the last seek and returns its time. */
export async function settledPlayerTime(page: Page): Promise<number> {
  const player = page.getByTestId('player');
  let reported = NaN;
  await expect
    .poll(async () => {
      reported = Number(await player.getAttribute('data-player-time'));
      const requested = Number(await player.getAttribute('data-current-time'));
      return Math.abs(reported - requested) < 0.05;
    })
    .toBe(true);
  return reported;
}

/** The terminal's visible text. */
export async function terminalText(page: Page): Promise<string> {
  return page.locator('.ap-term').innerText();
}

export async function waitForPlayer(page: Page): Promise<void> {
  await expect(page.getByTestId('player')).toHaveAttribute('data-ready', 'true');
  await expect(page.locator('.ap-player')).toBeVisible();
}
