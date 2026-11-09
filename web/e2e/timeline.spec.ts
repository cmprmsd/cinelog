import type { Locator, Page } from '@playwright/test';
import type { Timeline, TimelineBlock, TimelineLane } from '../src/lib/api/types.ts';
import { expect, FIX, test, waitForPlayer } from './helpers.ts';

// Team timeline (M4-B; plan §8.3, §13 M4 Accept 2). The fixture sessions all belong to
// alice; the needle session (tmux %1) carries a pause and a nested sudo session. Specs that
// need annotations or large data sets reshape `/api/timeline` with `page.route`, so they run
// unchanged against a real server.

const PANE1 = 'pane:lab1/4242:991/%1';

async function startOf(page: Page, uuid: string): Promise<number> {
  return page.evaluate(async (u) => {
    const r = await fetch(`/api/sessions/${u}`);
    return ((await r.json()) as { session: { started_at: number } }).session.started_at;
  }, uuid);
}

/** The needle session's first 25 s: one row, every block wide enough to click. */
async function needleWindow(page: Page): Promise<number> {
  await page.goto('/');
  const base = await startOf(page, FIX.needle);
  await page.goto(`/timeline?from=${base - 5}&to=${base + 25}`);
  await expect(page.getByTestId('timeline-chart')).toBeVisible();
  return base;
}

function windowOf(page: Page): { from: number; to: number } {
  const sp = new URL(page.url()).searchParams;
  return { from: Number(sp.get('from')), to: Number(sp.get('to')) };
}

const plot = (page: Page) => page.getByRole('application', { name: /Team timeline/ });
const tip = (page: Page) => page.getByTestId('tl-tip');

async function centre(el: Locator): Promise<{ x: number; y: number }> {
  const box = (await el.boundingBox())!;
  return { x: box.x + box.width / 2, y: box.y + box.height / 2 };
}

