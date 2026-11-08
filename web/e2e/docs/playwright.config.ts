import { defineConfig, devices } from '@playwright/test';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

// Documentation screenshots (`pnpm docs:shots`): a real `cinelog serve` over the synthetic
// demo data set (`dataset.ts`), every view once per theme, written to
// docs/screenshots/<view>-<light|dark>.png. `CINELOG_E2E_BIN` names the binary (default
// ../target/debug/cinelog), `CINELOG_CHROMIUM` a system Chromium.

// The signed-in state lives outside the repository; workers inherit the path.
process.env.CINELOG_DOCS_STATE ??= join(process.env.CINELOG_E2E_TMP ?? tmpdir(), `cinelog-docs-state-${process.pid}.json`);

// Without the device's User-Agent override: sessions are bound to the User-Agent the
// global setup signed in with (the same browser).
const desktop: Partial<(typeof devices)[string]> = { ...devices['Desktop Chrome'] };
delete desktop.userAgent;
const browser = {
  ...desktop,
  viewport: { width: 1440, height: 900 },
  deviceScaleFactor: 1,
  launchOptions: process.env.CINELOG_CHROMIUM ? { executablePath: process.env.CINELOG_CHROMIUM } : {},
};

export default defineConfig({
  testDir: '.',
  testMatch: /\.(shots|test)\.ts$/,
  globalSetup: './global-setup.ts',
  fullyParallel: false,
  workers: 1,
  retries: 0,
  timeout: 60_000,
  reporter: process.env.CI ? 'github' : 'list',
  outputDir: '../../test-results/docs',
  use: {
    ignoreHTTPSErrors: true,
    timezoneId: 'Europe/Berlin',
    locale: 'en-GB',
  },
  projects: [
    { name: 'dataset', testMatch: /\.test\.ts$/ },
    { name: 'light', testMatch: /\.shots\.ts$/, use: { ...browser, colorScheme: 'light' } },
    { name: 'dark', testMatch: /\.shots\.ts$/, use: { ...browser, colorScheme: 'dark' } },
  ],
});
