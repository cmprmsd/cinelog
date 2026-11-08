// Minimal history router: the current path and query as reactive state.

import { isPlainClick } from './route';

class Router {
  path = $state(globalThis.location?.pathname ?? '/');
  search = $state(globalThis.location?.search ?? '');
  query = $derived(new URLSearchParams(this.search));

  constructor() {
    globalThis.addEventListener?.('popstate', () => this.sync());
  }

  private sync(): void {
    this.path = location.pathname;
    this.search = location.search;
  }

  /** Navigates within the SPA. `replace` keeps the history entry (typing, filters). */
  goto(url: string, { replace = false }: { replace?: boolean } = {}): void {
    const target = new URL(url, location.href);
    if (target.origin !== location.origin) return;
    const href = target.pathname + target.search + target.hash;
    if (href === location.pathname + location.search + location.hash) return;
    if (replace) history.replaceState(history.state, '', href);
    else history.pushState(null, '', href);
    this.sync();
  }

  /** Replaces the query string of the current path. */
  setQuery(sp: URLSearchParams, opts?: { replace?: boolean }): void {
    const qs = sp.toString();
    this.goto(location.pathname + (qs ? `?${qs}` : ''), opts);
  }

  /** `onclick` for internal links: SPA navigation on plain clicks, browser default otherwise. */
  link = (e: MouseEvent): void => {
    const a = e.currentTarget;
    if (!(a instanceof HTMLAnchorElement) || !isPlainClick(e) || a.target === '_blank') return;
    e.preventDefault();
    this.goto(a.href);
  };
}

export const router = new Router();
