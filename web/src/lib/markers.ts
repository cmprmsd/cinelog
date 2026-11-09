// Recorder markers as bands and points on the session rail: pauses, gaps, forgotten
// ranges and nested child sessions (plan §8.2, §15.2: every pause leaves a visible marker).

import type { MaskedSpan, Marker, Scope } from './api/types';

export interface Band {
  kind: 'pause' | 'gap' | 'forgotten';
  from: number;
  to: number;
  label: string;
}

export interface NestedPoint {
  t: number;
  child: string | null;
  label: string;
  /**
   * The child's page on the other scope's server (a TEAM `sudo -i` inside a PERSONAL shell):
   * this server has no such session, so an in-app `/s/<child>` link would not find it.
   * `null` when the child is served here (link in-app) or its server is unknown.
   */
  external: string | null;
}

/** The TEAM server's `auto` port (README "Team (root) recording"). */
const TEAM_PORT = 10000;

/**
 * Where a child of another scope is served: the `url` its recorder reported when its server
 * ran (https only, and only a page of that very session), else the TEAM default port on this
 * host. A PERSONAL child seen from the TEAM server has a per-user port, unknown here.
 */
export function externalChildUrl(child: string, scope: Scope, url: string | null, host: string): string | null {
  if (url) {
    try {
      const u = new URL(url);
      if (u.protocol === 'https:' && u.pathname === `/s/${encodeURIComponent(child)}`) return u.href;
    } catch {
      // fall through to the default
    }
  }
  // `host` is a URL's `hostname`, which keeps an IPv6 address in brackets.
  if (scope !== 'team' || !host) return null;
  return `https://${host}:${TEAM_PORT}/s/${encodeURIComponent(child)}`;
}

/**
 * Where a `nested` child opens, seen from a server of `scope`: `external` for a child of the
 * other scope (its server, see {@link externalChildUrl}), else `child` for an in-app link.
 * A child of the other scope that cannot be located gets neither, like a child without uuid.
 */
export function nestedLink(
  child: string | null,
  childScope: Scope | null,
  url: string | null,
  scope: Scope | null,
  host: string,
): { child: string | null; external: string | null } {
  if (!child || !scope || !childScope || childScope === scope) return { child, external: null };
  const external = externalChildUrl(child, childScope, url, host);
  return { child: external ? child : null, external };
}

const str = (v: unknown): string | null => (typeof v === 'string' && v ? v : null);
const num = (v: unknown): number | null => (typeof v === 'number' && Number.isFinite(v) ? v : null);

function pauseLabel(data: Record<string, unknown>): string {
  const reason = str(data.reason);
  const detail = str(data.detail);
  if (!reason) return 'paused';
  return detail ? `paused (${reason}: ${detail})` : `paused (${reason})`;
}

export function markerBands(
  markers: readonly Marker[],
  masked: readonly MaskedSpan[],
  duration: number,
  /** The scope of the session these markers belong to, i.e. of this server. */
  scope: Scope | null = null,
  /** This page's host name, for the TEAM default. */
  host: string = globalThis.location?.hostname ?? '',
): { bands: Band[]; nested: NestedPoint[] } {
  const bands: Band[] = [];
  const nested: NestedPoint[] = [];
  let open: { t: number; label: string } | null = null;
  for (const m of markers) {
    switch (m.kind) {
      case 'pause':
        if (!open) open = { t: m.t, label: pauseLabel(m.data) };
        break;
      case 'resume':
        if (open) {
          bands.push({ kind: 'pause', from: open.t, to: Math.max(open.t, m.t), label: open.label });
          open = null;
        }
        break;
      case 'gap': {
        const from = num(m.data.from_t) ?? m.t;
        const to = num(m.data.to_t) ?? from;
        const bytes = num(m.data.dropped_bytes);
        bands.push({ kind: 'gap', from, to: Math.max(from, to), label: bytes ? `gap (${bytes} bytes dropped)` : 'gap' });
        break;
      }
      case 'nested': {
        const user = str(m.data.user);
        const sudo = str(m.data.sudo_user);
        const mode = str(m.data.mode);
        let label = user ? `nested: ${user}` : 'nested session';
        if (sudo && sudo !== user) label += ` (sudo by ${sudo})`;
        if (mode === 'pause') label += ', outer paused';
        const childScope = m.data.scope === 'team' || m.data.scope === 'personal' ? m.data.scope : null;
        const link = nestedLink(str(m.data.child_uuid), childScope, str(m.data.url), scope, host);
        nested.push({ t: m.t, label, ...link });
        break;
      }
    }
  }
  if (open) bands.push({ kind: 'pause', from: open.t, to: Math.max(open.t, duration), label: open.label });

  // The server reports what playback masks; a forgotten command has left the command list.
  for (const f of masked) {
    bands.push({ kind: 'forgotten', from: f.t_from, to: Math.max(f.t_from, f.t_to ?? duration), label: `forgotten by ${f.by}` });
  }
  bands.sort((a, b) => a.from - b.from);
  return { bands, nested };
}
