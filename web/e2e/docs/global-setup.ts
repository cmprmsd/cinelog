// Starts `cinelog serve` over the demo data set, signs in once through the login page,
// adds the annotations and tag colours the screenshots show, and stores the signed-in state
// for the specs. Sessions are bound to the User-Agent, so the sign-in uses the same browser
// the specs run in.

import { chromium, type Page } from '@playwright/test';
import { existsSync, rmSync } from 'node:fs';
import { DEMO } from './dataset.ts';
import { startDemo } from './server.ts';

async function post(page: Page, path: string, body: unknown): Promise<unknown> {
  return page.evaluate(
    async ([path, body]) => {
      const r = await fetch(path as string, { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify(body) });
      if (!r.ok) throw new Error(`${path}: ${r.status} ${await r.text()}`);
      return r.json();
    },
    [path, body] as const,
  );
}

async function annotate(page: Page): Promise<void> {
  await post(page, '/api/tags', { name: 'release', color: '#7c5cff' });
  await post(page, '/api/tags', { name: 'incident', color: '#e5484d' });
  await post(page, '/api/tags', { name: 'runbook', color: '#12a594' });
  await post(page, '/api/annotations', {
    session_uuid: DEMO.deploy,
    cmd_seq: DEMO.deploySeq,
    body: 'Rolled out 2.4.0 with the payment retry fix. Runbook: https://wiki.internal/shop/deploy',
    tags: ['release', 'runbook'],
  });
  await post(page, '/api/annotations', {
    session_uuid: DEMO.deploy,
    cmd_seq: 4,
    body: 'Lint failed on an unused variable; fixed in 9f3e7aa.',
    tags: ['incident'],
  });
  await post(page, '/api/annotations', {
    session_uuid: DEMO.logs,
    t_from: 88,
    t_to: 100,
    body: 'Timeouts start at 09:12, right after the payment provider maintenance window.',
    tags: ['incident'],
  });
}

export default async function globalSetup(): Promise<() => Promise<void>> {
  if (!existsSync(new URL('../../dist/index.html', import.meta.url))) throw new Error('web/dist is missing: run pnpm build first');
  const state = process.env.CINELOG_DOCS_STATE;
  if (!state) throw new Error('CINELOG_DOCS_STATE is set by e2e/docs/playwright.config.ts');
  const demo = await startDemo(process.env.CINELOG_E2E_BIN ?? '../target/debug/cinelog');
  const teardown = async () => {
    rmSync(state, { force: true });
    await demo.close();
  };
  try {
    const exe = process.env.CINELOG_CHROMIUM;
    const browser = await chromium.launch(exe ? { executablePath: exe } : {});
    try {
      const context = await browser.newContext({ ignoreHTTPSErrors: true, baseURL: demo.url });
      const page = await context.newPage();
      await page.goto('/login');
      await page.getByLabel('Access token').fill(demo.token);
      await page.getByRole('button', { name: 'Sign in' }).click();
      await page.getByTestId('search-input').waitFor();
      await annotate(page);
      await context.storageState({ path: state });
    } finally {
      await browser.close();
    }
  } catch (e) {
    await teardown();
    throw e;
  }
  process.env.CINELOG_DOCS_URL = demo.url;
  process.env.CINELOG_DOCS_LEGACY = demo.legacy;
  return teardown;
}
