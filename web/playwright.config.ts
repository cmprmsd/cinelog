import { defineConfig, devices } from '@playwright/test';

// E2E against a real `cinelog serve` with fixtures (plan §12.3). `CINELOG_E2E_URL` is the
// server's base URL; `CINELOG_CHROMIUM` points at a system Chromium when Playwright's own
// browser is not installed.
export default defineConfig({
  testDir: 'e2e',
  fullyParallel: false,
  forbidOnly: !!process.env.CI,
  retries: process.env.CI ? 1 : 0,
  reporter: process.env.CI ? 'github' : 'list',
  use: {
    baseURL: process.env.CINELOG_E2E_URL,
    ignoreHTTPSErrors: true,
    trace: 'retain-on-failure',
  },
  projects: [
    {
      name: 'chromium',
      use: {
        ...devices['Desktop Chrome'],
        launchOptions: process.env.CINELOG_CHROMIUM
          ? { executablePath: process.env.CINELOG_CHROMIUM }
          : {},
      },
    },
  ],
});
