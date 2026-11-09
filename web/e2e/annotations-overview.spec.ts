import type { Page } from '@playwright/test';
import type { Annotation, AnnotationCreate, AnnotationPage, Me } from '../src/lib/api/types.ts';
import { expect, FIX, test, waitForPlayer } from './helpers.ts';

// The annotations overview (`/annotations`) and the search tag filter (docs/API.md §5.4,
// §9.4 "Overview", §9.6). Every spec tags its annotations with tags unique to its run, so it
// also works against a server that keeps earlier annotations. Specs that need a second
// author use the mock's control endpoint and skip against a real server.
// `CINELOG_E2E_SHOTS=<dir>` saves screenshots of the page there.

const unique = (what: string) => `${what}-${Date.now().toString(36)}${Math.floor(Math.random() * 1e4)}`;
const isMock = process.env.CINELOG_E2E_MOCK === '1';
const SHOTS = process.env.CINELOG_E2E_SHOTS;

async function shot(page: Page, name: string): Promise<void> {
  if (SHOTS) await page.screenshot({ path: `${SHOTS}/${name}.png`, fullPage: false });
}

async function principal(page: Page): Promise<string> {
  return ((await (await page.request.get('/api/me')).json()) as Me).principal;
}

/** Mutating requests need the page's origin (call after a `goto`). */
const origin = (page: Page) => new URL(page.url()).origin;

async function send(page: Page, method: string, path: string, data?: unknown) {
  return page.request.fetch(path, { method, data, headers: { Origin: origin(page), 'Content-Type': 'application/json' } });
}

async function create(page: Page, req: Omit<AnnotationCreate, 'session_uuid'> & { session_uuid?: string }): Promise<Annotation> {
  const res = await send(page, 'POST', '/api/annotations', { session_uuid: FIX.needle, ...req });
  expect(res.status()).toBe(201);
  return (await res.json()) as Annotation;
}

/** As another TEAM member (mock only). */
async function createAs(page: Page, author: string, req: Omit<AnnotationCreate, 'session_uuid'>): Promise<Annotation> {
  const res = await send(page, 'POST', '/api/_mock/annotations', { session_uuid: FIX.needle, author, ...req });
  expect(res.status()).toBe(201);
  return (await res.json()) as Annotation;
}

const rows = (page: Page) => page.getByTestId('anno-row');
const row = (page: Page, uuid: string) => page.locator(`[data-testid="anno-row"][data-uuid="${uuid}"]`);
const uuids = (page: Page) => rows(page).evaluateAll((els) => els.map((e) => e.getAttribute('data-uuid')));

/** Opens the overview and waits for the first page. */
async function open(page: Page, query = ''): Promise<void> {
  await page.goto(`/annotations${query}`);
  await expect(page.getByTestId('annotations-page')).toBeVisible();
  await expect(page.getByTestId('anno-count').or(page.getByTestId('anno-empty')).first()).toBeVisible();
}