test.describe('timeline', () => {
  test('opens with t and lays out people, panes and nested sessions', async ({ page }) => {
    await page.goto('/');
    await page.keyboard.press('t');
    await expect(page).toHaveURL(/\/timeline$/);
    await expect(page.getByTestId('nav-timeline')).toHaveAttribute('aria-current', 'page');
    await expect(page.getByTestId('tl-follow')).toHaveAttribute('aria-pressed', 'true');
    const lanes = page.getByTestId('tl-lane');
    await expect(lanes).toHaveCount(1);
    await expect(lanes.first()).toHaveAttribute('data-key', 'user:alice');
    const rows = page.getByTestId('tl-row');
    await expect(rows).toHaveCount(4);
    // The nested sudo session is drawn in its parent's row.
    const needle = page.locator(`[data-testid="tl-row"][data-key="${PANE1}"]`);
    await expect(needle).toHaveText('work:0.%1');
    await expect(needle).toHaveAttribute('data-blocks', '6');
    await expect(page.locator('[data-testid="tl-row"][data-key="tty:lab1//dev/pts/3"]')).toHaveText('sshd-session pts/3');
    // The live session's row has a live dot; the 10-day-old legacy session is outside.
    await expect(page.locator('[data-testid="tl-row"] .live-dot')).toHaveCount(1);
    await expect(page.locator(`[data-testid="tl-row"][data-key="session:${FIX.legacy}"]`)).toHaveCount(0);
    await expect(page.getByTestId('tl-summary')).toContainText('in 4 rows');
  });

  test('groups by pane', async ({ page }) => {
    await page.goto('/timeline');
    await page.getByRole('radio', { name: 'by pane' }).check();
    await expect(page).toHaveURL(/group=pane/);
    await expect(page.getByTestId('tl-lane')).toHaveCount(4);
    await expect(page.locator(`[data-testid="tl-lane"][data-key="${PANE1}"]`)).toHaveText('work:0.%1');
    await expect(page.locator(`[data-testid="tl-row"][data-key="${PANE1}|user:alice"]`)).toHaveText('alice');
    await page.getByRole('radio', { name: 'by person' }).check();
    await expect(page).not.toHaveURL(/group=/);
    await expect(page.getByTestId('tl-lane')).toHaveCount(1);
  });

  test('keyboard and pointer: hover shows the command, a click opens its commands and player', async ({ page }) => {
    await needleWindow(page);
    await plot(page).focus();
    await page.keyboard.press('ArrowRight');
    await expect(page.getByTestId('tl-cursor')).toBeVisible();
    await expect(tip(page)).toContainText('$ echo hello');
    await page.keyboard.press('ArrowRight');
    await expect(tip(page)).toContainText('$ grep -n needle haystack.txt');
    await expect(tip(page)).toContainText('/srv');
    await expect(tip(page)).toContainText('exit 0');
    await page.keyboard.press('Enter');
    await expect(page).toHaveURL(new RegExp(`open=${FIX.needle}&cmd=2`));
    await expect(page.getByTestId('tl-session')).toBeVisible();
    await waitForPlayer(page);
    await expect(page.getByTestId('current-command')).toContainText('grep -n needle');
    await expect(page.getByTestId('tl-selected')).toBeVisible();

    // The pointer: the third command (failed, its output is markup) by its position.
    await plot(page).focus();
    await page.keyboard.press('ArrowRight');
    await expect(tip(page)).toContainText('$ cat evil.html');
    const at = await centre(page.getByTestId('tl-cursor'));
    await page.mouse.move(at.x, at.y);
    await expect(tip(page)).toContainText('exit 1');
    await page.mouse.click(at.x, at.y);
    await expect(page).toHaveURL(new RegExp(`open=${FIX.needle}&cmd=3`));
    await expect(page.getByTestId('current-command')).toContainText('cat evil.html');
    // The nested session's command sits between, in the same row.
    await plot(page).focus();
    await page.keyboard.press('ArrowRight');
    await expect(tip(page)).toContainText('$ id');
    await page.keyboard.press('Enter');
    await expect(page).toHaveURL(new RegExp(`open=${FIX.nested}&cmd=1`));
    await page.getByRole('link', { name: /Open session/ }).click();
    await expect(page).toHaveURL(new RegExp(`/s/${FIX.nested}\\?cmd=1$`));
  });

  test('a refreshed response keeps the keyboard cursor on its command', async ({ page }) => {
    // After the switch every response lacks the needle row's first command, so the
    // cursor's command moves to another index.
    let slid = false;
    await page.route('**/api/timeline?*', async (route) => {
      const resp = await route.fetch();
      const tl = (await resp.json()) as Timeline;
      if (slid) {
        for (const lane of tl.lanes) for (const row of lane.rows) if (row.key === PANE1) row.blocks = row.blocks.filter((b) => !b.typed.startsWith('echo hello'));
      }
      await route.fulfill({ response: resp, json: tl });
    });
    await needleWindow(page);
    await plot(page).focus();
    await page.keyboard.press('ArrowRight');
    await page.keyboard.press('ArrowRight');
    await expect(tip(page)).toContainText('$ grep -n needle haystack.txt');
    slid = true;
    const before = windowOf(page);
    const refreshed = page.waitForResponse((r) => r.url().includes('/api/timeline?'));
    await page.keyboard.press('-');
    await refreshed;
    await expect.poll(() => windowOf(page).to - windowOf(page).from).toBeCloseTo((before.to - before.from) * 2, 2);
    await expect.poll(() => page.getByTestId('timeline-chart').getAttribute('data-to')).toBe(String(windowOf(page).to));
    await expect(tip(page)).toContainText('$ grep -n needle haystack.txt');
    await expect(page.getByTestId('tl-cursor')).toBeVisible();
    await page.keyboard.press('Enter');
    await expect(page).toHaveURL(new RegExp(`open=${FIX.needle}&cmd=2`));
  });

  test('shows pauses and nested sessions; the nested point opens the child', async ({ page }) => {
    await needleWindow(page);
    const row = page.locator(`[data-testid="tl-row-plot"][data-key="${PANE1}"]`);
    await expect(row.locator('[data-testid="tl-mark"][data-kind="pause"]')).toHaveCount(1);
    const nested = row.locator('[data-testid="tl-mark"][data-kind="nested"]');
    await expect(nested).toHaveCount(1);
    // Near its start: the band may run on to the session end, over later commands.
    const pause = (await row.locator('[data-kind="pause"]').boundingBox())!;
    await page.mouse.move(pause.x + 4, pause.y + 3);
    await expect(tip(page)).toContainText('recording paused (multiplexer: tmux: client)');
    const box = (await nested.boundingBox())!;
    await page.mouse.move(box.x + box.width / 2, box.y + box.height / 2);
    await expect(tip(page)).toContainText('nested session: root');
    await page.mouse.click(box.x + box.width / 2, box.y + box.height / 2);
    await expect(page).toHaveURL(new RegExp(`open=${FIX.nested}$`));
    await expect(page.getByTestId('tl-session')).toBeVisible();
  });

  test('zooms with + and -, the buttons and a brush; pans; follows again', async ({ page }) => {
    await page.goto('/');
    const base = await startOf(page, FIX.needle);
    await page.goto(`/timeline?from=${base - 600}&to=${base + 600}`);
    await expect(page.getByTestId('timeline-chart')).toBeVisible();
    await expect(page.getByTestId('tl-follow')).toHaveAttribute('aria-pressed', 'false');
    await plot(page).focus();
    await page.keyboard.press('+');
    await expect.poll(() => windowOf(page).to - windowOf(page).from).toBeCloseTo(600, 2);
    expect(windowOf(page).from).toBeCloseTo(base - 300, 2);
    await page.keyboard.press('-');
    await expect.poll(() => windowOf(page).to - windowOf(page).from).toBeCloseTo(1200, 2);
    await page.getByTestId('tl-zoom-out').click();
    await expect.poll(() => windowOf(page).to - windowOf(page).from).toBeCloseTo(2400, 2);
    await page.getByTestId('tl-later').click();
    await expect.poll(() => windowOf(page).from).toBeCloseTo(base - 1200 + 1200, 2);
    await page.getByTestId('tl-earlier').click();
    await expect.poll(() => windowOf(page).from).toBeCloseTo(base - 1200, 2);

    const before = windowOf(page);
    const box = (await plot(page).boundingBox())!;
    const y = box.y + 30;
    await page.mouse.move(box.x + box.width * 0.25, y);
    await page.mouse.down();
    await page.mouse.move(box.x + box.width * 0.4, y, { steps: 5 });
    await expect(page.getByTestId('tl-brush')).toBeVisible();
    await page.mouse.move(box.x + box.width * 0.5, y, { steps: 5 });
    await page.mouse.up();
    await expect.poll(() => windowOf(page).to - windowOf(page).from).toBeLessThan((before.to - before.from) * 0.4);
    const after = windowOf(page);
    expect(after.from).toBeGreaterThan(before.from);
    expect(after.to).toBeLessThan(before.to);
    await page.getByTestId('tl-follow').click();
    await expect(page).toHaveURL(/\/timeline$/);
    await expect(page.getByTestId('tl-follow')).toHaveAttribute('aria-pressed', 'true');
  });

  test('the date picker shows one calendar day', async ({ page }) => {
    await page.goto('/timeline');
    await page.getByTestId('tl-day').fill('2020-01-15');
    const want = await page.evaluate(() => [new Date(2020, 0, 15).getTime() / 1000, new Date(2020, 0, 16).getTime() / 1000]);
    await expect.poll(() => [windowOf(page).from, windowOf(page).to]).toEqual(want);
    await expect(page.getByTestId('tl-empty')).toBeVisible();
  });

  test('draws annotations over their row in the tag colour', async ({ page }) => {
    const base = await (async () => {
      await page.goto('/');
      return startOf(page, FIX.needle);
    })();
    await page.route('**/api/timeline?*', async (route) => {
      const resp = await route.fetch();
      const tl = (await resp.json()) as Timeline;
      const note = { session: FIX.needle, row: PANE1, cmd_seq: null, tags: ['incident'], author: 'alice' };
      tl.annotations = [
        { ...note, uuid: 'a1', ts_from: base + 2.4, ts_to: base + 3.6, color: '#ff00aa' },
        { ...note, uuid: 'a2', ts_from: base + 18, ts_to: base + 19, color: 'red;fill:url(x)' },
      ];
      await route.fulfill({ response: resp, json: tl });
    });
    await page.goto(`/timeline?from=${base - 5}&to=${base + 25}`);
    const a1 = page.locator('[data-testid="tl-annotation"][data-uuid="a1"]');
    await expect(a1).toBeVisible();
    await expect(a1).toHaveCSS('fill', 'rgb(255, 0, 170)');
    const accent = await page.evaluate(() => getComputedStyle(document.documentElement).getPropertyValue('--accent').trim());
    expect(accent).not.toBe('');
    await expect(page.locator('[data-uuid="a2"]')).not.toHaveCSS('fill', 'rgb(255, 0, 0)');
    const at = await centre(a1);
    await page.mouse.move(at.x, at.y);
    await expect(tip(page)).toContainText('annotation by alice');
    await expect(tip(page)).toContainText('#incident');
  });

  test('a bad window shows the server error', async ({ page }) => {
    await page.goto('/timeline?from=0&to=99999999');
    await expect(page.getByTestId('tl-error')).toBeVisible();
  });
});

