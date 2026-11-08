import type { Page } from '@playwright/test';
import { expect, FIX, test, waitForPlayer } from './helpers.ts';

// Phones and touch tablets: the sidebar is a drawer over everything (player controls
// included), it closes by tap outside, swipe, Esc or navigation, and the header neither
// overlaps nor clips its controls.

/** A one-finger drag through the browser's real input pipeline (touch and pointer events). */
async function drag(
  page: Page,
  from: { x: number; y: number },
  to: { x: number; y: number },
  midway?: () => Promise<void>,
): Promise<void> {
  const steps = 8;
  const cdp = await page.context().newCDPSession(page);
  await cdp.send('Input.dispatchTouchEvent', { type: 'touchStart', touchPoints: [from] });
  for (let i = 1; i <= steps; i += 1) {
    const point = { x: from.x + ((to.x - from.x) * i) / steps, y: from.y + ((to.y - from.y) * i) / steps };
    await cdp.send('Input.dispatchTouchEvent', { type: 'touchMove', touchPoints: [point] });
    if (i === steps / 2) await midway?.();
  }
  await cdp.send('Input.dispatchTouchEvent', { type: 'touchEnd', touchPoints: [] });
  await cdp.detach();
}

/** The drawer's horizontal offset (px) from its transform. */
const drawerShift = (page: Page) => page.locator('#sidebar').evaluate((el) => new DOMMatrix(getComputedStyle(el).transform).m41);

type Box = { x: number; y: number; width: number; height: number; name: string; control: boolean };

/** The header's visible controls, as laid out. */
async function headerControls(page: Page): Promise<Box[]> {
  return page.evaluate(() =>
    Array.from(document.querySelectorAll<HTMLElement>('.top button, .top a, .top input, .top .who'))
      .filter((el) => el.checkVisibility({ visibilityProperty: true }) && el.getClientRects().length > 0)
      .map((el) => {
        const r = el.getBoundingClientRect();
        const name = el.getAttribute('aria-label') ?? (el.textContent?.trim() || el.tagName);
        return { x: r.x, y: r.y, width: r.width, height: r.height, name, control: el.matches('button, a, input') };
      }),
  );
}

/** Every pair of controls is apart (a segmented control shares its 1 px borders) and on screen. */
function expectLaidOut(boxes: Box[], viewport: number, touch: boolean): void {
  expect(boxes.length).toBeGreaterThan(3);
  for (const b of boxes) {
    expect(b.x, `${b.name} clipped on the left`).toBeGreaterThanOrEqual(0);
    expect(b.x + b.width, `${b.name} clipped on the right`).toBeLessThanOrEqual(viewport + 0.5);
    if (touch && b.control) expect(b.height, `${b.name} hit target`).toBeGreaterThanOrEqual(40);
  }
  for (let i = 0; i < boxes.length; i += 1) {
    for (let j = i + 1; j < boxes.length; j += 1) {
      const a = boxes[i]!;
      const b = boxes[j]!;
      const w = Math.min(a.x + a.width, b.x + b.width) - Math.max(a.x, b.x);
      const h = Math.min(a.y + a.height, b.y + b.height) - Math.max(a.y, b.y);
      expect(w <= 1.01 || h <= 1.01, `${a.name} overlaps ${b.name}`).toBe(true);
    }
  }
}

const drawer = (page: Page) => page.getByRole('dialog', { name: 'Sessions' });
const toggle = (page: Page) => page.getByRole('button', { name: 'Sessions' });

async function openSession(page: Page): Promise<void> {
  await page.goto(`/s/${FIX.needle}`);
  await waitForPlayer(page);
}

async function openDrawer(page: Page): Promise<void> {
  await toggle(page).tap();
  await expect(drawer(page)).toBeVisible();
  await expect(page.getByTestId('session-tree')).toBeVisible();
  // Settled: the slide-in transition is over.
  await expect.poll(() => drawer(page).evaluate((el) => getComputedStyle(el).transform)).toMatch(/^(none|matrix\(1, 0, 0, 1, 0, 0\))$/);
}

