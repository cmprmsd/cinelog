// Client routes (plan §8): `/login`, `/` (search), `/s/<uuid>` (session, `?t=` or `?cmd=`),
// `/timeline` (M4-B, `?from=&to=&group=`), `/live/<uuid>` (M4-D), `/annotations` (the
// annotations overview, `?tag=&author=&q=&date=&from=&to=`, docs/API.md §9.6).

export type Route =
  | { name: 'login' }
  | { name: 'search' }
  | { name: 'session'; uuid: string }
  | { name: 'timeline' }
  | { name: 'annotations' }
  | { name: 'live'; uuid: string }
  | { name: 'notfound' };

const UUID = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i;

export function isUuid(s: string): boolean {
  return UUID.test(s);
}

export function parseRoute(path: string): Route {
  const p = path.replace(/\/+$/, '') || '/';
  if (p === '/') return { name: 'search' };
  if (p === '/login') return { name: 'login' };
  if (p === '/timeline') return { name: 'timeline' };
  if (p === '/annotations') return { name: 'annotations' };
  const m = /^\/(s|live)\/([^/]+)$/.exec(p);
  if (m?.[2]) {
    let id: string;
    try {
      id = decodeURIComponent(m[2]);
    } catch {
      return { name: 'notfound' };
    }
    if (isUuid(id)) return { name: m[1] === 's' ? 'session' : 'live', uuid: id.toLowerCase() };
  }
  return { name: 'notfound' };
}

/** The live view of a session. */
export const liveHref = (uuid: string) => `/live/${uuid}`;

/** A same-origin path to return to after login; anything else falls back to `/`. */
export function safeNext(next: string | null | undefined): string {
  if (!next || !next.startsWith('/') || next.startsWith('//') || next.startsWith('/\\')) return '/';
  if (next.startsWith('/login')) return '/';
  return next;
}

/** `/s/<uuid>` with an optional `cmd` or `t`, keeping the given search state. */
export function sessionHref(uuid: string, at: { cmd?: number | null; t?: number | null } = {}, keep?: URLSearchParams): string {
  const sp = new URLSearchParams(keep);
  sp.delete('cmd');
  sp.delete('t');
  if (at.cmd !== undefined && at.cmd !== null) sp.set('cmd', String(at.cmd));
  else if (at.t !== undefined && at.t !== null) sp.set('t', String(Math.max(0, Math.round(at.t * 1000) / 1000)));
  const qs = sp.toString();
  return `/s/${uuid}${qs ? `?${qs}` : ''}`;
}

/** Plain left click without modifiers: handle in the SPA instead of letting the browser navigate. */
export function isPlainClick(e: MouseEvent): boolean {
  return e.button === 0 && !e.metaKey && !e.ctrlKey && !e.shiftKey && !e.altKey && !e.defaultPrevented;
}
