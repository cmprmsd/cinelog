// State of the live view (plan §8.4; docs/API.md §9.1, §9.2): what the player's
// `eventsource` stream reports, what the UI bus says about the recorder, and the badge that
// follows from both. Pure, so the rules are unit-tested without a browser.

import type { Options, Player, Source } from 'asciinema-player';
import type { BusEvent, LiveSession, Scope, SessionState } from '../api/types';
import { nestedLink } from '../markers';
import { isUuid, liveHref } from '../route';

/**
 * The stream as the page sees it:
 * - `connecting`: waiting for the first `LiveInit`;
 * - `open`: `LiveInit` arrived, events flow;
 * - `reconnecting`: the stream broke after it was open;
 * - `offline`: the recorder died (`LiveOffline`, or the bus says `crashed`);
 * - `ended`: `done` arrived (or the bus says the session ended);
 * - `unavailable`: the server refused the stream (503, rate limit, unreachable);
 * - `missing`: unknown or forgotten session (404).
 */
export type StreamState = 'connecting' | 'open' | 'reconnecting' | 'offline' | 'ended' | 'unavailable' | 'missing';

export interface LiveView {
  stream: StreamState;
  /** The recorder status from the bus, while it lists the session as live. */
  recorder: LiveSession | null;
  /** The HTTP status of the last refused stream (0: no response). */
  refused: number | null;
  /**
   * The bus has told this view the recorder status (a `hello`, or an event about this
   * session), so a seed from an older snapshot must not overwrite it.
   */
  heard: boolean;
}

export type LiveInput =
  /** The player received `LiveInit` (its `playing` event). */
  | { type: 'stream-open' }
  /** The player lost the stream (its `loading` event, or a driver error). */
  | { type: 'stream-error' }
  /** `LiveOffline` arrived. */
  | { type: 'stream-offline' }
  /** `done` arrived (the player's `ended` event). */
  | { type: 'stream-done' }
  /** A probe of the stream URL answered with this status. */
  | { type: 'probe'; status: number }
  /** A new player connection starts. */
  | { type: 'reconnect' }
  /** Session metadata from `GET /api/sessions/{uuid}` (initial load and bus-down polling). */
  | { type: 'detail'; state: SessionState }
  | { type: 'bus'; event: BusEvent }
  /**
   * The live sessions as the bus knew them before this view subscribed (the bus sends
   * `hello` once per connection, not per subscriber).
   */
  | { type: 'seed'; live: readonly LiveSession[] };

export const initialView = (): LiveView => ({ stream: 'connecting', recorder: null, refused: null, heard: false });

/** Final states: only a reconnect (a recreated player) leaves them. */
const settled = (s: StreamState) => s === 'offline' || s === 'ended' || s === 'missing';

/** What a refused stream's status means for the page. */
export function probeOutcome(status: number): 'retry-now' | 'signed-out' | 'missing' | 'unavailable' {
  if (status >= 200 && status < 300) return 'retry-now';
  if (status === 401) return 'signed-out';
  if (status === 404 || status === 410) return 'missing';
  return 'unavailable';
}

/**
 * What a 404 on `GET /api/sessions/{uuid}` means here. The recorder registers a session a few
 * seconds before the indexer has its metadata, so a session the bus lists as live, or whose
 * stream is up or accepted (`streamStatus`, a probe of the stream URL), is not indexed yet: the
 * view keeps streaming and asks again. Only a stream that is gone too (404, 410) is missing.
 * `probe`: the answer needs a probe of the stream URL.
 */
export function detailAbsent(
  stream: StreamState,
  listedLive: boolean,
  streamStatus?: number,
): 'not-indexed' | 'missing' | 'probe' {
  if (stream === 'missing') return 'missing';
  if (listedLive || stream === 'open' || stream === 'reconnecting') return 'not-indexed';
  if (streamStatus === undefined) return 'probe';
  return probeOutcome(streamStatus) === 'missing' ? 'missing' : 'not-indexed';
}

/** Retries of the metadata while the session is not indexed yet: every second, then slower. */
export function notIndexedDelay(attempt: number): number {
  return attempt < 8 ? 1000 : 5000;
}

