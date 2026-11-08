import { afterEach, describe, expect, it, vi } from 'vitest';
import { api, ApiFailure, buildQuery, onUnauthorized } from './client';

function respond(status: number, body?: unknown, headers: Record<string, string> = {}) {
  const fetch = vi.fn<(url: string, init?: RequestInit) => Promise<Response>>(
    async () => new Response(body === undefined ? null : JSON.stringify(body), { status, headers }),
  );
  vi.stubGlobal('fetch', fetch);
  return fetch;
}

describe('api client', () => {
  afterEach(() => {
    vi.unstubAllGlobals();
    onUnauthorized(null);
  });

  it('builds queries from defined values', () => {
    expect(buildQuery({ q: 'a b', in: 'cmd', case: false, limit: 5, cursor: undefined, user: '', host: null })).toBe(
      '?q=a+b&in=cmd&case=false&limit=5',
    );
    expect(buildQuery({})).toBe('');
    expect(buildQuery()).toBe('');
  });

  it('sends JSON bodies with the content type and same-origin credentials', async () => {
    const fetch = respond(204);
    await api.login('TOKEN');
    const [url, init] = fetch.mock.calls[0]!;
    expect(url).toBe('/api/login');
    expect(init?.method).toBe('POST');
    expect(init?.credentials).toBe('same-origin');
    expect(init?.headers).toEqual({ 'Content-Type': 'application/json' });
    expect(init?.body).toBe('{"token":"TOKEN"}');
  });

  it('never puts the token in the URL', async () => {
    const fetch = respond(204);
    await api.login('SECRET');
    expect(String(fetch.mock.calls[0]![0])).not.toContain('SECRET');
  });

  it('encodes path segments and query parameters', async () => {
    const fetch = respond(200, { hits: [], next: null, partial: false, capped: false });
    await api.search({ q: '%3 & co', in: 'both', pane: '%3' });
    expect(fetch.mock.calls[0]![0]).toBe('/api/search?q=%253+%26+co&in=both&pane=%253');
    respond(200, {});
    expect(api.exportUrl('a/b', { format: 'txt', cmd: 2 })).toBe('/api/sessions/a%2Fb/export?format=txt&cmd=2');
  });

  it('turns ApiError bodies into ApiFailure', async () => {
    respond(429, { error: 'too_many_requests', message: 'slow down' }, { 'Retry-After': '42' });
    const err = await api.login('x').catch((e: unknown) => e);
    expect(err).toBeInstanceOf(ApiFailure);
    expect(err).toMatchObject({ status: 429, code: 'too_many_requests', message: 'slow down', retryAfter: 42 });
  });

  it('reports 401 outside login to the handler', async () => {
    const handler = vi.fn();
    onUnauthorized(handler);
    respond(401, { error: 'unauthorized', message: 'no session' });
    await expect(api.me()).rejects.toMatchObject({ status: 401 });
    expect(handler).toHaveBeenCalledTimes(1);
    respond(401, { error: 'unauthorized', message: 'wrong token' });
    await expect(api.login('bad')).rejects.toMatchObject({ status: 401 });
    expect(handler).toHaveBeenCalledTimes(1);
  });

  it('maps network failures and keeps aborts', async () => {
    vi.stubGlobal('fetch', vi.fn(async () => Promise.reject(new TypeError('Failed to fetch'))));
    await expect(api.status()).rejects.toMatchObject({ code: 'network', status: 0 });
    vi.stubGlobal('fetch', vi.fn(async () => Promise.reject(new DOMException('aborted', 'AbortError'))));
    await expect(api.status()).rejects.toBeInstanceOf(DOMException);
  });

  it('tolerates non-JSON error bodies', async () => {
    vi.stubGlobal('fetch', vi.fn(async () => new Response('<html>bad gateway</html>', { status: 502, statusText: 'Bad Gateway' })));
    await expect(api.status()).rejects.toMatchObject({ status: 502, code: 'internal', message: '502 Bad Gateway' });
  });
});
