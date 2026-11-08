// Typed client for the cinelog HTTP API (docs/API.md). Same-origin fetch only: the session
// cookie is HttpOnly and travels by itself; nothing secret ever goes into a URL.

import type {
  Annotation,
  AnnotationCreate,
  AnnotationPage,
  AnnotationParams,
  AnnotationPatch,
  ApiError,
  AuthorList,
  CommandList,
  ErrorCode,
  ExportParams,
  ForgetRequest,
  Forgotten,
  LoginInfo,
  LoginRequest,
  Me,
  PlayInfo,
  SearchParams,
  SearchResponse,
  SessionDetail,
  SessionPage,
  SessionParams,
  Status,
  Tag,
  TagList,
  TagRequest,
  Timeline,
  TimelineParams,
  Tree,
  TreeParams,
} from './types';

export type Query = Record<string, string | number | boolean | null | undefined>;

/** A failed request: an API error, or `network` when no response arrived. */
export class ApiFailure extends Error {
  readonly status: number;
  readonly code: ErrorCode | 'network';
  /** Seconds from `Retry-After` (429). */
  readonly retryAfter: number | null;

  constructor(status: number, code: ErrorCode | 'network', message: string, retryAfter: number | null = null) {
    super(message);
    this.name = 'ApiFailure';
    this.status = status;
    this.code = code;
    this.retryAfter = retryAfter;
  }
}

/** `?a=1&b=x` from defined, non-empty values; `''` when nothing is left. */
export function buildQuery(params?: Query | object): string {
  if (!params) return '';
  const sp = new URLSearchParams();
  for (const [k, v] of Object.entries(params)) {
    if (v === undefined || v === null || v === '') continue;
    sp.set(k, String(v));
  }
  const s = sp.toString();
  return s ? `?${s}` : '';
}

const seg = (s: string) => encodeURIComponent(s);

export const sessionPath = (uuid: string) => `/api/sessions/${seg(uuid)}`;

let unauthorized: (() => void) | null = null;

/** Called on any 401 outside the login endpoint (the session expired or was revoked). */
export function onUnauthorized(handler: (() => void) | null): void {
  unauthorized = handler;
}

interface RequestOptions {
  query?: Query | object;
  body?: unknown;
  signal?: AbortSignal;
}

async function failure(res: Response): Promise<ApiFailure> {
  let code: ErrorCode | 'network' = 'internal';
  let message = `${res.status} ${res.statusText}`.trim();
  try {
    const body = (await res.json()) as Partial<ApiError>;
    if (typeof body.error === 'string') code = body.error;
    if (typeof body.message === 'string' && body.message) message = body.message;
  } catch {
    // Not an ApiError body (proxy error page and the like).
  }
  const ra = Number(res.headers.get('retry-after'));
  return new ApiFailure(res.status, code, message, Number.isFinite(ra) && ra > 0 ? ra : null);
}

/**
 * Runs `load` again while it fails with 404, up to `tries` times `delayMs` apart. A session
 * whose link was just printed (`cinelog link`) is in the index only after the indexer's
 * first commit, about 2 s after the shell started.
 */
export async function retryNotFound<T>(
  load: () => Promise<T>,
  signal: AbortSignal,
  tries = 5,
  delayMs = 1000,
): Promise<T> {
  for (let i = 1; ; i++) {
    try {
      return await load();
    } catch (e) {
      if (!(e instanceof ApiFailure && e.status === 404) || i >= tries || signal.aborted) throw e;
    }
    await new Promise<void>((resolve, reject) => {
      const timer = setTimeout(resolve, delayMs);
      signal.addEventListener(
        'abort',
        () => {
          clearTimeout(timer);
          reject(new DOMException('aborted', 'AbortError'));
        },
        { once: true },
      );
    });
  }
}

export type Method = 'GET' | 'POST' | 'PATCH' | 'DELETE';

export async function request<T>(method: Method, path: string, opts: RequestOptions = {}): Promise<T> {
  const init: RequestInit = { method, credentials: 'same-origin', signal: opts.signal, headers: {} };
  if (opts.body !== undefined) {
    init.headers = { 'Content-Type': 'application/json' };
    init.body = JSON.stringify(opts.body);
  }
  let res: Response;
  try {
    res = await fetch(path + buildQuery(opts.query), init);
  } catch (e) {
    if (e instanceof DOMException && e.name === 'AbortError') throw e;
    throw new ApiFailure(0, 'network', 'The server is not reachable.');
  }
  if (!res.ok) {
    const err = await failure(res);
    if (res.status === 401 && path !== '/api/login') unauthorized?.();
    throw err;
  }
  if (res.status === 204) return undefined as T;
  return (await res.json()) as T;
}

const get = <T>(path: string, query?: Query | object, signal?: AbortSignal) =>
  request<T>('GET', path, { query, signal });

export const api = {
  loginInfo: () => get<LoginInfo>('/api/login'),
  login: (token: string) => request<void>('POST', '/api/login', { body: { token } satisfies LoginRequest }),
  logout: () => request<void>('POST', '/api/logout', { body: {} }),
  me: () => get<Me>('/api/me'),
  status: () => get<Status>('/api/status'),
  sessions: (params: SessionParams, signal?: AbortSignal) => get<SessionPage>('/api/sessions', params, signal),
  tree: (params: TreeParams, signal?: AbortSignal) => get<Tree>('/api/tree', params, signal),
  session: (uuid: string, signal?: AbortSignal) => get<SessionDetail>(sessionPath(uuid), undefined, signal),
  commands: (uuid: string, signal?: AbortSignal) =>
    get<CommandList>(`${sessionPath(uuid)}/commands`, undefined, signal),
  search: (params: SearchParams, signal?: AbortSignal) => get<SearchResponse>('/api/search', params, signal),
  play: (uuid: string, signal?: AbortSignal) => get<PlayInfo>(`${sessionPath(uuid)}/play`, undefined, signal),
  forget: (req: ForgetRequest) => request<Forgotten>('POST', '/api/forget', { body: req }),
  exportUrl: (uuid: string, params: ExportParams) => `${sessionPath(uuid)}/export${buildQuery(params)}`,
  // M4 (docs/API.md §9)
  /** The player's `eventsource` driver URL of a session (`LiveMessage` stream). */
  liveUrl: (uuid: string) => `${sessionPath(uuid)}/live`,
  timeline: (params: TimelineParams, signal?: AbortSignal) => get<Timeline>('/api/timeline', params, signal),
  annotations: (params: AnnotationParams, signal?: AbortSignal) =>
    get<AnnotationPage>('/api/annotations', params, signal),
  createAnnotation: (req: AnnotationCreate) => request<Annotation>('POST', '/api/annotations', { body: req }),
  updateAnnotation: (uuid: string, patch: AnnotationPatch) =>
    request<Annotation>('PATCH', `/api/annotations/${seg(uuid)}`, { body: patch }),
  deleteAnnotation: (uuid: string) => request<void>('DELETE', `/api/annotations/${seg(uuid)}`),
  annotationAuthors: (signal?: AbortSignal) => get<AuthorList>('/api/annotations/authors', undefined, signal),
  tags: (signal?: AbortSignal) => get<TagList>('/api/tags', undefined, signal),
  putTag: (req: TagRequest) => request<Tag>('POST', '/api/tags', { body: req }),
};

/** A short user-facing text for any thrown value. */
export function errorText(e: unknown): string {
  if (e instanceof ApiFailure) return e.message;
  if (e instanceof Error) return e.message;
  return String(e);
}

export const isAbort = (e: unknown) => e instanceof DOMException && e.name === 'AbortError';
