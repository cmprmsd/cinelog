// Mock of `/api/annotations`, `/api/annotations/{uuid}` and `/api/tags` (work package M4-C;
// docs/API.md §9.4): in-memory annotations and tags with the server's validation, author
// rules, soft delete, cursor pages and bus events. Annotations of forgotten sessions and
// commands are treated as absent, like the server does.

import { randomBytes } from 'node:crypto';
import type {
  Annotation,
  AnnotationCreate,
  AnnotationPage,
  AnnotationPatch,
  Tag,
  TagList,
  TagRequest,
} from '../../../src/lib/api/types.ts';
import type { MockContext, MockHandler, MockRequest } from './context.ts';

const MAX_BODY = 16 * 1024;
const MAX_TAGS = 16;
const UUID_RE = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i;
const TAG_CHAR = /^[\p{Alphabetic}\p{N}\-_.:/+#@]$/u;

const annotations = new Map<string, Annotation>();
/** Tag colours by name (`null`: automatic). */
const tags = new Map<string, string | null>();
let lastMs = 0;
let counter = 0;

/** A UUIDv7 (time-ordered) like the server's. */
function uuidV7(): string {
  const ms = Math.max(Date.now(), lastMs);
  lastMs = ms;
  const b = randomBytes(16);
  b.writeUIntBE(ms, 0, 6);
  b[6] = 0x70 | (b[6]! & 0x0f);
  b[8] = 0x80 | (b[8]! & 0x3f);
  const h = b.toString('hex');
  return `${h.slice(0, 8)}-${h.slice(8, 12)}-${h.slice(12, 16)}-${h.slice(16, 20)}-${h.slice(20)}`;
}

/** Epoch seconds at µs precision, strictly increasing (stable ordering in fast specs). */
function now(): number {
  counter += 1;
  return Math.round(Date.now() * 1000 + counter) / 1e6;
}

function normalizeTag(ctx: MockContext, raw: unknown): string {
  const n = typeof raw === 'string' ? raw.trim().toLowerCase() : '';
  const chars = [...n];
  if (chars.length < 1 || chars.length > 40 || !chars.every((c) => TAG_CHAR.test(c))) {
    ctx.fail(400, 'bad_request', 'tag names are 1 to 40 letters, digits or - _ . : / + # @');
  }
  return n;
}

function normalizeTags(ctx: MockContext, raw: unknown): string[] {
  if (raw === undefined || raw === null) return [];
  if (!Array.isArray(raw)) ctx.fail(400, 'bad_request', 'tags must be a list');
  const out = [...new Set(raw.map((t) => normalizeTag(ctx, t)))].sort();
  if (out.length > MAX_TAGS) ctx.fail(400, 'bad_request', `at most ${MAX_TAGS} tags`);
  return out;
}

function normalizeBody(ctx: MockContext, raw: unknown): string {
  if (raw === undefined || raw === null) return '';
  if (typeof raw !== 'string') ctx.fail(400, 'bad_request', 'body must be text');
  const body = raw.trimEnd();
  if (Buffer.byteLength(body) > MAX_BODY) ctx.fail(400, 'bad_request', `the body is longer than ${MAX_BODY} bytes`);
  return body;
}

function normalizeColor(ctx: MockContext, raw: unknown): string | null {
  if (raw === undefined || raw === null) return null;
  const m = typeof raw === 'string' ? /^#([0-9a-f]{3}|[0-9a-f]{6})$/i.exec(raw.trim()) : null;
  if (!m) ctx.fail(400, 'bad_request', 'colours are #rgb or #rrggbb');
  const hex = m[1]!.toLowerCase();
  return `#${hex.length === 3 ? [...hex].map((c) => c + c).join('') : hex}`;
}

function tagInfo(ctx: MockContext, name: string): Tag {
  let count = 0;
  for (const a of annotations.values()) if (a.deleted_at === null && a.tags.includes(name) && !gone(ctx, a)) count += 1;
  return { name, color: tags.get(name) ?? null, count };
}

/** Creates missing tags; returns the new ones (published before the annotation). */
function ensureTags(names: readonly string[]): string[] {
  const created = names.filter((n) => !tags.has(n));
  for (const n of created) tags.set(n, null);
  return created;
}

/** Gone with what it annotates: a forgotten session or a forgotten command. */
function gone(ctx: MockContext, a: Annotation): boolean {
  const f = ctx.visible().find((x) => x.session.uuid === a.session);
  return !f || (a.cmd_seq !== null && ctx.maskFor(f)(a.cmd_seq) !== null);
}

/** The live annotation `uuid`, or 404. */
function existing(ctx: MockContext, uuid: string): Annotation {
  if (!UUID_RE.test(uuid)) ctx.fail(400, 'bad_request', 'not an annotation uuid');
  const a = annotations.get(uuid.toLowerCase());
  if (!a || a.deleted_at !== null || gone(ctx, a)) ctx.fail(404, 'not_found', 'not found');
  return a;
}

function publishChange(ctx: MockContext, op: 'created' | 'updated' | 'deleted', a: Annotation, newTags: string[]): void {
  for (const n of newTags) ctx.publish({ type: 'tag', data: tagInfo(ctx, n) });
  ctx.publish({ type: 'annotation', data: { op, annotation: { ...a, tags: [...a.tags] } } });
}

function list(ctx: MockContext, url: URL): AnnotationPage {
  const sp = url.searchParams;
  const session = sp.get('session')?.toLowerCase() ?? null;
  if (session !== null && !UUID_RE.test(session)) ctx.fail(400, 'bad_request', 'not a session uuid');
  const cmdRaw = sp.get('cmd_seq');
  const cmdSeq = cmdRaw === null ? null : Number(cmdRaw);
  if (cmdSeq !== null && (!Number.isInteger(cmdSeq) || cmdSeq < 0)) ctx.fail(400, 'bad_request', 'invalid query: cmd_seq');
  if (cmdSeq !== null && session === null) ctx.fail(400, 'bad_request', 'cmd_seq needs session');
  const tag = sp.has('tag') ? normalizeTag(ctx, sp.get('tag')) : null;
  const author = sp.get('author')?.trim() || null;
  const deleted = sp.get('deleted');
  if (deleted !== null && deleted !== 'true' && deleted !== 'false') ctx.fail(400, 'bad_request', 'invalid query: deleted');
  const limit = sp.has('limit') ? Number(sp.get('limit')) : 200;
  if (!Number.isInteger(limit) || limit < 1 || limit > 500) ctx.fail(400, 'bad_request', 'limit must be 1…500');
  let after: [number, string] | null = null;
  const cursor = sp.get('cursor');
  if (cursor) {
    const [ts, id] = cursor.split('~');
    if (!ts || !id || !Number.isFinite(Number(ts)) || !UUID_RE.test(id)) ctx.fail(400, 'bad_request', 'invalid cursor');
    after = [Number(ts), id];
  }
  const rows = [...annotations.values()]
    .filter(
      (a) =>
        (session === null || a.session === session) &&
        (cmdSeq === null || a.cmd_seq === cmdSeq) &&
        (tag === null || a.tags.includes(tag)) &&
        (author === null || a.author === author) &&
        (deleted === 'true' || a.deleted_at === null) &&
        (after === null || a.created_at > after[0] || (a.created_at === after[0] && a.uuid > after[1])),
    )
    .sort((a, b) => a.created_at - b.created_at || (a.uuid < b.uuid ? -1 : 1))
    .slice(0, limit);
  const last = rows.at(-1);
  return {
    annotations: rows.filter((a) => !gone(ctx, a)),
    next: rows.length === limit && last ? `${last.created_at}~${last.uuid}` : null,
    // The overview params (order, q, from/to, tag lists, context) come with the UI work package.
    contexts: {},
  };
}

function create(ctx: MockContext, raw: unknown, by: string): Annotation {
  const req = (raw ?? {}) as Partial<AnnotationCreate>;
  if (typeof req.session_uuid !== 'string' || !UUID_RE.test(req.session_uuid.trim())) {
    ctx.fail(400, 'bad_request', 'session_uuid is not a uuid');
  }
  const has = (v: unknown) => v !== undefined && v !== null;
  const [c, f, t] = [has(req.cmd_seq), has(req.t_from), has(req.t_to)];
  if (c && (f || t)) ctx.fail(400, 'bad_request', 'give either cmd_seq or both t_from and t_to, not both');
  if (!c && f !== t) ctx.fail(400, 'bad_request', 't_from and t_to come together');
  if (!c && !f) ctx.fail(400, 'bad_request', 'give either cmd_seq or both t_from and t_to');
  if (f) {
    const [a, b] = [req.t_from, req.t_to];
    if (typeof a !== 'number' || typeof b !== 'number' || !Number.isFinite(a) || !Number.isFinite(b) || a < 0 || a > b) {
      ctx.fail(400, 'bad_request', 't_from and t_to must be finite, non-negative and ordered');
    }
  }
  if (c && (!Number.isInteger(req.cmd_seq) || req.cmd_seq! < 0)) ctx.fail(400, 'bad_request', 'invalid cmd_seq');
  const body = normalizeBody(ctx, req.body);
  const tagList = normalizeTags(ctx, req.tags);
  if (!body && !tagList.length) ctx.fail(400, 'bad_request', 'an annotation needs a body or a tag');
  const f0 = ctx.find(req.session_uuid.trim());
  if (c && (!f0.commands.some((x) => x.seq === req.cmd_seq) || ctx.maskFor(f0)(req.cmd_seq!) !== null))
    ctx.fail(404, 'not_found', 'not found');
  const at = now();
  const a: Annotation = {
    uuid: uuidV7(),
    session: f0.session.uuid,
    cmd_seq: c ? req.cmd_seq! : null,
    t_from: f ? req.t_from! : null,
    t_to: f ? req.t_to! : null,
    author: by,
    body,
    tags: tagList,
    created_at: at,
    updated_at: at,
    deleted_at: null,
  };
  const created = ensureTags(tagList);
  annotations.set(a.uuid, a);
  publishChange(ctx, 'created', a, created);
  return a;
}

function update(ctx: MockContext, uuid: string, raw: unknown, by: string): Annotation {
  const req = (raw ?? {}) as AnnotationPatch;
  const addRaw = req.add_tags;
  if (req.body === undefined && req.tags === undefined && (!Array.isArray(addRaw) || addRaw.length === 0)) {
    ctx.fail(400, 'bad_request', 'nothing to change');
  }
  const body = req.body === undefined ? undefined : normalizeBody(ctx, req.body);
  const replace = req.tags === undefined ? undefined : normalizeTags(ctx, req.tags);
  const add = normalizeTags(ctx, addRaw);
  const a = existing(ctx, uuid);
  if ((body !== undefined || replace !== undefined) && a.author !== by) {
    ctx.fail(403, 'forbidden', 'only the author can edit this annotation');
  }
  const nextTags = [...new Set([...(replace ?? a.tags), ...add])].sort();
  if (nextTags.length > MAX_TAGS) ctx.fail(400, 'bad_request', `at most ${MAX_TAGS} tags`);
  const nextBody = body ?? a.body;
  if (!nextBody && !nextTags.length) ctx.fail(400, 'bad_request', 'an annotation needs a body or a tag');
  const created = ensureTags(nextTags);
  const out: Annotation = { ...a, body: nextBody, tags: nextTags, updated_at: now() };
  annotations.set(out.uuid, out);
  publishChange(ctx, 'updated', out, created);
  return out;
}

function remove(ctx: MockContext, uuid: string, by: string): void {
  const a = existing(ctx, uuid);
  if (a.author !== by) ctx.fail(403, 'forbidden', 'only the author can delete this annotation');
  const at = now();
  const out: Annotation = { ...a, deleted_at: at, updated_at: at };
  annotations.set(out.uuid, out);
  publishChange(ctx, 'deleted', out, []);
}

function putTag(ctx: MockContext, raw: unknown): Tag {
  const req = (raw ?? {}) as Partial<TagRequest>;
  const name = normalizeTag(ctx, req.name);
  tags.set(name, normalizeColor(ctx, req.color));
  const t = tagInfo(ctx, name);
  ctx.publish({ type: 'tag', data: t });
  return t;
}

const ITEM = /^\/api\/annotations\/([^/]+)$/;

export const handle: MockHandler = async (ctx: MockContext, { req, res, url, method, principal }: MockRequest) => {
  const path = url.pathname;
  if (path === '/api/annotations') {
    if (method === 'GET') ctx.json(res, 200, list(ctx, url));
    else if (method === 'POST') ctx.json(res, 201, create(ctx, await ctx.readJson(req), principal));
    else ctx.fail(405, 'method_not_allowed', 'method not allowed');
    return true;
  }
  const item = ITEM.exec(path);
  if (item) {
    const uuid = decodeURIComponent(item[1]!);
    if (method === 'PATCH') ctx.json(res, 200, update(ctx, uuid, await ctx.readJson(req), principal));
    else if (method === 'DELETE') {
      remove(ctx, uuid, principal);
      res.writeHead(204);
      res.end();
    } else ctx.fail(405, 'method_not_allowed', 'method not allowed');
    return true;
  }
  if (path === '/api/tags') {
    if (method === 'GET') {
      const list: TagList = { tags: [...tags.keys()].sort().map((n) => tagInfo(ctx, n)) };
      ctx.json(res, 200, list);
    } else if (method === 'POST') ctx.json(res, 200, putTag(ctx, await ctx.readJson(req)));
    else ctx.fail(405, 'method_not_allowed', 'method not allowed');
    return true;
  }
  return false;
};