// ------------------------------------------------------------------ large data sets

/** 10 people with 3 panes, 7 days, `per` commands per pane and day (plan §13 M4 Accept 2). */
function week(to: number, per: number, aggregated: boolean): Timeline {
  const from = to - 7 * 86_400;
  const lanes: TimelineLane[] = [];
  let commands = 0;
  for (let p = 0; p < 10; p += 1) {
    const rows = [];
    for (let pane = 0; pane < 3; pane += 1) {
      const session = `0b9e4c1a-0000-7000-8000-${String(p * 3 + pane).padStart(12, '0')}`;
      const blocks: TimelineBlock[] = [];
      for (let d = 0; d < 7; d += 1) {
        const start = from + d * 86_400 + 8 * 3600 + pane * 600;
        for (let c = 0; c < per; c += 1) {
          const ts = start + c * ((9 * 3600) / per);
          blocks.push({ session, seq: d * per + c + 1, t_start: ts - from, ts_start: ts, ts_end: ts + 25, exit: c % 11 === 0 ? 1 : 0, typed: `make target-${c}`, cwd: '/srv', approx: false, depth: 0 });
        }
      }
      commands += blocks.length;
      const density = aggregated
        ? Array.from({ length: 7 * 36 }, (_, i) => ({ ts: from + Math.floor(i / 36) * 86_400 + 8 * 3600 + (i % 36) * 900, commands: 40, failed: 4 }))
        : [];
      rows.push({
        key: `pane:h/1:2/%${p * 3 + pane}`,
        label: `work:0.%${p * 3 + pane}`,
        host: 'h',
        user: `user${p}`,
        tmux: { server: '1:2', session_id: '$0', session: 'work', window: '0', window_name: null, pane: `%${p * 3 + pane}` },
        holder_comm: 'tmux: server',
        tty: '/dev/pts/1',
        sessions: [{ uuid: session, user: 'root', state: 'ended' as const, started_at: from, ended_at: to, parent_uuid: null, legacy: false }],
        blocks: aggregated ? [] : blocks,
        marks: [{ session, kind: 'skip' as const, ts: from + 9 * 3600, ts_end: null, reason: null, detail: null, child: null, child_scope: null, child_url: null }],
        density,
      });
    }
    lanes.push({ key: `user:user${p}`, label: `user${p}`, rows });
  }
  return { from, to, group: 'user', lanes, idle: [], annotations: [], commands, aggregated, bin_secs: aggregated ? 900 : null };
}

