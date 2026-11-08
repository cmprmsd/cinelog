import { existsSync } from 'node:fs';
import { startMock } from './mock/server.ts';
import { startReal } from './real/server.ts';

// Picks the server the specs run against: `CINELOG_E2E_URL` (an already running server with
// the fixtures), `CINELOG_E2E_BIN` (a real `cinelog serve` over the fixture casts in a
// throwaway HOME), or the contract mock. Workers inherit `process.env`.
export default async function globalSetup(): Promise<(() => Promise<void>) | undefined> {
  if (process.env.CINELOG_E2E_URL) {
    if (!process.env.CINELOG_E2E_TOKEN) throw new Error('CINELOG_E2E_URL needs CINELOG_E2E_TOKEN');
    return undefined;
  }
  if (!existsSync(new URL('../dist/index.html', import.meta.url))) throw new Error('web/dist is missing: run pnpm build first');
  if (process.env.CINELOG_E2E_BIN) {
    const real = await startReal(process.env.CINELOG_E2E_BIN);
    process.env.CINELOG_E2E_URL = real.url;
    process.env.CINELOG_E2E_TOKEN = real.token;
    process.env.CINELOG_E2E_FIX = JSON.stringify(real.fix);
    return () => real.close();
  }
  const mock = await startMock();
  process.env.CINELOG_E2E_URL = mock.url;
  process.env.CINELOG_E2E_TOKEN = mock.token;
  process.env.CINELOG_E2E_MOCK = '1';
  return () => mock.close();
}