/** The view after one input for session `uuid`. */
export function reduce(view: LiveView, input: LiveInput, uuid: string): LiveView {
  switch (input.type) {
    case 'stream-open':
      return settled(view.stream) ? view : { ...view, stream: 'open', refused: null };
    case 'stream-error':
      if (settled(view.stream) || view.stream === 'unavailable') return view;
      return { ...view, stream: view.stream === 'open' ? 'reconnecting' : view.stream };
    case 'stream-offline':
      return { ...view, stream: 'offline', recorder: null };
    case 'stream-done':
      return view.stream === 'offline' || view.stream === 'missing' ? view : { ...view, stream: 'ended', recorder: null };
    case 'probe': {
      const out = probeOutcome(input.status);
      if (out === 'missing') return { ...view, stream: 'missing', recorder: null, refused: input.status };
      if (out === 'unavailable') return { ...view, stream: 'unavailable', refused: input.status };
      return view;
    }
    case 'reconnect':
      if (view.stream === 'missing') return view;
      return { ...view, stream: 'connecting' };
    case 'detail':
      // An ended session needs nothing here: its stream sends the final screen and `done`.
      return input.state === 'crashed' && view.stream !== 'missing' ? { ...view, stream: 'offline', recorder: null } : view;
    case 'bus':
      return reduceBus(view, input.event, uuid);
    case 'seed':
      if (view.heard) return view;
      return { ...view, heard: true, recorder: input.live.find((l) => l.session.uuid === uuid) ?? null };
  }
}

function reduceBus(view: LiveView, e: BusEvent, uuid: string): LiveView {
  switch (e.type) {
    case 'hello':
      return { ...view, heard: true, recorder: e.data.live.find((l) => l.session.uuid === uuid) ?? null };
    case 'session-start':
    case 'session-update':
      return e.data.session.uuid === uuid ? { ...view, heard: true, recorder: e.data } : view;
    case 'session-end':
      if (e.data.session !== uuid) return view;
      if (e.data.state === 'crashed' && view.stream !== 'missing') return { ...view, heard: true, stream: 'offline', recorder: null };
      // The stream's own `done` follows; until then the screen keeps updating.
      return { ...view, heard: true, recorder: null };
    case 'forgotten':
      if (e.data.target.kind === 'session' && e.data.target.uuid === uuid) return { ...view, stream: 'missing', recorder: null };
      return view;
    default:
      return view;
  }
}

/** True when a bus event means the stream must be reopened (masking changed). */
export function needsReconnect(e: BusEvent, uuid: string): boolean {
  return e.type === 'forgotten' && e.data.target.uuid === uuid && e.data.target.kind !== 'session';
}

export type BadgeKind = 'live' | 'paused' | 'degraded' | 'connecting' | 'offline' | 'ended' | 'unavailable' | 'missing';

export interface LiveBadge {
  kind: BadgeKind;
  /** Short text: `LIVE`, `paused (tmux)`, `offline`, … */
  label: string;
  /** Longer explanation for the title and screen readers. */
  title: string;
  /** For a nested pause: the child session to follow. */
  child: string | null;
}

/** `paused (tmux)` from a multiplexer detail such as `tmux: client`. */
function multiplexerName(detail: string | null): string {
  const name = detail?.split(':')[0]?.trim();
  return name ? name : 'multiplexer';
}

/** The badge for a recorder status from the bus. */
export function recorderBadge(r: LiveSession): LiveBadge {
  if (r.status === 'recording') return { kind: 'live', label: 'LIVE', title: 'Recording; new output appears as it happens', child: null };
  if (r.status === 'degraded') {
    return { kind: 'degraded', label: 'degraded', title: `The recorder is degraded${r.detail ? `: ${r.detail}` : ''}`, child: null };
  }
  switch (r.reason) {
    case 'multiplexer': {
      const mux = multiplexerName(r.detail);
      return { kind: 'paused', label: `paused (${mux})`, title: `Paused while a ${mux} client runs in this shell; its panes are recorded on their own`, child: null };
    }
    case 'nested': {
      const child = r.detail && isUuid(r.detail) ? r.detail.toLowerCase() : null;
      return { kind: 'paused', label: 'paused (nested)', title: 'Paused while a nested session (sudo) is recorded on its own', child };
    }
    case 'user':
      return { kind: 'paused', label: 'paused (user)', title: `Paused with cinelog pause${r.detail ? `: ${r.detail}` : ''}`, child: null };
    default:
      return { kind: 'paused', label: 'paused', title: `Recording is paused${r.detail ? `: ${r.detail}` : ''}`, child: null };
  }
}