for (const [width, height] of [
  [390, 844],
  [904, 1000],
] as const) {
  for (const colorScheme of ['dark', 'light'] as const) {
    test.describe(`touch ${width}x${height} ${colorScheme}`, () => {
      test.use({ viewport: { width, height }, hasTouch: true, isMobile: true, colorScheme });

      test('a tap on the backdrop closes the drawer', async ({ page }) => {
        await openSession(page);
        await expect(page.getByTestId('session-tree')).toBeHidden();
        await openDrawer(page);
        await page.touchscreen.tap(width - 12, height / 2);
        await expect(drawer(page)).toBeHidden();
        await expect(page.getByTestId('drawer-scrim')).toHaveCount(0);
        await expect(toggle(page)).toBeFocused();
        // The page behind is usable again.
        await expect(page.locator('main')).not.toHaveAttribute('inert');
      });

      test('swiping left closes the drawer; a vertical swipe does not', async ({ page }) => {
        await openSession(page);
        await openDrawer(page);
        const box = (await page.locator('#sidebar').boundingBox())!;
        await drag(page, { x: box.width / 2, y: 300 }, { x: box.width / 2 + 4, y: 120 });
        await expect(drawer(page)).toBeVisible();
        // The drawer follows the finger, then settles closed.
        await drag(page, { x: box.width * 0.8, y: 300 }, { x: 10, y: 310 }, async () => {
          const shift = await drawerShift(page);
          expect(shift).toBeLessThan(-20);
          expect(shift).toBeGreaterThan(-box.width);
        });
        await expect(drawer(page)).toBeHidden();
      });

      test('an edge swipe opens the drawer, Esc closes it', async ({ page }) => {
        await openSession(page);
        await drag(page, { x: 4, y: height / 2 }, { x: Math.min(300, width * 0.7), y: height / 2 + 5 });
        await expect(drawer(page)).toBeVisible();
        await expect(drawer(page)).toBeFocused();
        await page.keyboard.press('Escape');
        await expect(drawer(page)).toBeHidden();
      });

      test('picking a session closes the drawer', async ({ page }) => {
        await openSession(page);
        await openDrawer(page);
        await page.getByTestId('live-list').getByRole('link').first().tap();
        await expect(page).toHaveURL(new RegExp(`/live/${FIX.live}$`));
        await expect(drawer(page)).toBeHidden();
      });

      test('nothing from the player shows through the open drawer', async ({ page }) => {
        await openSession(page);
        await openDrawer(page);
        const side = (await page.locator('#sidebar').boundingBox())!;
        const targets = ['.ap-control-bar', '.ap-playback-button', '[data-testid="session-view"] .head', '.chap', '[data-testid="session-view"] .rail'];
        let checked = 0;
        for (const sel of targets) {
          const el = page.locator(sel).first();
          if (!(await el.count())) continue;
          const b = await el.boundingBox();
          if (!b || b.height === 0) continue;
          const y = b.y + b.height / 2;
          if (y < 0 || y > height) continue;
          const hit = (x: number) =>
            page.evaluate(([px, py]) => {
              const e = document.elementFromPoint(px!, py!);
              return e?.closest('#sidebar') ? 'drawer' : e?.closest('[data-testid="drawer-scrim"]') ? 'scrim' : (e?.className.toString() ?? 'none');
            }, [x, y]);
          // Under the drawer and beside it.
          expect(await hit(Math.min(side.x + side.width - 8, b.x + 8)), `${sel} over the drawer`).toBe('drawer');
          if (side.width < width - 16) expect(await hit(width - 8), `${sel} over the scrim`).toBe('scrim');
          checked += 1;
        }
        expect(checked).toBeGreaterThanOrEqual(3);
        // The screenshot case: the control bar's own z-index does not lift it above the drawer.
        const bar = (await page.locator('.ap-control-bar').boundingBox())!;
        expect(await page.evaluate(([x, y]) => !!document.elementFromPoint(x!, y!)?.closest('#sidebar'), [20, bar.y + bar.height / 2])).toBe(true);
      });

      test('header controls do not overlap and are touch-sized', async ({ page }) => {
        await page.goto('/?q=needle&in=both');
        await expect(page.getByTestId('result-count')).not.toContainText('…');
        expectLaidOut(await headerControls(page), width, true);
        const more = page.getByTestId('more-toggle');
        if (await more.isVisible()) {
          // The compact header folds the tools into a menu.
          await more.tap();
          await expect(page.getByTestId('theme-toggle')).toBeVisible();
          expectLaidOut(await headerControls(page), width, true);
          await page.getByTestId('search-input').tap();
          await expect(page.getByTestId('theme-toggle')).toBeHidden();
          // Esc closes it too, before the views behind see the key.
          await more.tap();
          await expect(page.getByTestId('theme-toggle')).toBeVisible();
          await page.keyboard.press('Escape');
          await expect(page.getByTestId('theme-toggle')).toBeHidden();
          await expect(more).toBeFocused();
        }
        expect(await page.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth)).toBe(true);
      });
    });
  }
}

test('reduced motion: the drawer opens and closes without sliding', async ({ page }) => {
  await page.emulateMedia({ reducedMotion: 'reduce' });
  await page.setViewportSize({ width: 600, height: 900 });
  await page.goto('/');
  await page.getByRole('button', { name: 'Sessions' }).click();
  await expect(drawer(page)).toBeVisible();
  expect(await drawer(page).evaluate((el) => getComputedStyle(el).transitionDuration)).toBe('0s');
  await page.getByTestId('drawer-scrim').click({ position: { x: 580, y: 450 } });
  await expect(drawer(page)).toBeHidden();
});

test('the header lays out without overlaps from 320 px to desktop', async ({ page }) => {
  await page.goto('/?q=needle&in=both&cwd=/srv');
  await expect(page.getByTestId('result-count')).not.toContainText('…');
  for (const width of [320, 360, 480, 640, 760, 761, 820, 900, 960, 1024, 1100, 1180, 1280, 1600]) {
    await page.setViewportSize({ width, height: 800 });
    expectLaidOut(await headerControls(page), width, width <= 760);
  }
});

test('the cwd filter says it covers the directory and below', async ({ page }) => {
  await page.goto('/?q=needle&in=both');
  await page.getByRole('button', { name: 'Filters' }).click();
  await expect(page.getByPlaceholder('/dir (and below)')).toBeVisible();
});
