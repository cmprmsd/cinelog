import { defineConfig, devices } from '@playwright/test';

// E2E (plan §12.3). Without `CINELOG_E2E_URL` the global setup starts the contract mock
// (`e2e/mock/server.ts`, HTTPS, serving `dist/`), or with `CINELOG_E2E_BIN` a real
// `cinelog serve` over the fixture casts (`e2e/real/`); with `CINELOG_E2E_URL` the specs run
// against that server, which must carry the same fixtures and accept `CINELOG_E2E_TOKEN`. `CINELOG_CHROMIUM`
// points at a system Chromium when Playwright's own browser is not installed.
// Without the device's User-Agent override: Chromium drops the override on some navigations
// while an EventSource (the UI bus) is open, and sessions are bound to the User-Agent, so a
// changing one signs the test out.
const desktopChrome: Partial<(typeof devices)[string]> = { ...devices['Desktop Chrome'] };
delete desktopChrome.userAgent;
const chromium = {
  ...desktopChrome,
  launchOptions: process.env.CINELOG_CHROMIUM ? { executablePath: process.env.CINELOG_CHROMIUM } : {},
};

// e2e/docs/ holds the documentation screenshots with their own config (`pnpm docs:shots`).
const docs = /[\\/]e2e[\\/]docs[\\/]/;

export default defineConfig({
  testDir: 'e2e',
  testIgnore: docs,
  globalSetup: './e2e/global-setup.ts',
  fullyParallel: false,
  workers: 1,
  forbidOnly: !!process.env.CI,
  // A real server keeps the forget of the previous attempt: no retries there.
  retries: process.env.CI && !process.env.CINELOG_E2E_BIN && !process.env.CINELOG_E2E_URL ? 1 : 0,
  reporter: process.env.CI ? 'github' : 'list',
  use: {
    ignoreHTTPSErrors: true,
    trace: 'retain-on-failure',
  },
  projects: [
    { name: 'setup', testMatch: /auth\.setup\.ts/, use: chromium },
    { name: 'login', testMatch: /login\.spec\.ts/, use: chromium, dependencies: ['setup'] },
    {
      name: 'chromium',
      testIgnore: [/login\.spec\.ts/, docs],
      use: { ...chromium, storageState: 'e2e/.auth/state.json' },
      dependencies: ['setup'],
    },
  ],
});
