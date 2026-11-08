// A real `cinelog serve` over the demo data set (`dataset.ts`): a throwaway HOME below
// `$CINELOG_E2E_TMP` (default the system temp dir), indexed once, one personal token, the
// live session registered with a stand-in recorder, and the server on an ephemeral loopback
// port. `close()` stops every process it started and removes the directory.

import { execFileSync, spawn, spawnSync, type ChildProcess } from 'node:child_process';
import { existsSync, mkdirSync, mkdtempSync, openSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';
import { registerLive } from '../real/server.ts';
import { DEMO, writeDemo } from './dataset.ts';

export interface DemoServer {
  url: string;
  token: string;
  /** The legacy session's uuid, which the server derives from the file. */
  legacy: string;
  close(): Promise<void>;
}

const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));

/**
 * The command prefix that runs a process with the demo host name. Legacy casts carry no
 * host, so the server names them after the machine; a private UTS namespace (an
 * unprivileged user namespace that maps back to the caller's uid) keeps the real host name
 * out of the screenshots. Without user namespaces this throws, unless
 * `CINELOG_DOCS_REAL_HOSTNAME=1` accepts the machine's host name (the commands then run as
 * they are). `unshare` names the binary, for tests.
 */
export function withHostname(name: string, env: NodeJS.ProcessEnv = process.env, unshare = 'unshare'): string[] {
  const uid = process.getuid?.() ?? 0;
  const gid = process.getgid?.() ?? 0;
  // sh -c SCRIPT <name> <uid> <gid> <command…>: set the host name as namespace root, then
  // switch to a nested user namespace in which the caller's own uid and gid are back.
  const script = 'hostname "$0" && u=$1 g=$2 && shift 2 && exec unshare --user --map-user="$u" --map-group="$g" "$@"';
  const prefix = [unshare, '-r', '--uts', 'sh', '-c', script, name, String(uid), String(gid)];
  const probe = spawnSync(prefix[0]!, [...prefix.slice(1), 'hostname'], { encoding: 'utf8' });
  if (probe.status === 0 && probe.stdout.trim() === name) return prefix;
  const why = (probe.stderr || probe.error?.message || `hostname printed ${JSON.stringify(probe.stdout.trim())}`).trim();
  if (env.CINELOG_DOCS_REAL_HOSTNAME === '1') {
    console.warn(`cinelog docs: no private host name (${why}); the legacy session shows this machine's host name`);
    return [];
  }
  throw new Error(
    `cinelog docs: cannot run cinelog under the demo host name ${name} (${why}). Without it the screenshots would show ` +
      `this machine's host name; set CINELOG_DOCS_REAL_HOSTNAME=1 to accept that.`,
  );
}

export async function startDemo(binary: string): Promise<DemoServer> {
  const bin = resolve(binary);
  if (!existsSync(bin)) throw new Error(`CINELOG_E2E_BIN: ${bin} does not exist (build it with cargo build -p cinelog)`);
  const dir = mkdtempSync(join(process.env.CINELOG_E2E_TMP ?? tmpdir(), 'cinelog-docs-'));
  const children: ChildProcess[] = [];

  const close = async () => {
    for (const c of children.reverse()) {
      if (c.exitCode !== null || c.signalCode !== null) continue;
      const exited = new Promise((r) => c.once('exit', r));
      c.kill('SIGTERM');
      await Promise.race([exited, sleep(10_000)]);
      if (c.exitCode === null && c.signalCode === null) c.kill('SIGKILL');
    }
    if (process.env.CINELOG_E2E_KEEP) console.log(`cinelog docs sandbox kept: ${dir}`);
    else rmSync(dir, { recursive: true, force: true });
  };

  try {
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
      RUST_LOG: 'warn',
    });
    const wrap = withHostname(DEMO.host);
    const command = (args: string[]): [string, string[]] => (wrap.length ? [wrap[0]!, [...wrap.slice(1), bin, ...args]] : [bin, args]);
    const cl = (...args: string[]) => execFileSync(...command(args), { env, encoding: 'utf8', stdio: ['ignore', 'pipe', 'pipe'] });

    const live = writeDemo(join(home, 'logs'), Date.now() / 1000);
    cl('index', '--once');
    const token = /\(shown once\): (\S+)/.exec(cl('token', 'new', DEMO.user))?.[1];
    if (!token) throw new Error('cinelog token new printed no token');
    const hit = cl('search', '--json', 'make release').split('\n').find(Boolean);
    if (!hit) throw new Error('the legacy demo cast was not imported');
    const legacy = String(JSON.parse(hit).entry).split(':')[0]!;

    children.push(registerLive(join(run, 'cinelog'), DEMO.live, live.path));
    const logFd = openSync(join(dir, 'serve.log'), 'a');
    const serve = spawn(...command(['serve', '--scope', 'personal']), { env, stdio: ['ignore', logFd, logFd] });
    children.push(serve);

    const info = join(run, 'cinelog/server.json');
    let url = '';
    for (let i = 0; i < 200 && !url && serve.exitCode === null; i++) {
      try {
        url = String(JSON.parse(readFileSync(info, 'utf8')).url);
      } catch {
        await sleep(50);
      }
    }
    if (!url) throw new Error(`cinelog serve did not start:\n${readFileSync(join(dir, 'serve.log'), 'utf8')}`);
    return { url: `https://127.0.0.1:${new URL(url).port}`, token, legacy, close };
  } catch (e) {
    await close();
    throw e;
  }
}
