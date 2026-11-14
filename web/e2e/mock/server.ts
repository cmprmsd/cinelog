// A stand-in for `cinelog serve` built from the API contract (docs/API.md) and the generated
// types: HTTPS with a throwaway self-signed certificate, the same security headers and request
// checks, cookie sessions bound to the User-Agent, and the endpoints tab 1 uses, over the
// fixtures in `fixtures.ts`. The SPA is served from `dist/` (run `pnpm build` first).
//
// Standalone: `node e2e/mock/server.ts [port]` prints the URL and the token.

import { execFileSync } from 'node:child_process';
import { createHash, randomBytes, X509Certificate } from 'node:crypto';
import { mkdtempSync, readFileSync, rmSync, statSync } from 'node:fs';
import { createServer, type Server } from 'node:https';
import type { IncomingMessage, ServerResponse } from 'node:http';
import type { AddressInfo } from 'node:net';
import { tmpdir } from 'node:os';
import { extname, join, normalize, resolve, sep } from 'node:path';
import { fileURLToPath } from 'node:url';
import type {
  ApiError,
  BusEvent,
  BusHello,
  CommandList,
  ErrorCode,
  ForgetTarget,
  Forgotten,
  LoginInfo,
  MaskedSpan,
  Me,
  PlayIndex,
  PlayInfo,
  PlaySegment,
  SearchHit,
  SearchResponse,
  Session,
  SessionDetail,
  SessionPage,
  SnippetPart,
  Status,
  Tree,
  TreeHost,
  TreeNode,
} from '../../src/lib/api/types.ts';
import { buildFixtures, FIX, OUTPUT_DELAY, sessionEvents, toCommand, type CastEvent, type FixSession } from './fixtures.ts';
import { handle as annotationsMock, liveAnnotations, parseTagList } from './m4/annotations.ts';
import type { MockContext, MockHandler } from './m4/context.ts';
import { handle as liveMock, recorderOf } from './m4/live.ts';
import { handle as timelineMock } from './m4/timeline.ts';

/** M4 handlers, each owned by its work package (docs/API.md §9.7). */
const M4_HANDLERS: readonly MockHandler[] = [liveMock, timelineMock, annotationsMock];

/** Same policy as `crates/cinelog-web/src/headers.rs`, so the player is tested under it. */
export const CSP =
  "default-src 'self'; script-src 'self' 'wasm-unsafe-eval'; style-src 'self' 'unsafe-inline'; " +
  "img-src 'self' data:; font-src 'self' data:; connect-src 'self'; worker-src 'self'; object-src 'none'; " +
  "base-uri 'none'; form-action 'self'; frame-ancestors 'none'";

export const MOCK_TOKEN = 'MFRGGZDFMZTWQ2LKNNWG23TPOBYXE43UOV3HO6DZPIYTEMZUGU3A';
const VERSION = '0.12.0-mock';
const PRINCIPAL = 'alice';
const ALLOWED_HOSTS = new Set(['localhost', '127.0.0.1', '::1']);
const LOGIN_LIMIT = 5;

export interface MockServer {
  url: string;
  token: string;
  close(): Promise<void>;
}

interface AuthSession {
  principal: string;
  uaHash: string;
  expires: number;
}

class HttpError extends Error {
  readonly status: number;
  readonly code: ErrorCode;
  constructor(status: number, code: ErrorCode, message: string) {
    super(message);
    this.status = status;
    this.code = code;
  }
}

const MIME: Record<string, string> = {
  '.html': 'text/html; charset=utf-8',
  '.js': 'text/javascript; charset=utf-8',
  '.css': 'text/css; charset=utf-8',
  '.svg': 'image/svg+xml',
  '.json': 'application/json',
  '.wasm': 'application/wasm',
  '.png': 'image/png',
  '.woff2': 'font/woff2',
};

const sha256 = (s: string) => createHash('sha256').update(s).digest('hex');

/** The `cwd` filter: the directory itself or anything below it (whole path components). */
function underDir(cwd: string, dir: string): boolean {
  const base = dir.replace(/\/+$/, '');
  return cwd === dir || cwd === base || cwd.startsWith(`${base}/`);
}