test.describe('annotations overview', () => {
  test('A opens the overview from a session', async ({ page }) => {
    await page.goto(`/s/${FIX.needle}`);
    await waitForPlayer(page);
    await page.locator('body').click({ position: { x: 1, y: 1 } });
    await page.keyboard.press('Shift+A');
    await expect(page).toHaveURL(/\/annotations$/);
    await expect(page.getByTestId('nav-annotations')).toHaveAttribute('aria-current', 'page');
    await expect(page.getByTestId('annotations-page')).toBeVisible();
  });

  test('lists annotations newest first with author, tags and context; a row opens the session there', async ({ page }) => {
    await page.goto('/');
    const tag = unique('ov');
    const first = await create(page, { cmd_seq: 1, body: `first ${tag}`, tags: [tag] });
    const span = await create(page, { t_from: 4, t_to: 10, body: 'line one\nline two\nline three\nline four', tags: [tag, 'z-extra'] });
    const third = await create(page, { cmd_seq: 3, body: `${FIX.evil} third`, tags: [tag] });

    // The tab sits after Timeline.
    await page.getByTestId('nav-annotations').click();
    await expect(page).toHaveURL(/\/annotations$/);
    await expect(page.getByTestId('nav-annotations')).toHaveAttribute('aria-current', 'page');
    await page.goto(`/annotations?tag=${tag}`);
    await expect(rows(page)).toHaveCount(3);
    expect(await uuids(page)).toEqual([third.uuid, span.uuid, first.uuid]);
    await expect(page.getByTestId('anno-count')).toHaveText('3 annotations');

    const me = await principal(page);
    const t = row(page, third.uuid);
    await expect(t.getByTestId('anno-row-author')).toHaveText(me);
    await expect(t.getByTestId('anno-tag')).toHaveText([`#${tag}`]);
    await expect(t.getByTestId('anno-target')).toContainText('$cat evil.html');
    await expect(t.locator('.exit')).toHaveText('1');
    await expect(t.getByTestId('anno-open')).toContainText(`${FIX.host}`);
    // The body is text: the markup shows as written, nothing is rendered.
    await expect(t.getByTestId('anno-body')).toContainText(FIX.evil);
    await expect(t.locator('img')).toHaveCount(0);
    expect(await page.title()).not.toBe('pwned');

    const s = row(page, span.uuid);
    await expect(s.getByTestId('anno-target')).toContainText('span');
    await expect(s.getByTestId('anno-target')).toContainText(/\d\d:\d\d:\d\d – \d\d:\d\d:\d\d/);
    await expect(s.getByTestId('anno-tag')).toHaveText([`#${tag}`, '#z-extra']);
    // The first lines only.
    await expect(s.getByTestId('anno-body')).toContainText('line three');
    await expect(s.getByTestId('anno-body')).not.toContainText('line four');
    await shot(page, 'overview-desktop-light');

    // Times follow the UI time zone.
    const tz = page.getByTestId('tz-toggle');
    const created = await t.getByTestId('anno-created').textContent();
    const wasUtc = (await tz.getAttribute('aria-pressed')) === 'true';
    await tz.click();
    await expect(tz).toHaveAttribute('aria-pressed', String(!wasUtc));
    if (!wasUtc) await expect(t.getByTestId('anno-created')).toHaveText(new Date(third.created_at * 1000).toISOString().slice(0, 19).replace('T', ' '));
    await tz.click();
    await expect(t.getByTestId('anno-created')).toHaveText(created!);

    // A command annotation opens its session at the command.
    await t.getByTestId('anno-open').click();
    await expect(page).toHaveURL(`/s/${FIX.needle}?cmd=3`);
    await waitForPlayer(page);
    await expect(page.getByTestId('current-command')).toContainText('cat evil.html');
    // Back to the filtered list; a span opens at its start.
    await page.goBack();
    await expect(page).toHaveURL(new RegExp(`/annotations\\?tag=${tag}$`));
    await s.getByTestId('anno-open').click();
    await expect(page).toHaveURL(`/s/${FIX.needle}?t=4`);
    await waitForPlayer(page);
    await expect(page.getByTestId('current-command')).toContainText('grep -n needle');
  });

  test('filters by tag, author, text and time through the UI and the URL; history restores them', async ({ page }) => {
    await page.goto('/');
    const [a, b] = [unique('fa'), unique('fb')];
    const one = await create(page, { cmd_seq: 1, body: 'alpha one', tags: [a] });
    const two = await create(page, { cmd_seq: 2, body: 'beta TWO', tags: [a, b] });
    const bob = isMock ? await createAs(page, 'bob', { t_from: 1, t_to: 2, body: 'gamma three', tags: [a] }) : null;
    const all = bob ? 3 : 2;

    await open(page, `?tag=${a}`);
    await expect(rows(page)).toHaveCount(all);

    // A tag chip of a row adds its tag (a history entry).
    await row(page, two.uuid).getByTestId('anno-tag').filter({ hasText: `#${b}` }).click();
    await expect(page).toHaveURL(new RegExp(`tag=${a}%2C${b}$`));
    await expect(rows(page)).toHaveCount(1);
    await expect(page.getByTestId('anno-chips')).toContainText(`#${b}`);
    await page.goBack();
    await expect(page).toHaveURL(new RegExp(`tag=${a}$`));
    await expect(rows(page)).toHaveCount(all);
    await page.goForward();
    await expect(rows(page)).toHaveCount(1);
    // Removing the chip drops the tag again.
    await page.getByRole('button', { name: `Remove filter #${b}` }).click();
    await expect(rows(page)).toHaveCount(all);

    // The tag picker lists tags with counts.
    await page.getByTestId('anno-tag-picker').click();
    const picker = page.getByTestId('anno-tag-list');
    await picker.getByRole('searchbox').fill(b);
    const pick = picker.locator(`[data-testid="anno-pick-tag"][data-tag="${b}"]`);
    await expect(pick.locator('.n')).toHaveText('1');
    await pick.click();
    await expect(pick).toHaveAttribute('aria-pressed', 'true');
    await expect(rows(page)).toHaveCount(1);
    await page.keyboard.press('Escape');
    await expect(picker).toBeHidden();
    await expect(page.getByTestId('anno-tag-picker')).toBeFocused();
    await page.getByRole('button', { name: `Remove filter #${b}` }).click();
    await expect(rows(page)).toHaveCount(all);

    // Author.
    const me = await principal(page);
    if (bob) {
      await page.getByTestId('anno-author').selectOption('bob');
      await expect(page).toHaveURL(/author=bob/);
      await expect(rows(page)).toHaveCount(1);
      await expect(row(page, bob.uuid)).toBeVisible();
      await page.goBack();
      await expect(rows(page)).toHaveCount(all);
    }
    await page.getByTestId('anno-author').selectOption(me);
    await expect(rows(page)).toHaveCount(2);

    // Text: typing replaces the history entry; matches are marked.
    const before = page.url();
    await page.getByTestId('anno-q').fill('two');
    await expect(page).toHaveURL(/q=two/);
    await expect(rows(page)).toHaveCount(1);
    await expect(row(page, two.uuid).locator('mark')).toHaveText('TWO');
    await page.goBack();
    await expect(page).not.toHaveURL(/q=two/);
    expect(page.url()).not.toBe(before);
    await expect(page.getByTestId('anno-q')).toHaveValue('');
    await expect(rows(page)).toHaveCount(all);

    // Time: written today; a custom range in the past matches nothing and offers a reset.
    await page.getByTestId('anno-date').selectOption('today');
    await expect(page).toHaveURL(/date=today/);
    await expect(rows(page)).toHaveCount(all);
    await page.getByTestId('anno-date').selectOption('custom');
    await expect(page.getByTestId('anno-from')).toBeVisible();
    await page.getByTestId('anno-to').fill('2001-01-01T00:00');
    await expect(page).toHaveURL(/to=\d+/);
    await expect(page.getByTestId('anno-empty')).toContainText('No annotations match these filters.');
    await page.getByTestId('anno-reset').click();
    await expect(page).toHaveURL(/\/annotations$/);
    await expect(page.getByTestId('anno-chips')).toHaveCount(0);

    // All of it from a shared link.
    const now = Math.floor(Date.now() / 1000);
    await open(page, `?tag=${a}&q=ALPHA&author=${encodeURIComponent(me)}&date=custom&from=${now - 3600}&to=${now + 3600}`);
    expect(await uuids(page)).toEqual([one.uuid]);
    await expect(page.getByTestId('anno-q')).toHaveValue('ALPHA');
    await expect(page.getByTestId('anno-date')).toHaveValue('custom');
    await open(page, `?tag=${a}&date=custom&from=${now - 7200}&to=${now - 3600}`);
    await expect(page.getByTestId('anno-empty')).toBeVisible();
  });

  test('the header search keeps to itself on the overview', async ({ page }) => {
    await open(page, `?q=needle&tag=${unique('h')}`);
    await expect(page.getByTestId('search-input')).toHaveValue('');
    await expect(page.locator('.searchbar .chips')).toHaveCount(0);
    await page.getByTestId('search-input').fill('echo');
    await expect(page).toHaveURL(/\/annotations\?q=needle/);
    await page.getByTestId('search-input').press('Enter');
    await expect(page).toHaveURL(/\/\?q=echo$/);
    await expect(page.getByTestId('results')).toContainText('echo hello');
  });

  test('bus events add, change and remove rows without a reload', async ({ page }) => {
    await page.goto('/');
    const tag = unique('live');
    await open(page, `?tag=${tag}`);
    await expect(page.getByTestId('anno-empty')).toContainText('No annotations match');
    await page.evaluate(() => ((window as unknown as { marker: number }).marker = 1));

    const a = await create(page, { cmd_seq: 1, body: 'from elsewhere', tags: [tag] });
    await expect(row(page, a.uuid)).toBeVisible();
    await expect(row(page, a.uuid).getByTestId('anno-target')).toContainText('echo hello');
    const other = await create(page, { cmd_seq: 2, body: 'not matching yet', tags: [unique('other')] });
    if (isMock) {
      const b = await createAs(page, 'carol', { cmd_seq: 2, body: 'by carol', tags: [tag] });
      await expect(rows(page).first()).toHaveAttribute('data-uuid', b.uuid);
    }
    await expect(row(page, other.uuid)).toHaveCount(0);
    // Adding the tag makes it match: it shows up in its place.
    expect((await send(page, 'PATCH', `/api/annotations/${other.uuid}`, { add_tags: [tag] })).status()).toBe(200);
    await expect(row(page, other.uuid)).toBeVisible();
    // An edit shows in place; a delete removes the row.
    expect((await send(page, 'PATCH', `/api/annotations/${a.uuid}`, { body: 'edited elsewhere' })).status()).toBe(200);
    await expect(row(page, a.uuid).getByTestId('anno-body')).toHaveText('edited elsewhere');
    await expect(row(page, a.uuid)).toContainText('edited');
    expect((await send(page, 'DELETE', `/api/annotations/${a.uuid}`)).status()).toBe(204);
    await expect(row(page, a.uuid)).toHaveCount(0);
    // Without a reload.
    expect(await page.evaluate(() => (window as unknown as { marker?: number }).marker)).toBe(1);
  });

  test('keyboard: j/k move, Enter opens, o opens a new tab', async ({ page, context }) => {
    await page.goto('/');
    const tag = unique('keys');
    const older = await create(page, { cmd_seq: 1, body: 'older', tags: [tag] });
    const newer = await create(page, { cmd_seq: 4, body: 'newer', tags: [tag] });
    await open(page, `?tag=${tag}`);
    await expect(rows(page)).toHaveCount(2);
    await page.locator('h1').click();
    await page.keyboard.press('j');
    await expect(row(page, newer.uuid).getByTestId('anno-open')).toBeFocused();
    await expect(row(page, newer.uuid)).toHaveClass(/active/);
    await page.keyboard.press('j');
    await expect(row(page, older.uuid).getByTestId('anno-open')).toBeFocused();
    await page.keyboard.press('k');
    await expect(row(page, newer.uuid).getByTestId('anno-open')).toBeFocused();
    const popup = context.waitForEvent('page');
    await page.keyboard.press('o');
    const tab = await popup;
    await expect(tab).toHaveURL(new RegExp(`/s/${FIX.needle}\\?cmd=4$`));
    await tab.close();
    await page.keyboard.press('j');
    await page.keyboard.press('Enter');
    await expect(page).toHaveURL(`/s/${FIX.needle}?cmd=1`);
  });

  test('pages follow `next` on scroll, also past short pages', async ({ page }) => {
    await page.goto('/');
    const tag = unique('many');
    for (let i = 0; i < 56; i += 1) await create(page, { cmd_seq: 1 + (i % 4), body: `note ${i}`, tags: [tag] });
    // The first page comes back empty (rows dropped after the limit) but with a cursor.
    let emptied = false;
    await page.route('**/api/annotations?*', async (route) => {
      const url = new URL(route.request().url());
      if (emptied || url.searchParams.has('cursor') || url.searchParams.has('uuid')) return route.fallback();
      emptied = true;
      const res = await route.fetch();
      const body = (await res.json()) as AnnotationPage;
      await route.fulfill({ response: res, json: { ...body, annotations: [], contexts: {} } });
    });
    await open(page, `?tag=${tag}`);
    // The second page arrives without scrolling; then scrolling loads the rest.
    await expect(rows(page)).toHaveCount(6);
    await expect(rows(page).first()).toContainText('note 5');
    await page.unroute('**/api/annotations?*');
    await page.reload();
    await expect(rows(page)).toHaveCount(50);
    await expect(page.getByTestId('anno-count')).toHaveText('50+ annotations');
    await rows(page).last().scrollIntoViewIfNeeded();
    await expect(rows(page)).toHaveCount(56);
    await expect(page.getByTestId('anno-count')).toHaveText('56 annotations');
    await expect(rows(page).last()).toContainText('note 0');
  });

  test('empty and error states', async ({ page }) => {
    let fail = true;
    await page.route('**/api/annotations?*', async (route) => {
      if (fail) return route.fulfill({ status: 500, json: { error: 'internal', message: 'disk on fire' } });
      return route.fulfill({ json: { annotations: [], next: null, contexts: {} } satisfies AnnotationPage });
    });
    await page.goto('/annotations');
    await expect(page.getByTestId('anno-error')).toContainText('disk on fire');
    fail = false;
    await page.getByTestId('anno-retry').click();
    await expect(page.getByTestId('anno-error')).toHaveCount(0);
    await expect(page.getByTestId('anno-empty')).toContainText('No annotations yet.');
    await expect(page.getByTestId('anno-reset')).toHaveCount(0);
  });
});

