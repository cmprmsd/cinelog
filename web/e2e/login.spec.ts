import { expect, FIX, test, token } from './helpers.ts';

test.describe('login', () => {
  test('shows the fingerprint and rejects a wrong token', async ({ page }) => {
    const requests: string[] = [];
    page.on('request', (r) => requests.push(r.url()));
    await page.goto('/login');
    await expect(page.getByTestId('fingerprint')).toHaveText(/^([0-9A-F]{2}:){31}[0-9A-F]{2}$/);
    await page.getByLabel('Access token').fill('AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA');
    const [res] = await Promise.all([
      page.waitForResponse((r) => r.url().endsWith('/api/login') && r.request().method() === 'POST'),
      page.getByRole('button', { name: 'Sign in' }).click(),
    ]);
    expect(res.status()).toBe(401);
    await expect(page.getByRole('alert')).toHaveText('That token is not valid.');
    expect(requests.some((u) => u.includes('AAAAAAAA'))).toBe(false);
  });

  test('sets an HttpOnly host cookie, returns to the page, and signs out', async ({ page, context, browser, baseURL }) => {
    const target = `/s/${FIX.needle}?cmd=2`;
    await page.goto(target);
    await expect(page).toHaveURL(/\/login\?next=/);
    await page.getByLabel('Access token').fill(`  ${token().toLowerCase()}  `);
    await page.keyboard.press('Enter');
    await expect(page).toHaveURL(new RegExp(`${target.replace(/[?]/g, '\\?')}$`));
    await expect(page.getByTestId('session-view')).toBeVisible();
    expect(page.url()).not.toContain(token());

    expect(await page.evaluate(() => document.cookie)).toBe('');
    const cookies = await context.cookies();
    const session = cookies.find((c) => c.name.startsWith('__Host-cl-'));
    expect(session).toMatchObject({ httpOnly: true, secure: true, sameSite: 'Strict', path: '/' });

    // The same cookie from another User-Agent is rejected (and revokes the session).
    const other = await browser.newContext({ ignoreHTTPSErrors: true, userAgent: 'cinelog-e2e-other-agent', baseURL });
    await other.addCookies(cookies);
    const res = await other.request.get('/api/me');
    expect(res.status()).toBe(401);
    await other.close();

    await page.reload();
    await expect(page).toHaveURL(/\/login\?next=/);

    // Sign in again and out through the header.
    await page.getByLabel('Access token').fill(token());
    await page.getByRole('button', { name: 'Sign in' }).click();
    await expect(page.getByTestId('session-view')).toBeVisible();
    await page.getByRole('button', { name: 'Sign out' }).click();
    await expect(page).toHaveURL(/\/login/);
    expect((await page.request.get('/api/me')).status()).toBe(401);
  });

  test('API without a cookie is 401 and cross-origin writes are 403', async ({ playwright, baseURL }) => {
    const api = await playwright.request.newContext({ baseURL, ignoreHTTPSErrors: true });
    expect((await api.get('/api/search?q=needle')).status()).toBe(401);
    expect((await api.get('/healthz')).status()).toBe(200);
    const port = new URL(baseURL!).port;
    const res = await api.post('/api/login', {
      headers: { Origin: `https://127.0.0.1:${Number(port) + 1}` },
      data: { token: token() },
    });
    expect(res.status()).toBe(403);
    await api.dispose();
  });
});