test.describe('timeline at scale', () => {
  test('renders one week of team data in under 500 ms', async ({ page }) => {
    const data = week(Math.floor(Date.now() / 1000), 95, false);
    await page.route('**/api/timeline*', (route) => route.fulfill({ json: data }));
    await page.goto('/timeline');
    const chart = page.getByTestId('timeline-chart');
    await expect(chart).toHaveAttribute('data-blocks', String(data.commands));
    await expect(page.getByTestId('tl-row')).toHaveCount(30);
    await expect(page.locator('[data-testid="tl-mark"][data-kind="skip"]')).toHaveCount(30);
    const ms = Number(await chart.getAttribute('data-render-ms'));
    console.log(`timeline render of ${data.commands} blocks: ${ms} ms`);
    expect(ms).toBeLessThan(500);
    // Still interactive: the keyboard reaches the first command.
    await plot(page).focus();
    await page.keyboard.press('ArrowRight');
    await expect(tip(page)).toContainText('$ make target-0');
  });

  test('draws density bars above 20 000 commands; a bar zooms in', async ({ page }) => {
    const data = week(Math.floor(Date.now() / 1000), 120, true);
    await page.route('**/api/timeline*', (route) => route.fulfill({ json: data }));
    await page.goto('/timeline');
    await expect(page.getByTestId('timeline-chart')).toHaveAttribute('data-aggregated', 'true');
    await expect(page.getByTestId('tl-summary')).toContainText('density, 15m 00s bins');
    const bar = page.getByTestId('tl-density').first();
    await expect(bar).toBeVisible();
    const box = (await bar.boundingBox())!;
    await page.mouse.move(box.x + box.width / 2, box.y + box.height / 2);
    await expect(tip(page)).toContainText('40 commands, 4 failed');
    await page.mouse.click(box.x + box.width / 2, box.y + box.height / 2);
    await expect(page).toHaveURL(/from=.*&to=/);
    expect(windowOf(page).to - windowOf(page).from).toBeCloseTo(900 * 12, 0);
  });
});