function selfSigned(): { key: Buffer; cert: Buffer; dir: string } {
  const dir = mkdtempSync(join(tmpdir(), 'cinelog-e2e-'));
  const key = join(dir, 'key.pem');
  const cert = join(dir, 'cert.pem');
  execFileSync(
    'openssl',
    ['req', '-x509', '-newkey', 'ec', '-pkeyopt', 'ec_paramgen_curve:prime256v1', '-nodes', '-days', '2', '-subj', '/CN=localhost',
      '-addext', 'subjectAltName=DNS:localhost,IP:127.0.0.1,IP:::1', '-keyout', key, '-out', cert],
    { stdio: 'ignore' },
  );
  return { key: readFileSync(key), cert: readFileSync(cert), dir };
}

/** Splits `text` on case-(in)sensitive matches of `q` into snippet parts. */
export function markMatches(text: string, q: string, caseSensitive: boolean): SnippetPart[] {
  if (!q) return [{ text, hit: false }];
  const hay = caseSensitive ? text : text.toLowerCase();
  const needle = caseSensitive ? q : q.toLowerCase();
  const parts: SnippetPart[] = [];
  let at = 0;
  for (let i = hay.indexOf(needle); i >= 0; i = hay.indexOf(needle, i + needle.length)) {
    if (i > at) parts.push({ text: text.slice(at, i), hit: false });
    parts.push({ text: text.slice(i, i + needle.length), hit: true });
    at = i + needle.length;
  }
  if (at < text.length) parts.push({ text: text.slice(at), hit: false });
  return parts.length ? parts : [{ text, hit: false }];
}

