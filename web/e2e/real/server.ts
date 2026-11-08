// Runs the specs against a real `cinelog serve` (plan §12.3): a throwaway HOME with the
// fixture casts (`casts.ts`), indexed once, a personal token, and the server on an ephemeral
// loopback port. Everything lives in one temp directory that `close()` removes.

import { execFileSync, spawn, type ChildProcess } from 'node:child_process';
import { existsSync, mkdirSync, mkdtempSync, openSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';
import { FIX } from '../mock/fixtures.ts';
import { touchLive, writeFixtures } from './casts.ts';

export interface RealServer {
  url: string;
  token: string;
  /** `FIX` entries that differ on this server (the legacy uuid is derived from the file). */
  fix: Partial<Record<keyof typeof FIX, string>>;
  close(): Promise<void>;
}

const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));

export async function startReal(binary: string): Promise<RealServer> {
  const bin = resolve(binary);
  if (!existsSync(bin)) throw new Error(`CINELOG_E2E_BIN: ${bin} does not exist`);
  const dir = mkdtempSync(join(process.env.CINELOG_E2E_TMP ?? tmpdir(), 'cinelog-e2e-'));
  const home = join(dir, 'home');
  const run = join(dir, 'run');
  const config = join(home, '.config');
  mkdirSync(join(config, 'cinelog'), { recursive: true, mode: 0o700 });
  mkdirSync(run, { mode: 0o700 });
  writeFileSync(join(config, 'cinelog/config.toml'), '[server]\nautostart = "off"\nbind = ["127.0.0.1:0"]\n', { mode: 0o600 });

  const env: NodeJS.ProcessEnv = Object.fromEntries(Object.entries(process.env).filter(([k]) => !k.startsWith('CINELOG_')));
  Object.assign(env, {
    HOME: home,
    XDG_CONFIG_HOME: config,
    XDG_STATE_HOME: join(home, '.local/state'),
    XDG_RUNTIME_DIR: run,
    TZ: 'UTC',
    RUST_LOG: 'info',
  });
  const cl = (...args: string[]) => execFileSync(bin, args, { env, encoding: 'utf8', stdio: ['ignore', 'pipe', 'pipe'] });

  const live = writeFixtures(join(home, 'logs'), Date.now() / 1000);
  cl('index', '--once');
  const token = /\(shown once\): (\S+)/.exec(cl('token', 'new'))?.[1];
  if (!token) throw new Error('cinelog token new printed no token');
  const legacyHit = cl('search', '--json', 'legacy-target').split('\n').find(Boolean);
  if (!legacyHit) throw new Error('the legacy fixture was not imported');
  const legacy = String(JSON.parse(legacyHit).entry).split(':')[0]!;

  const logFd = openSync(join(dir, 'serve.log'), 'a');
  const serve: ChildProcess = spawn(bin, ['serve', '--scope', 'personal'], { env, stdio: ['ignore', logFd, logFd] });
  const info = join(run, 'cinelog/server.json');
  let url = '';
  for (let i = 0; i < 200 && !url; i++) {
    if (serve.exitCode !== null) break;
    if (existsSync(info)) {
      try {
        url = String(JSON.parse(readFileSync(info, 'utf8')).url);
      } catch {
        // Not written completely yet.
      }
    }
    if (!url) await sleep(50);
  }
  const keepAlive = setInterval(() => touchLive(live, Date.now() / 1000), 5000);
  const close = async () => {
    clearInterval(keepAlive);
    if (serve.exitCode === null) {
      const exited = new Promise((r) => serve.once('exit', r));
      serve.kill('SIGTERM');
      await Promise.race([exited, sleep(10_000)]);
      if (serve.exitCode === null) serve.kill('SIGKILL');
    }
    if (process.env.CINELOG_E2E_KEEP) console.log(`cinelog e2e sandbox kept: ${dir}`);
    else rmSync(dir, { recursive: true, force: true });
  };
  if (!url) {
    const log = readFileSync(join(dir, 'serve.log'), 'utf8');
    await close();
    throw new Error(`cinelog serve did not start:\n${log}`);
  }
  // Serve with the hostname it advertises replaced by the loopback address the specs use.
  const port = new URL(url).port;
  return { url: `https://127.0.0.1:${port}`, token, fix: { legacy }, close };
}