test.describe('search tag filter', () => {
  test('narrows results to annotated commands and spans; result chips add their tag', async ({ page }) => {
    await page.goto('/');
    // Rows show the first two tags of a command: `0…` sorts before the other specs' tags.
    const [tag, spanTag] = [unique('0st'), unique('sp')];
    await create(page, { cmd_seq: 2, body: 'the grep', tags: [tag] });
    await create(page, { t_from: 0.5, t_to: 1.5, body: 'over echo hello', tags: [spanTag] });

    await page.goto('/?q=needle&in=both');
    await expect(page.getByTestId('result-count')).not.toContainText('…');
    const chip = page.getByTestId('hit-tag').filter({ hasText: `#${tag}` }).first();
    await expect(chip).toBeVisible();
    await shot(page, 'search-tags-desktop-light');
    await chip.click();
    await expect(page).toHaveURL(new RegExp(`tag=${tag}`));
    await expect(page.getByTestId('search-tag-chip')).toHaveText(new RegExp(`#${tag}`));
    // The command and its output, both of the annotated command (one group).
    await expect(page.getByTestId('result-count')).toHaveText('2 hits');
    await expect(page.getByTestId('results').locator('[data-hit]')).toHaveCount(1);
    await expect(page.getByTestId('results')).toContainText('grep -n needle haystack.txt');

    // A span annotation counts for the commands it overlaps; set through the filter field.
    await page.getByTestId('search-input').fill('');
    await page.getByRole('button', { name: `Remove filter #${tag}` }).click();
    await page.getByRole('button', { name: 'Filters' }).click();
    await page.getByTestId('search-tag-input').fill(spanTag.toUpperCase());
    await page.getByTestId('search-tag-input').press('Enter');
    await expect(page).toHaveURL(new RegExp(`tag=${spanTag}`));
    await expect(page.getByTestId('result-count')).toHaveText(/^1 recent commands?$/);
    await expect(page.getByTestId('results')).toContainText('echo hello');

    // An unknown tag finds nothing (not an error); an invalid one is refused in the field.
    await page.goto(`/?tag=${unique('none')}`);
    await expect(page.getByTestId('no-results')).toContainText('No commands annotated');
    await page.getByRole('button', { name: 'Filters' }).click();
    await page.getByTestId('search-tag-input').fill('no spaces<>');
    await page.getByTestId('search-tag-input').press('Enter');
    await expect(page.getByRole('alert')).toContainText('is not a tag');
  });
});

