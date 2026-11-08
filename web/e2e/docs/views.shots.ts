// One screenshot per view and theme: docs/screenshots/<view>-<light|dark>.png. The project
// name is the theme; it is set both as the browser's colour scheme and as the stored
// preference, so the header toggle shows the theme explicitly.

import { test as base, expect, type Page } from '@playwright/test';
import { mkdirSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { DEMO } from './dataset.ts';

const OUT = fileURLToPath(new URL('../../../docs/screenshots/', import.meta.url));

const test = base.extend<{ theme: 'light' | 'dark' }>({
  /* eslint-disable no-empty-pattern -- Playwright fixtures take a destructured first argument */
  baseURL: async ({}, use) => use(process.env.CINELOG_DOCS_URL),
  storageState: async ({}, use) => use(process.env.CINELOG_DOCS_STATE),
  theme: async ({}, use, info) => use(info.project.name === 'dark' ? 'dark' : 'light'),
  /* eslint-enable no-empty-pattern */
  page: async ({ page, theme }, use) => {
    await page.addInitScript((t) => {
      try {
        localStorage.setItem('cinelog.theme', t);
      } catch {
        // Storage disabled: the colour scheme still selects the theme.
      }
    }, theme);
    await use(page);
  },
});

async function shoot(page: Page, view: string, theme: string): Promise<void> {
  await expect(page.locator('html')).toHaveAttribute('data-theme', theme);
  // The footer of a settled server, not one that is still indexing.
  const status = page.getByTestId('status');
  if (await status.count()) await expect(status).not.toContainText('indexing');
  // Let the player paint its first frame and fonts settle.
  await page.evaluate(() => document.fonts.ready);
  await page.waitForTimeout(400);
  mkdirSync(OUT, { recursive: true });
  await page.screenshot({ path: `${OUT}${view}-${theme}.png`, animations: 'disabled', caret: 'hide' });
}

async function playerReady(page: Page): Promise<void> {
  await expect(page.getByTestId('player')).toHaveAttribute('data-ready', 'true');
  await expect(page.locator('.ap-player')).toBeVisible();
}

/** Plays and pauses once, so the player drops its start overlay and shows a plain frame. */
async function paused(page: Page): Promise<void> {
  const overlay = page.locator('.ap-overlay-start');
  if (!(await overlay.count())) return;
  const body = page.locator('body');
  await body.press(' ');
  await expect(overlay).toHaveCount(0);
  await body.press(' ');
  const timer = page.locator('.ap-timer').first();
  await expect(async () => {
    const before = await timer.textContent();
    await page.waitForTimeout(1100);
    expect(await timer.textContent()).toBe(before);
  }).toPass();
}

/** Steps the player forward (`.`, 5 s) until the terminal shows `text`. */
async function stepUntil(page: Page, text: string): Promise<void> {
  const term = page.locator('.ap-term');
  await expect(async () => {
    if (!(await term.textContent())?.includes(text)) await page.locator('body').press('.');
    await expect(term).toContainText(text, { timeout: 200 });
  }).toPass({ intervals: [100] });
}

test.describe('signed out', () => {
  test.use({ storageState: { cookies: [], origins: [] } });

  test('login', async ({ page, theme }) => {
    await page.goto('/login');
    await expect(page.getByTestId('fingerprint')).not.toBeEmpty();
    await shoot(page, 'login', theme);
  });
});

test('search with the split player', async ({ page, theme }) => {
  await page.setViewportSize({ width: 1600, height: 900 });
  await page.goto(`/?q=${DEMO.query}&in=both`);
  await expect(page.getByTestId('result-count')).not.toContainText('…');
  const rows = page.getByTestId('results').getByRole('listitem');
  await expect(rows.first()).toBeVisible();
  await rows.filter({ hasText: 'Waiting for deployment' }).first().getByRole('link').first().click();
  await playerReady(page);
  await expect(page.getByTestId('current-command')).toContainText('rollout');
  // The hit is a line of output a few seconds after the start of its chunk.
  await stepUntil(page, 'Waiting for deployment');
  await paused(page);
  const viewer = page.locator('.viewer');
  for (const width of [1100, 1440, 1600]) {
    await page.setViewportSize({ width, height: 900 });
    await expect
      .poll(() => viewer.evaluate((v) => v.scrollWidth - v.clientWidth), { message: `the split panel is clipped at ${width} px` })
      .toBeLessThanOrEqual(0);
  }
  await shoot(page, 'search', theme);
});

test('session with chapters and annotations', async ({ page, theme }) => {
  await page.setViewportSize({ width: 1440, height: 1400 });
  await page.goto(`/s/${DEMO.deploy}?t=${DEMO.deployShotT}`);
  await playerReady(page);
  await expect(page.getByTestId('current-command')).toContainText('rollout');
  await expect(page.getByTestId('annotations-panel').getByTestId('annotation')).toHaveCount(2);
  await paused(page);
  await expect(page.locator('.ap-term')).toContainText('successfully rolled out');
  await shoot(page, 'session', theme);
});

test('team timeline', async ({ page, theme }) => {
  await page.setViewportSize({ width: 1440, height: 560 });
  await page.goto('/timeline');
  await expect(page.getByTestId('timeline-chart')).toBeVisible();
  await expect(page.getByTestId('tl-row').first()).toBeVisible();
  await expect(page.locator('[data-testid="tl-mark"][data-kind="pause"]').first()).toBeAttached();
  await shoot(page, 'timeline', theme);
});

test('live view', async ({ page, theme }) => {
  await page.goto(`/live/${DEMO.live}`);
  await expect(page.getByTestId('live-badge')).toHaveText(/live/i);
  await expect(page.getByTestId('live-view')).toHaveAttribute('data-stream', 'open');
  await expect(page.locator('.ap-term')).toContainText('running 48 tests');
  await shoot(page, 'live', theme);
});

test('keyboard shortcuts', async ({ page, theme }) => {
  await page.goto(`/s/${DEMO.deploy}?cmd=${DEMO.deploySeq}`);
  await playerReady(page);
  await paused(page);
  await page.locator('body').press('?');
  await expect(page.getByRole('dialog', { name: 'Keyboard shortcuts' })).toBeVisible();
  await shoot(page, 'shortcuts', theme);
});