/**
 * Where "watch the nested session" goes for the badge's `child`, on a server of `scope`:
 * in-app (`/live/<child>`) for a child served here, the child's live page on the other
 * scope's server (`external`, a new tab) for a TEAM `sudo -i` in a PERSONAL shell, `null`
 * for a child of the other scope that cannot be located. The child's scope and URL come
 * with the recorder status (`child_scope`, `child_url`, from the `nested` marker); without
 * them the child is assumed to be served here.
 */
export function childLiveHref(
  child: string,
  recorder: LiveSession | null,
  scope: Scope | null,
  host: string,
): { href: string; external: boolean } | null {
  const own = recorder?.detail?.toLowerCase() === child;
  const link = nestedLink(child, own ? (recorder?.child_scope ?? null) : null, own ? (recorder?.child_url ?? null) : null, scope, host);
  if (link.external) {
    const u = new URL(link.external);
    u.pathname = liveHref(child);
    return { href: u.href, external: true };
  }
  return link.child ? { href: liveHref(link.child), external: false } : null;
}

/** The page's badge: final stream states first, then the recorder status, then the stream. */
export function badgeFor(view: LiveView): LiveBadge {
  const b = (kind: BadgeKind, label: string, title: string): LiveBadge => ({ kind, label, title, child: null });
  switch (view.stream) {
    case 'offline':
      return b('offline', 'offline', 'The recorder stopped without ending the session (crashed); this is the last screen');
    case 'ended':
      return b('ended', 'ended', 'The session ended; this is its final screen');
    case 'missing':
      return b('missing', 'not found', 'This session does not exist or was forgotten');
    case 'unavailable':
      return b('unavailable', 'unavailable', 'The server does not stream this session right now');
    case 'connecting':
      return b('connecting', 'connecting…', 'Waiting for the current screen');
    case 'reconnecting':
      return b('connecting', 'reconnecting…', 'The stream was interrupted; reconnecting');
    case 'open':
      return view.recorder ? recorderBadge(view.recorder) : b('live', 'LIVE', 'New output appears as it happens');
  }
}

export const RETRY_MIN_MS = 1000;
export const RETRY_MAX_MS = 30_000;

/** Backoff before the `attempt`-th retry (0-based): 1 s doubling up to 30 s. */
export function retryDelay(attempt: number): number {
  return Math.min(RETRY_MAX_MS, RETRY_MIN_MS * 2 ** Math.max(0, Math.min(attempt, 16)));
}

/**
 * The HTTP status of the stream URL, without reading the stream: the request is aborted as
 * soon as the headers arrive. `0` when no response came (network error).
 */
export async function probeStream(url: string, fetchFn: typeof fetch = fetch): Promise<number> {
  const ctl = new AbortController();
  try {
    const res = await fetchFn(url, { credentials: 'same-origin', signal: ctl.signal, headers: { Accept: 'text/event-stream' } });
    return res.status;
  } catch {
    return 0;
  } finally {
    ctl.abort();
  }
}

/** Fixed player buffer: smooths the server's ~100 ms polling and keeps latency bounded. */
export const LIVE_BUFFER_MS = 200;

/** The player's `eventsource` driver source (not covered by the player's typings). */
export function liveSource(url: string): Source {
  return { driver: 'eventsource', url, bufferTime: LIVE_BUFFER_MS } as unknown as Source;
}

/**
 * Player options for the live view: starts at once, no seek bar (a live stream cannot seek or
 * pause) and no parser options. The size comes from `LiveInit`.
 */
export function liveOptions(opts: { theme: string; keystrokeOverlay: boolean }): Options {
  return { autoPlay: true, theme: opts.theme, fit: 'width', controls: false, keystrokeOverlay: opts.keystrokeOverlay };
}

/** Subscribes to a player event the typings leave out (`loading`, `offline`, `reset`). */
export function onPlayerEvent(player: Player, name: 'loading' | 'offline' | 'reset', handler: () => void): void {
  const add = player.addEventListener as unknown as (name: string, fn: () => void) => void;
  add.call(player, name, handler);
}