export async function startMock(opts: { port?: number; dist?: string } = {}): Promise<MockServer> {
  const here = fileURLToPath(new URL('.', import.meta.url));
  const dist = resolve(opts.dist ?? join(here, '..', '..', 'dist'));
  const tls = selfSigned();
  const fingerprint = new X509Certificate(tls.cert).fingerprint256;
  const startedAt = Date.now() / 1000;
  const fixtures = buildFixtures(startedAt);
  const byId = new Map(fixtures.map((f) => [f.session.uuid, f]));
  const sessions = new Map<string, AuthSession>();
  const loginAttempts: number[] = [];
  const forgotten: Forgotten[] = [];
  const busClients = new Set<ServerResponse>();
  let port = 0;

  const isForgottenCmd = (uuid: string, seq: number) =>
    forgotten.find((f) => f.target.uuid === uuid && f.target.kind === 'command' && f.target.seq === seq) ?? null;
  const maskFor = (f: FixSession) => (seq: number) => isForgottenCmd(f.session.uuid, seq)?.by ?? null;
  const sessionForgotten = (uuid: string) => forgotten.some((f) => f.target.kind === 'session' && f.target.uuid === uuid);

  function visible(): FixSession[] {
    return fixtures.filter((f) => !sessionForgotten(f.session.uuid));
  }

  function find(uuid: string): FixSession {
    const f = byId.get(uuid.toLowerCase());
    if (!f || sessionForgotten(f.session.uuid)) throw new HttpError(404, 'not_found', 'no such session');
    return f;
  }

  // ---- UI bus (docs/API.md §9.2) ---------------------------------------------------------

  function publish(event: BusEvent): void {
    const frame = `event: ${event.type}\ndata: ${JSON.stringify(event.data)}\n\n`;
    for (const res of busClients) res.write(frame);
  }

  function openBus(req: IncomingMessage, res: ServerResponse): void {
    res.writeHead(200, { 'Content-Type': 'text/event-stream', 'Cache-Control': 'no-store', 'X-Accel-Buffering': 'no' });
    const hello: BusHello = {
      version: VERSION,
      server_started_at: startedAt,
      live: visible()
        .filter((f) => f.session.state === 'live')
        .map((f) => recorderOf(f.session.uuid) ?? { session: f.session, status: 'recording', reason: null, detail: null, since: null, child_scope: null, child_url: null }),
    };
    res.write(`event: hello\ndata: ${JSON.stringify(hello)}\n\n`);
    busClients.add(res);
    const ping = setInterval(() => res.write(': keep-alive\n\n'), 15_000);
    req.on('close', () => {
      clearInterval(ping);
      busClients.delete(res);
    });
  }

  // ---- search ----------------------------------------------------------------------------

  function search(sp: URLSearchParams): SearchResponse {
    const q = sp.get('q') ?? '';
    const mode = sp.get('in') ?? 'cmd';
    if (!['cmd', 'out', 'both'].includes(mode)) throw new HttpError(400, 'bad_request', 'in must be cmd, out or both');
    const cs = sp.get('case') === 'true';
    const limit = Math.min(500, Math.max(1, Number(sp.get('limit') ?? 50) || 50));
    const offset = Number(sp.get('cursor') ?? 0) || 0;
    const exit = sp.get('exit');
    const from = sp.get('from') !== null ? Number(sp.get('from')) : null;
    const to = sp.get('to') !== null ? Number(sp.get('to')) : null;
    const match = (s: string | null) => s !== null && (cs ? s.includes(q) : s.toLowerCase().includes(q.toLowerCase()));
    // Tag filter (docs/API.md §5.4): one annotation carrying every tag, on the command or a
    // span of the session over the hit; hit tags are those of the command's annotations.
    const tagFilter = parseTagList(m4, sp.get('tag'));
    const notes = liveAnnotations(m4);
    const tagged = notes.filter((a) => tagFilter.every((t) => a.tags.includes(t)));
    const tagOk = (uuid: string, seq: number, from: number, to: number) =>
      !tagFilter.length ||
      tagged.some(
        (a) => a.session === uuid && (a.cmd_seq === seq || (a.cmd_seq === null && a.t_from !== null && a.t_to !== null && a.t_from <= to && a.t_to >= from)),
      );
    const tagsOf = (uuid: string, seq: number) =>
      [...new Set(notes.filter((a) => a.session === uuid && a.cmd_seq === seq).flatMap((a) => a.tags))].sort();
    const hits: SearchHit[] = [];
    for (const f of visible()) {
      const s = f.session;
      if (s.hidden && sp.get('hidden') !== 'true') continue;
      if (sp.get('user') && s.user !== sp.get('user')) continue;
      if (sp.get('host') && s.host !== sp.get('host')) continue;
      if (sp.get('pane') && s.tmux.pane !== sp.get('pane')) continue;
      if (sp.get('session') && s.uuid !== sp.get('session')!.toLowerCase()) continue;
      for (const c of f.commands) {
        if (isForgottenCmd(s.uuid, c.seq)) continue;
        if (sp.get('cwd') && !underDir(c.cwd, sp.get('cwd')!)) continue;
        if (exit === 'ok' && c.exit !== 0) continue;
        if (exit === 'fail' && (c.exit === null || c.exit === 0)) continue;
        if (exit && exit !== 'ok' && exit !== 'fail' && c.exit !== Number(exit)) continue;
        const ts = s.started_at + c.t_start;
        if ((from !== null && ts < from) || (to !== null && ts > to)) continue;
        const common = {
          session: s.uuid,
          cmd_seq: c.seq,
          ts,
          user: s.user,
          host: s.host,
          pane: s.tmux.pane,
          tmux_session: s.tmux.session,
          exit: c.exit,
          cwd: c.cwd,
          typed: c.typed,
          approx: c.approx ?? false,
          confidence: c.confidence ?? null,
          legacy: s.legacy,
          tags: tagsOf(s.uuid, c.seq),
        };
        if (mode !== 'out' && (!q || match(c.typed) || match(c.cwd)) && tagOk(s.uuid, c.seq, c.t_start, c.t_end)) {
          hits.push({ ...common, id: `c:${s.uuid}:${c.seq}`, kind: 'command', t: c.t_start, snippet: markMatches(c.typed, q, cs), chunk_kind: null });
        }
        const out = c.output.join('\n');
        const tOut = c.t_start + OUTPUT_DELAY;
        if (mode !== 'cmd' && q && match(out) && tagOk(s.uuid, c.seq, tOut, tOut)) {
          hits.push({
            ...common,
            id: `o:${s.uuid}:${c.seq}`,
            kind: 'output',
            t: c.t_start + OUTPUT_DELAY,
            ts: ts + OUTPUT_DELAY,
            snippet: markMatches(out, q, cs),
            chunk_kind: 'primary',
          });
        }
      }
    }
    hits.sort((a, b) => b.ts - a.ts || a.id.localeCompare(b.id));
    const page = hits.slice(offset, offset + limit);
    return {
      hits: page,
      next: offset + limit < hits.length ? String(offset + limit) : null,
      partial: q.length > 0 && [...q].length < 3,
      capped: false,
    };
  }

  // ---- sessions and tree -----------------------------------------------------------------

  function sessionList(sp: URLSearchParams): SessionPage {
    let list = visible().map((f) => f.session);
    const live = sp.get('live');
    if (live === 'true') list = list.filter((s) => s.state === 'live');
    if (live === 'false') list = list.filter((s) => s.state !== 'live');
    if (sp.get('hidden') !== 'true') list = list.filter((s) => !s.hidden);
    list.sort((a, b) => b.started_at - a.started_at);
    const limit = Math.min(500, Math.max(1, Number(sp.get('limit') ?? 100) || 100));
    return { sessions: list.slice(0, limit), next: null };
  }

  function tree(sp: URLSearchParams): Tree {
    const from = sp.get('from') !== null ? Number(sp.get('from')) : null;
    const list = visible()
      .map((f) => f.session)
      .filter((s) => from === null || (s.ended_at ?? Infinity) >= from)
      .sort((a, b) => b.started_at - a.started_at);
    const inResponse = new Set(list.map((s) => s.uuid));
    const node = (s: Session): TreeNode => ({
      uuid: s.uuid,
      user: s.user,
      state: s.state,
      started_at: s.started_at,
      ended_at: s.ended_at,
      legacy: s.legacy,
      hidden: s.hidden,
      commands: s.commands,
      sudo_user: s.sudo_user || null,
      attached: [],
      children: list.filter((c) => c.parent_uuid === s.uuid).reverse().map(node),
    });
    const count = (nodes: TreeNode[]): number => nodes.reduce((n, x) => n + 1 + count(x.children), 0);
    const hosts = new Map<string, TreeHost>();
    for (const s of list) {
      if (s.parent_uuid && inResponse.has(s.parent_uuid)) continue;
      let h = hosts.get(s.host);
      if (!h) hosts.set(s.host, (h = { host: s.host, tmux_servers: [], shells: [] }));
      const t = s.tmux;
      if (t.server && t.pane) {
        let srv = h.tmux_servers.find((x) => x.server === t.server);
        if (!srv) h.tmux_servers.push((srv = { server: t.server, first_at: s.started_at, live: false, sessions: [] }));
        let ts = srv.sessions.find((x) => x.session_id === t.session_id);
        if (!ts) srv.sessions.push((ts = { session_id: t.session_id, name: t.session, live: false, recordings: 0, attached_from: [], windows: [] }));
        let w = ts.windows.find((x) => x.window === t.window);
        if (!w) ts.windows.push((w = { window: t.window, name: t.window_name, panes: [] }));
        let p = w.panes.find((x) => x.pane === t.pane);
        if (!p) w.panes.push((p = { pane: t.pane, index: t.pane_index, sessions: [] }));
        p.sessions.push(node(s));
        srv.first_at = Math.min(srv.first_at, s.started_at);
        ts.live ||= s.state === 'live';
        srv.live ||= ts.live;
        ts.recordings = ts.windows.reduce((n, x) => n + x.panes.reduce((k, y) => k + count(y.sessions), 0), 0);
      } else {
        let g = h.shells.find((x) => x.holder_comm === s.holder_comm);
        if (!g) h.shells.push((g = { holder_comm: s.holder_comm, sessions: [] }));
        g.sessions.push(node(s));
      }
    }
    for (const h of hosts.values()) {
      for (const srv of h.tmux_servers) for (const ts of srv.sessions) ts.windows.sort((a, b) => Number(a.window) - Number(b.window));
      h.shells.sort((a, b) => (a.holder_comm === null ? 1 : b.holder_comm === null ? -1 : a.holder_comm.localeCompare(b.holder_comm)));
    }
    return { hosts: [...hosts.values()].sort((a, b) => a.host.localeCompare(b.host)) };
  }

  function ref(s: Session) {
    return { uuid: s.uuid, user: s.user, scope: s.scope, state: s.state, started_at: s.started_at, ended_at: s.ended_at, holder_comm: s.holder_comm };
  }

  /** What playback masks, as the server reports it: a command from the end of the one before. */
  function masked(f: FixSession): MaskedSpan[] {
    const out: MaskedSpan[] = [];
    for (const x of forgotten) {
      const tg = x.target;
      if (tg.uuid !== f.session.uuid) continue;
      if (tg.kind === 'range') out.push({ t_from: tg.t_from, t_to: tg.t_to, by: x.by });
      if (tg.kind !== 'command') continue;
      const i = f.commands.findIndex((c) => c.seq === tg.seq);
      if (i >= 0) out.push({ t_from: i > 0 ? f.commands[i - 1]!.t_end : 0, t_to: f.commands[i]!.t_end, by: x.by });
    }
    return out.sort((a, b) => a.t_from - b.t_from);
  }

  function detail(f: FixSession): SessionDetail {
    const parent = f.session.parent_uuid ? byId.get(f.session.parent_uuid) : undefined;
    return {
      session: f.session,
      parent: parent ? ref(parent.session) : null,
      children: visible()
        .filter((c) => c.session.parent_uuid === f.session.uuid)
        .map((c) => ref(c.session)),
      markers: f.markers,
      parts: [{ part: 1, compressed: false, bytes: f.session.cast_bytes, t_offset: 0 }],
      forgotten: forgotten.filter((x) => x.target.uuid === f.session.uuid),
      masked: masked(f),
      play_mode: f.segments ? 'segmented' : 'cast',
    };
  }

  // ---- playback --------------------------------------------------------------------------

  function castText(f: FixSession, events: CastEvent[], startAt = 0): string {
    const header = {
      version: 3,
      term: { cols: f.session.cols, rows: f.session.rows, type: f.session.term },
      timestamp: Math.floor(f.session.started_at + startAt),
    };
    let prev = startAt;
    const lines = [JSON.stringify(header)];
    for (const [t, code, data] of events) {
      lines.push(JSON.stringify([Math.round((t - prev) * 1e6) / 1e6, code, data]));
      prev = t;
    }
    return lines.join('\n') + '\n';
  }

  function playIndex(f: FixSession): PlayIndex {
    return {
      version: 1,
      duration: f.duration,
      term: { cols: f.session.cols, rows: f.session.rows, theme: null },
      markers: f.commands.map((c) => [c.t_start, `$ ${c.typed}`]),
      segments: f.segments!.map((start, n) => ({ start, url: `seg/${n}` })),
    };
  }

  function playSegment(f: FixSession, n: number): PlaySegment {
    const starts = f.segments!;
    if (!Number.isInteger(n) || n < 0 || n >= starts.length) throw new HttpError(404, 'not_found', 'no such segment');
    const start = starts[n]!;
    const end = starts[n + 1];
    const all = sessionEvents(f, maskFor(f));
    const events = all.filter(([t]) => t >= start && (end === undefined ? t <= f.duration : t < end));
    if (n > 0 && events[0]?.[0] !== start) events.unshift([start, 'o', '']);
    // The terminal at the segment start: what the earlier events printed (a stand-in for
    // the server's avt dump).
    const before = all.filter(([t, code]) => t < start && code === 'o').map(([, , d]) => d);
    return { snapshot: { cols: f.session.cols, rows: f.session.rows, init: before.join('') }, events };
  }

  function exportBody(f: FixSession, sp: URLSearchParams): { body: string; type: string; name: string } {
    const format = sp.get('format');
    let from = 0;
    let to = f.duration;
    const cmd = sp.get('cmd');
    if (cmd !== null) {
      const c = f.commands.find((x) => x.seq === Number(cmd));
      if (!c) throw new HttpError(404, 'not_found', 'no such command');
      from = c.t_start;
      to = c.t_end;
    } else {
      if (sp.get('from') !== null) from = Number(sp.get('from'));
      if (sp.get('to') !== null) to = Number(sp.get('to'));
    }
    const events = sessionEvents(f, maskFor(f)).filter(([t]) => t >= from && t <= to);
    const name = `cinelog-${f.session.uuid.slice(0, 8)}-${Math.floor(from)}-${Math.ceil(to)}.${format}`;
    const text = events
      .filter(([, code]) => code === 'o')
      .map(([, , d]) => d)
      .join('')
      // eslint-disable-next-line no-control-regex -- strips SGR sequences for the text form
      .replace(/\x1b\[[0-9;]*m/g, '');
    switch (format) {
      case 'cast':
        return { body: castText(f, [[from, 'o', ''], ...events.filter(([t]) => t > from)], from), type: 'application/x-asciicast', name };
      case 'txt':
        return { body: text, type: 'text/plain; charset=utf-8', name };
      case 'html': {
        const esc = text.replace(/[&<>"']/g, (c) => `&#${c.charCodeAt(0)};`);
        const body =
          `<!doctype html><html><head><meta charset="utf-8">` +
          `<meta http-equiv="Content-Security-Policy" content="default-src 'none'; style-src 'unsafe-inline'">` +
          `<title>cinelog export</title></head><body><pre>${esc}</pre></body></html>`;
        return { body, type: 'text/html; charset=utf-8', name };
      }
      default:
        throw new HttpError(400, 'bad_request', 'format must be cast, txt or html');
    }
  }

  function forget(body: unknown, by: string): Forgotten {
    const b = (body ?? {}) as Record<string, unknown>;
    const uuid = typeof b.session_uuid === 'string' ? b.session_uuid.toLowerCase() : '';
    find(uuid);
    let target: ForgetTarget;
    const n = (v: unknown) => typeof v === 'number' && Number.isFinite(v);
    if (b.cmd_seq === undefined && b.t_from === undefined && b.t_to === undefined) target = { kind: 'session', uuid };
    else if (n(b.cmd_seq) && b.t_from === undefined && b.t_to === undefined) target = { kind: 'command', uuid, seq: b.cmd_seq as number };
    else if (b.cmd_seq === undefined && n(b.t_from) && n(b.t_to)) target = { kind: 'range', uuid, t_from: b.t_from as number, t_to: b.t_to as number };
    else throw new HttpError(400, 'bad_request', 'session_uuid with cmd_seq, or with t_from and t_to');
    const existing = forgotten.find((x) => JSON.stringify(x.target) === JSON.stringify(target));
    if (existing) return existing;
    const row: Forgotten = { target, by, at: Date.now() / 1000 };
    forgotten.push(row);
    publish({ type: 'forgotten', data: row });
    return row;
  }

  // ---- HTTP plumbing ---------------------------------------------------------------------

  function cookieName(): string {
    return `__Host-cl-${port}`;
  }

  function principalOf(req: IncomingMessage): string {
    const raw = req.headers.cookie ?? '';
    const id = raw
      .split(';')
      .map((c) => c.trim().split('='))
      .find(([k]) => k === cookieName())?.[1];
    const s = id ? sessions.get(sha256(id)) : undefined;
    if (!id || !s || s.expires < Date.now() / 1000) throw new HttpError(401, 'unauthorized', 'sign in first');
    if (s.uaHash !== sha256(req.headers['user-agent'] ?? '')) {
      sessions.delete(sha256(id));
      throw new HttpError(401, 'unauthorized', 'session rejected');
    }
    return s.principal;
  }

  async function readJson(req: IncomingMessage): Promise<unknown> {
    const chunks: Buffer[] = [];
    for await (const c of req) chunks.push(c as Buffer);
    const text = Buffer.concat(chunks).toString('utf8');
    if (!text) return undefined;
    try {
      return JSON.parse(text);
    } catch {
      throw new HttpError(400, 'bad_request', 'invalid JSON');
    }
  }

  function json(res: ServerResponse, status: number, body: unknown): void {
    res.writeHead(status, { 'Content-Type': 'application/json' });
    res.end(JSON.stringify(body));
  }

  function serveStatic(res: ServerResponse, path: string): void {
    let rel: string;
    try {
      rel = normalize(decodeURIComponent(path)).replace(/^([/\\])+/, '');
    } catch {
      throw new HttpError(400, 'bad_request', 'bad path');
    }
    let file = resolve(dist, rel);
    if (file !== dist && !file.startsWith(dist + sep)) throw new HttpError(404, 'not_found', 'not found');
    let isFile: boolean;
    try {
      isFile = statSync(file).isFile();
    } catch {
      isFile = false;
    }
    if (!isFile) {
      if (extname(rel)) throw new HttpError(404, 'not_found', 'not found');
      file = join(dist, 'index.html');
    }
    let data: Buffer;
    try {
      data = readFileSync(file);
    } catch {
      throw new HttpError(503, 'unavailable', 'web/dist is missing; run pnpm build');
    }
    const immutable = rel.startsWith('assets/');
    res.writeHead(200, {
      'Content-Type': MIME[extname(file)] ?? 'application/octet-stream',
      'Cache-Control': immutable ? 'public, max-age=31536000, immutable' : 'no-cache',
    });
    res.end(data);
  }

  async function route(req: IncomingMessage, res: ServerResponse): Promise<void> {
    const authority = req.headers[':authority'] ?? req.headers.host ?? '';
    const host = String(authority).replace(/^\[(.*)\](:\d+)?$/, '$1').replace(/:\d+$/, '').toLowerCase();
    if (!ALLOWED_HOSTS.has(host)) throw new HttpError(400, 'bad_request', 'host not allowed');
    const url = new URL(req.url ?? '/', `https://${authority}`);
    const path = url.pathname;
    const method = req.method ?? 'GET';
    const mutating = !['GET', 'HEAD', 'OPTIONS'].includes(method);
    if (mutating) {
      if (req.headers.origin !== `https://${authority}`) throw new HttpError(403, 'forbidden', 'origin mismatch');
      // Like the real server (hosts.rs `origin_problem`), only a request with a body must be JSON.
      const hasBody = Number(req.headers['content-length'] ?? 0) > 0 || req.headers['transfer-encoding'] !== undefined;
      if (hasBody && !String(req.headers['content-type'] ?? '').startsWith('application/json'))
        throw new HttpError(415, 'unsupported_media_type', 'expected application/json');
    }

    if (path === '/healthz') return json(res, 200, { version: VERSION });
    const link = /^\/link\/([^/]+)$/.exec(path);
    if (link) {
      if (!/^[0-9a-f-]{36}$/i.test(link[1]!)) throw new HttpError(404, 'not_found', 'not a uuid');
      res.writeHead(303, { Location: `/s/${link[1]!.toLowerCase()}` });
      return void res.end();
    }
    if (!path.startsWith('/api/') && path !== '/api') {
      if (method !== 'GET' && method !== 'HEAD') throw new HttpError(405, 'method_not_allowed', 'method not allowed');
      return serveStatic(res, path);
    }

    if (path === '/api/login' && method === 'GET') {
      const info: LoginInfo = { version: VERSION, scope: 'personal', fingerprint };
      return json(res, 200, info);
    }
    if (path === '/api/login' && method === 'POST') {
      const now = Date.now();
      while (loginAttempts.length && loginAttempts[0]! < now - 60_000) loginAttempts.shift();
      if (loginAttempts.length >= LOGIN_LIMIT) {
        res.setHeader('Retry-After', String(Math.ceil((loginAttempts[0]! + 60_000 - now) / 1000)));
        throw new HttpError(429, 'too_many_requests', 'too many login attempts');
      }
      loginAttempts.push(now);
      const body = (await readJson(req)) as { token?: unknown } | undefined;
      const token = typeof body?.token === 'string' ? body.token.trim().toUpperCase() : '';
      if (token !== MOCK_TOKEN) throw new HttpError(401, 'unauthorized', 'wrong token');
      const id = randomBytes(32).toString('base64url');
      const maxAge = 7 * 86400;
      sessions.set(sha256(id), { principal: PRINCIPAL, uaHash: sha256(req.headers['user-agent'] ?? ''), expires: now / 1000 + maxAge });
      res.writeHead(204, { 'Set-Cookie': `${cookieName()}=${id}; HttpOnly; Secure; SameSite=Strict; Path=/; Max-Age=${maxAge}` });
      return void res.end();
    }

    const principal = principalOf(req);
    if (path === '/api/logout' && method === 'POST') {
      const id = (req.headers.cookie ?? '').split(';').map((c) => c.trim().split('=')).find(([k]) => k === cookieName())?.[1];
      if (id) sessions.delete(sha256(id));
      res.writeHead(204, { 'Set-Cookie': `${cookieName()}=; HttpOnly; Secure; SameSite=Strict; Path=/; Max-Age=0` });
      return void res.end();
    }
    if (method === 'POST' && path === '/api/forget') return json(res, 200, forget(await readJson(req), principal));
    if (method === 'GET' && path === '/api/events') return openBus(req, res);
    for (const h of M4_HANDLERS) if (await h(m4, { req, res, url, method, principal })) return;
    if (method !== 'GET') throw new HttpError(405, 'method_not_allowed', 'method not allowed');

    const sp = url.searchParams;
    if (path === '/api/me') {
      const me: Me = { principal, scope: 'personal', host: FIX.host, version: VERSION, protocol: 1, fingerprint, expires_at: Date.now() / 1000 + 43200 };
      return json(res, 200, me);
    }
    if (path === '/api/status') return json(res, 200, status());
    if (path === '/api/sessions') return json(res, 200, sessionList(sp));
    if (path === '/api/tree') return json(res, 200, tree(sp));
    if (path === '/api/search') return json(res, 200, search(sp));

    const m = /^\/api\/sessions\/([^/]+)(\/.*)?$/.exec(path);
    if (m) {
      const f = find(decodeURIComponent(m[1]!));
      const rest = m[2] ?? '';
      if (rest === '') return json(res, 200, detail(f));
      if (rest === '/commands') {
        const list: CommandList = { session: f.session.uuid, commands: f.commands.map((c) => toCommand(f, c)) };
        return json(res, 200, list);
      }
      if (rest === '/play') {
        const base = `/api/sessions/${f.session.uuid}`;
        const info: PlayInfo = {
          mode: f.segments ? 'segmented' : 'cast',
          url: f.segments ? `${base}/play/index.json` : `${base}/cast`,
          ready: true,
          progress: null,
          duration: f.duration,
          cols: f.session.cols,
          rows: f.session.rows,
        };
        return json(res, 200, info);
      }
      if (rest === '/cast') {
        if (f.segments) throw new HttpError(413, 'payload_too_large', 'use segmented playback');
        res.writeHead(200, { 'Content-Type': 'application/x-asciicast' });
        return void res.end(castText(f, sessionEvents(f, maskFor(f))));
      }
      if (rest === '/play/index.json' && f.segments) return json(res, 200, playIndex(f));
      const seg = /^\/play\/seg\/(\d+)$/.exec(rest);
      if (seg && f.segments) return json(res, 200, playSegment(f, Number(seg[1])));
      if (rest === '/export') {
        const { body, type, name } = exportBody(f, sp);
        res.writeHead(200, { 'Content-Type': type, 'Content-Disposition': `attachment; filename="${name}"` });
        return void res.end(body);
      }
    }
    throw new HttpError(404, 'not_found', 'not found');
  }

  function status(): Status {
    const all = visible();
    const count = (st: string) => all.filter((f) => f.session.state === st).length;
    return {
      version: VERSION,
      protocol: 1,
      scope: 'personal',
      host: FIX.host,
      server_started_at: startedAt,
      port,
      fingerprint,
      schema_version: 1,
      index_bytes: 1 << 20,
      sessions: { live: count('live'), ended: count('ended'), crashed: 0, degraded: 0, error: 0, legacy: all.filter((f) => f.session.legacy).length, hidden: 0 },
      commands: all.reduce((n, f) => n + f.commands.length, 0),
      outputs: all.reduce((n, f) => n + f.commands.length, 0),
      live: [],
      backlog_bytes: 0,
      legacy: { files: 1, imported: 1, hidden: 0, skipped: 0, failed: 0, banners: 2, log_lines: 2, aligned: 2 },
      last_error: null,
    };
  }

  const m4: MockContext = {
    fixtures,
    visible,
    find,
    fail: (status, code, message) => {
      throw new HttpError(status, code, message);
    },
    readJson,
    json,
    publish,
    maskFor,
    startedAt,
  };

  const server: Server = createServer({ key: tls.key, cert: tls.cert }, (req, res) => {
    res.setHeader('Content-Security-Policy', CSP);
    res.setHeader('X-Frame-Options', 'DENY');
    res.setHeader('Referrer-Policy', 'no-referrer');
    res.setHeader('X-Content-Type-Options', 'nosniff');
    res.setHeader('Cross-Origin-Opener-Policy', 'same-origin');
    const p = (req.url ?? '/').split('?')[0]!;
    if (p === '/healthz' || p === '/api' || p.startsWith('/api/') || p.startsWith('/link/')) res.setHeader('Cache-Control', 'no-store');
    route(req, res).catch((e: unknown) => {
      if (res.headersSent) return void res.end();
      const err = e instanceof HttpError ? e : new HttpError(500, 'internal', 'internal error');
      if (!(e instanceof HttpError)) console.error('mock server:', e);
      const body: ApiError = { error: err.code, message: err.message };
      json(res, err.status, body);
    });
  });

  await new Promise<void>((ok, fail) => {
    server.once('error', fail);
    server.listen(opts.port ?? 0, '127.0.0.1', () => ok());
  });
  port = (server.address() as AddressInfo).port;
  return {
    url: `https://127.0.0.1:${port}`,
    token: MOCK_TOKEN,
    close: () =>
      new Promise<void>((ok) => {
        server.closeAllConnections();
        server.close(() => {
          rmSync(tls.dir, { recursive: true, force: true });
          ok();
        });
      }),
  };
}

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  const m = await startMock({ port: Number(process.argv[2] ?? 0) || 0 });
  console.log(`cinelog mock server: ${m.url}\ntoken: ${m.token}`);
  const stop = () => void m.close().then(() => process.exit(0));
  process.on('SIGINT', stop);
  process.on('SIGTERM', stop);
}
