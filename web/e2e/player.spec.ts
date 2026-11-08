import { expect, FIX, playerTime, search, settledPlayerTime, terminalText, test, waitForPlayer } from './helpers.ts';

test.describe('player', () => {
  test('a result click seeks to the hit and highlights its command', async ({ page }) => {
    await page.goto('/');
    await search(page, 'needle', 'out');
    await page.getByTestId('results').getByRole('listitem').filter({ hasText: 'a needle in the haystack' }).getByRole('link').click();
    await waitForPlayer(page);
    // The output hit is written 0.4 s after the command at t=4.
    await expect.poll(() => playerTime(page)).toBeGreaterThan(3.4);
    expect(Math.abs((await settledPlayerTime(page)) - 4.4)).toBeLessThan(1);
    // The frame is the one at the hit: the command is on screen, the next one is not yet.
    await expect(page.locator('.ap-term')).toContainText('grep -n needle haystack.txt');
    await expect(page.locator('.ap-term')).not.toContainText('cat evil.html');
    await expect(page.getByTestId('current-command')).toContainText('grep -n needle haystack.txt');
    await expect(page.locator('[data-testid="command-strip"] button[aria-current="step"]')).toContainText('grep -n needle');
  });

  test('[ and ] move between chapters', async ({ page }) => {
    await page.goto(`/s/${FIX.needle}?cmd=1`);
    await waitForPlayer(page);
    await expect(page.getByTestId('current-command')).toContainText('echo hello');
    await page.locator('body').click({ position: { x: 1, y: 1 } });
    await page.keyboard.press(']');
    await expect(page.getByTestId('current-command')).toContainText('grep -n needle');
    await page.keyboard.press(']');
    await expect(page.getByTestId('current-command')).toContainText('cat evil.html');
    await page.keyboard.press('[');
    await expect(page.getByTestId('current-command')).toContainText('grep -n needle');
    // At the next chapter the terminal shows the earlier output as text, never as markup.
    await page.keyboard.press(']');
    await page.keyboard.press(']');
    await expect(page.getByTestId('current-command')).toContainText('ls /srv');
    await expect(page.locator('.ap-term')).toContainText(FIX.evil);
    await expect(page.locator('[data-testid="player"] img')).toHaveCount(0);
  });

  test('shows pause and nested markers, and links parent and child', async ({ page }) => {
    await page.goto(`/s/${FIX.needle}`);
    await waitForPlayer(page);
    const markers = page.getByTestId('markers');
    await expect(markers.getByRole('button', { name: /paused \(multiplexer: tmux: client\)/ })).toBeVisible();
    await expect(page.locator('[data-testid="rail"] .band.pause')).toHaveCount(1);
    await markers.getByRole('link', { name: /nested: root/ }).click();
    await expect(page).toHaveURL(new RegExp(`/s/${FIX.nested}$`));
    await expect(page.getByTestId('session-view')).toHaveAttribute('data-uuid', FIX.nested);
    await page.getByTestId('parent-link').click();
    await expect(page.getByTestId('session-view')).toHaveAttribute('data-uuid', FIX.needle);
  });

  test('legacy chapters carry the approx badge', async ({ page }) => {
    await page.goto(`/s/${FIX.legacy}`);
    await waitForPlayer(page);
    await expect(page.locator('[data-testid="command-strip"] .chap.approx')).toHaveCount(2);
    await expect(page.getByTestId('session-view').locator('.badge.legacy').first()).toBeVisible();
  });

  test('large casts play segmented and fetch only the needed segments', async ({ page }) => {
    const segs: string[] = [];
    page.on('request', (r) => {
      const m = /\/play\/seg\/(\d+)$/.exec(r.url());
      if (m) segs.push(m[1]!);
    });
    await page.goto(`/s/${FIX.segmented}?cmd=1`);
    await waitForPlayer(page);
    await expect(page.getByTestId('player')).toHaveAttribute('data-mode', 'segmented');
    // The server decides the segment layout: look up which segments hold t=2 and t=112.
    const index = await (await page.request.get(`/api/sessions/${FIX.segmented}/play/index.json`)).json();
    const starts: number[] = index.segments.map((s: { start: number }) => s.start);
    const segAt = (t: number) => String(starts.filter((s) => s <= t).length - 1);
    const [first, last] = [segAt(2), segAt(112)];
    expect(Number(last)).toBeGreaterThan(Number(first) + 1);
    await expect.poll(() => segs.length).toBeGreaterThan(0);
    expect(segs).toContain(first);
    expect(segs).not.toContain(last);
    // Jump to the last chapter (t=112).
    await page.getByTestId('command-strip').getByRole('button', { name: '$ step 12' }).click();
    await expect.poll(() => segs).toContain(last);
    await expect.poll(() => playerTime(page)).toBeGreaterThan(111);
    await expect(page.getByTestId('current-command')).toContainText('step 12');
    await expect(page.locator('.ap-term')).toContainText('segmented output 11');
    // Only the segments around the two positions were fetched, never the whole recording.
    expect(new Set(segs).size).toBeLessThan(starts.length);
  });

  test('exports the current command as text and HTML', async ({ page }) => {
    await page.goto(`/s/${FIX.needle}?cmd=3`);
    await waitForPlayer(page);
    await expect(page.getByTestId('current-command')).toContainText('cat evil.html');
    await page.getByText('Export', { exact: true }).click();
    const [txt] = await Promise.all([page.waitForEvent('download'), page.getByRole('menuitem', { name: '.txt' }).first().click()]);
    expect(txt.suggestedFilename()).toMatch(/^cinelog-0b9e4c1a-.*\.txt$/);
    const text = await (await txt.createReadStream()).toArray();
    expect(Buffer.concat(text).toString()).toContain('onerror');
    await page.getByText('Export', { exact: true }).click();
    const [html] = await Promise.all([page.waitForEvent('download'), page.getByRole('menuitem', { name: '.html' }).first().click()]);
    const body = Buffer.concat(await (await html.createReadStream()).toArray()).toString();
    expect(body).not.toMatch(/<script/i);
    expect(body).not.toContain('<img');
    expect(body).toMatch(/&lt;img|&#60;img/);
  });

  test('n and N step through every hit of the session, across cursor pages', async ({ page }) => {
    // Session hits arrive newest first over two pages; the oldest (t=4) is on the second.
    const cursors: string[] = [];
    await page.route('**/api/search?*', async (route) => {
      const url = new URL(route.request().url());
      if (url.searchParams.get('session') !== FIX.needle) return route.continue();
      const cursor = url.searchParams.get('cursor');
      cursors.push(cursor ?? '');
      const base = await (await route.fetch({ url: `${url.origin}/api/search?q=needle&in=both&session=${FIX.needle}` })).json();
      const at = (ts: number[]) => ts.map((t, i) => ({ ...base.hits[0], id: `x:${t}:${i}`, t }));
      const body = cursor === 'p2' ? { hits: at([8.4, 4]), next: null } : { hits: at([16, 12.4]), next: 'p2' };
      return route.fulfill({ json: { ...body, partial: false, capped: false } });
    });
    await page.goto(`/s/${FIX.needle}?q=needle&in=both`);
    await waitForPlayer(page);
    await expect(page.getByTestId('hit-count')).toContainText('4 hits here');
    await expect(page.getByTestId('hit-count')).not.toContainText('more hits than shown');
    expect(cursors).toEqual(['', 'p2']);
    await expect(page.locator('[data-testid="rail"] .hit')).toHaveCount(4);
    await page.locator('body').click({ position: { x: 1, y: 1 } });
    await page.keyboard.press('n');
    await expect(page.getByTestId('current-command')).toContainText('grep -n needle');
    expect(Math.abs((await settledPlayerTime(page)) - 4)).toBeLessThan(0.1);
    await expect(page.locator('.ap-term')).toContainText('grep -n needle haystack.txt');
    await page.keyboard.press('n');
    await expect(page.getByTestId('current-command')).toContainText('cat evil.html');
    await page.keyboard.press('n');
    await expect(page.getByTestId('current-command')).toContainText('ls /srv');
    await page.keyboard.press('N');
    await expect(page.getByTestId('current-command')).toContainText('cat evil.html');
    expect(Math.abs((await settledPlayerTime(page)) - 8.4)).toBeLessThan(0.1);
    await expect(page.locator('.ap-term')).toContainText(FIX.evil);
    await expect(page.locator('.ap-term')).not.toContainText('ls /srv');
  });

  test('a recording still being prepared shows progress, then plays', async ({ page }) => {
    let polls = 0;
    await page.route(`**/api/sessions/${FIX.needle}/play`, async (route) => {
      const info = await (await route.fetch()).json();
      polls += 1;
      if (polls <= 2) return route.fulfill({ json: { ...info, ready: false, progress: polls * 0.3 } });
      return route.fulfill({ json: info });
    });
    await page.goto(`/s/${FIX.needle}?cmd=2`);
    await expect(page.getByRole('status').filter({ hasText: 'Preparing playback' })).toBeVisible();
    await expect(page.getByTestId('player')).toHaveAttribute('data-ready', 'false');
    await waitForPlayer(page);
    expect(polls).toBe(3);
    await expect(page.getByRole('status').filter({ hasText: 'Preparing playback' })).toHaveCount(0);
    expect(Math.abs((await settledPlayerTime(page)) - 4)).toBeLessThan(0.1);
  });

  test('forget masks the loaded recording and drops the hit from the open results', async ({ page }) => {
    await page.goto('/');
    await search(page, FIX.forgetWord, 'out');
    const rows = page.getByTestId('results').getByRole('listitem');
    await expect(rows.first()).toBeVisible();
    // The mock has one hit; a real index also has the echoed command line (see below).
    const before = await rows.count();
    await rows.first().getByRole('link').click();
    await waitForPlayer(page);
    await expect(page.getByTestId('current-command')).toContainText(FIX.forgetWord);
    // Past the output: the command line and its output both show the word.
    await page.getByTestId('command-strip').getByRole('button', { name: '$ echo keep-me' }).click();
    await expect(page.locator('.ap-term')).toContainText('keep-me');
    const shown = await terminalText(page);
    expect(shown.split(FIX.forgetWord).length - 1).toBe(2);
    await page.getByTestId('command-strip').getByRole('button', { name: `$ echo ${FIX.forgetWord}` }).click();
    await expect(page.getByTestId('current-command')).toContainText(FIX.forgetWord);
    const at = await settledPlayerTime(page);
    page.once('dialog', (d) => void d.accept());
    await page.getByRole('button', { name: 'Forget…' }).click();
    await expect(page.getByRole('status').filter({ hasText: 'Forgotten.' })).toBeVisible();
    await expect(page.locator('[data-testid="rail"] .band.forgotten')).toHaveCount(1);
    // The open result list drops the command's hit at once.
    await expect(rows).toHaveCount(before - 1);
    // The reloaded player keeps the position and plays the masked recording.
    await waitForPlayer(page);
    expect(Math.abs((await settledPlayerTime(page)) - at)).toBeLessThan(0.1);
    await page.getByTestId('command-strip').getByRole('button', { name: '$ echo keep-me' }).click();
    await expect(page.locator('.ap-term')).toContainText('[redacted by');
    // At most the typed command line is left (the server may mask its echo too).
    const after = await terminalText(page);
    expect(after.split(FIX.forgetWord).length - 1).toBeLessThan(2);
  });

  test('a forgotten command is gone from search', async ({ page }) => {
    // Also its echo: zsh prints the command line before preexec, into the output before it.
    await page.goto(`/?q=${FIX.forgetWord}&in=both`);
    await expect(page.getByTestId('result-count')).toContainText('0 hits');
    await expect(page.getByTestId('results')).not.toContainText(FIX.forgetWord);
  });
});