for (const [width, height] of [
  [390, 844],
  [904, 1000],
] as const) {
  for (const colorScheme of ['light', 'dark'] as const) {
    test.describe(`overview touch ${width}x${height} ${colorScheme}`, () => {
      test.use({ viewport: { width, height }, hasTouch: true, isMobile: true, colorScheme });

      test('lays out without overlaps; the drawer and the tag picker fit', async ({ page }) => {
        await page.goto('/');
        const tag = unique('m');
        await create(page, { cmd_seq: 1, body: 'a fairly long note that wraps on a phone screen, with more words than fit on one line', tags: [tag, 'ops'] });
        await create(page, { t_from: 4, t_to: 10, body: 'span note', tags: [tag] });
        await open(page, `?tag=${tag}&author=${encodeURIComponent(await principal(page))}`);
        await expect(rows(page)).toHaveCount(2);
        await expect(page.getByTestId('nav-annotations')).toBeVisible();
        await expect(page.getByTestId('nav-annotations')).toHaveAttribute('aria-current', 'page');

        expect(await page.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth)).toBe(true);
        const boxes = await page.evaluate(() =>
          Array.from(
            document.querySelectorAll<HTMLElement>(
              '.top button, .top a, .top input, .overview .filters button, .overview .filters input, .overview .filters select, .overview .chips li, [data-testid="anno-row"]',
            ),
          )
            .filter((el) => el.checkVisibility({ visibilityProperty: true }) && el.getClientRects().length > 0)
            .map((el) => {
              const r = el.getBoundingClientRect();
              return { x: r.x, y: r.y, w: r.width, h: r.height, name: el.dataset.testid ?? el.getAttribute('aria-label') ?? el.textContent?.trim().slice(0, 20) ?? el.tagName, control: el.matches('button, input, select') };
            }),
        );
        for (const b of boxes) {
          expect(b.x, `${b.name} clipped left`).toBeGreaterThanOrEqual(0);
          expect(b.x + b.w, `${b.name} clipped right`).toBeLessThanOrEqual(width + 0.5);
          if (b.control) expect(b.h, `${b.name} hit target`).toBeGreaterThanOrEqual(30);
        }
        for (let i = 0; i < boxes.length; i += 1) {
          for (let j = i + 1; j < boxes.length; j += 1) {
            const [p, q] = [boxes[i]!, boxes[j]!];
            const ow = Math.min(p.x + p.w, q.x + q.w) - Math.max(p.x, q.x);
            const oh = Math.min(p.y + p.h, q.y + q.h) - Math.max(p.y, q.y);
            expect(ow <= 1.01 || oh <= 1.01, `${p.name} overlaps ${q.name}`).toBe(true);
          }
        }
        // Row tag chips are tappable.
        const chip = rows(page).first().getByTestId('anno-tag').first();
        expect((await chip.boundingBox())!.height).toBeGreaterThanOrEqual(30);
        await shot(page, `overview-${width}-${colorScheme}`);

        await page.getByTestId('anno-tag-picker').tap();
        const picker = page.getByTestId('anno-tag-list');
        await expect(picker).toBeVisible();
        const pb = (await picker.boundingBox())!;
        expect(pb.x).toBeGreaterThanOrEqual(0);
        expect(pb.x + pb.width).toBeLessThanOrEqual(width + 0.5);
        // A long tag list scrolls inside the picker.
        const last = (await picker.locator('li').last().boundingBox())!;
        expect(last.y + last.height <= pb.y + pb.height + 0.5 || (await picker.locator('ul').evaluate((ul) => ul.scrollHeight > ul.clientHeight))).toBe(true);
        expect(await picker.locator('ul').evaluate((ul) => ul.getBoundingClientRect().bottom)).toBeLessThanOrEqual(pb.y + pb.height + 0.5);
        await shot(page, `overview-${width}-${colorScheme}-picker`);
        // A tap elsewhere closes it.
        await page.locator('h1').tap();
        await expect(picker).toBeHidden();

        await page.getByRole('button', { name: 'Sessions' }).tap();
        const drawer = page.getByRole('dialog', { name: 'Sessions' });
        await expect(drawer).toBeVisible();
        await expect.poll(() => drawer.evaluate((el) => getComputedStyle(el).transform)).toMatch(/^(none|matrix\(1, 0, 0, 1, 0, 0\))$/);
        const side = (await drawer.boundingBox())!;
        const firstRow = (await rows(page).first().boundingBox())!;
        expect(await page.evaluate(([x, y]) => !!document.elementFromPoint(x!, y!)?.closest('#sidebar'), [Math.min(side.width - 8, firstRow.x + 8), firstRow.y + 8])).toBe(true);
        await page.keyboard.press('Escape');
        await expect(drawer).toBeHidden();

        // A tap on a row opens the session.
        await rows(page).first().getByTestId('anno-target').tap();
        await expect(page).toHaveURL(new RegExp(`/s/${FIX.needle}\\?t=4$`));
      });
    });
  }
}

test('dark theme on the desktop', async ({ page }) => {
  await page.emulateMedia({ colorScheme: 'dark' });
  await page.goto('/');
  const tag = unique('dk');
  await create(page, { cmd_seq: 3, body: 'dark mode note\nsecond line', tags: [tag, 'ops'] });
  await create(page, { t_from: 4, t_to: 10, body: 'a span', tags: [tag] });
  await open(page, `?tag=${tag}`);
  await expect(rows(page)).toHaveCount(2);
  const bg = await page.evaluate(() => getComputedStyle(document.querySelector('[data-testid="anno-row"]')!).backgroundColor);
  expect(bg).toBe('rgb(29, 32, 36)');
  await shot(page, 'overview-desktop-dark');
  await page.goto(`/?q=evil&tag=${tag}`);
  await expect(page.getByTestId('result-count')).not.toContainText('…');
  await shot(page, 'search-tags-desktop-dark');
});
