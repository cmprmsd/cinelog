// Client of the UI bus `GET /api/events` (docs/API.md §9.2): one shared EventSource for the
// whole app, opened while anything is subscribed. Every SSE event is named after
// `BusEvent.type` and carries the JSON of `BusEvent.data`; `hello` comes first on every
// (re)connection and is a full resync of the live sessions.

import { api } from './client';
import type { BusEvent, LiveSession } from './types';

/** Every bus event name (must match `BusEvent::NAMES` in api/types.rs). */
export const BUS_EVENT_NAMES = [
  'hello',
  'session-start',
  'session-update',
  'session-end',
  'index-progress',
  'import-progress',
  'annotation',
  'tag',
  'forgotten',
] as const satisfies readonly BusEvent['type'][];

// Fails to compile when a `BusEvent` variant is missing above.
const exhaustive: Exclude<BusEvent['type'], (typeof BUS_EVENT_NAMES)[number]> extends never ? true : never = true;
void exhaustive;

export const BUS_URL = '/api/events';
const RETRY_MIN_MS = 1000;
const RETRY_MAX_MS = 30_000;

/**
 * `idle`: nothing subscribed. `connecting`: waiting for `hello`. `open`: events flow.
 * `down`: the stream failed and a reconnect is scheduled (callers fall back to polling).
 */
export type BusStatus = 'idle' | 'connecting' | 'open' | 'down';

export type BusHandler = (event: BusEvent) => void;

/** The part of `EventSource` the bus uses (tests pass a fake). */
export interface EventSourceLike {
  readonly readyState: number;
  addEventListener(type: string, listener: (e: MessageEvent<string>) => void): void;
  close(): void;
}

const CLOSED = 2;

/** One bus event from its SSE name and data; `null` for unknown names or bad JSON. */
export function parseBusEvent(type: string, data: string): BusEvent | null {
  if (!(BUS_EVENT_NAMES as readonly string[]).includes(type)) return null;
  try {
    return { type, data: JSON.parse(data) } as BusEvent;
  } catch {
    return null;
  }
}

/**
 * The live sessions after `event`: `hello` replaces them, `session-start`/`session-update`
 * upsert one (newest `started_at` first), `session-end` and a forgotten session remove one.
 */
export function nextLive(live: readonly LiveSession[], event: BusEvent): LiveSession[] | null {
  switch (event.type) {
    case 'hello':
      return [...event.data.live].sort((a, b) => b.session.started_at - a.session.started_at);
    case 'session-start':
    case 'session-update': {
      const uuid = event.data.session.uuid;
      return [...live.filter((l) => l.session.uuid !== uuid), event.data].sort((a, b) => b.session.started_at - a.session.started_at);
    }
    case 'session-end':
      return live.filter((l) => l.session.uuid !== event.data.session);
    case 'forgotten': {
      const t = event.data.target;
      return t.kind === 'session' ? live.filter((l) => l.session.uuid !== t.uuid) : null;
    }
    default:
      return null;
  }
}

export class Bus {
  status = $state<BusStatus>('idle');
  /** The live sessions as of the last `hello` and the events since; empty while idle. */
  live = $state<LiveSession[]>([]);

  private readonly open: () => EventSourceLike;
  private readonly onClosed: () => void;
  private handlers: BusHandler[] = [];
  private source: EventSourceLike | null = null;
  private retryMs = RETRY_MIN_MS;
  private timer: ReturnType<typeof setTimeout> | undefined;

  /**
   * `open` creates the stream; `onClosed` runs when the server refused it (an HTTP error
   * closes an EventSource for good), e.g. to find out whether the session expired.
   */
  constructor(open: () => EventSourceLike, onClosed: () => void = () => {}) {
    this.open = open;
    this.onClosed = onClosed;
  }

  /** Calls `handler` for every event until the returned function is called. */
  subscribe(handler: BusHandler): () => void {
    this.handlers = [...this.handlers, handler];
    if (!this.source && this.timer === undefined) this.connect();
    return () => {
      const before = this.handlers.length;
      this.handlers = this.handlers.filter((h) => h !== handler);
      if (before > 0 && this.handlers.length === 0) this.stop();
    };
  }

  private emit(event: BusEvent): void {
    for (const h of this.handlers) {
      try {
        h(event);
      } catch (e) {
        console.error('bus handler failed', e);
      }
    }
  }

  private connect(): void {
    this.timer = undefined;
    this.status = 'connecting';
    let es: EventSourceLike;
    try {
      es = this.open();
    } catch {
      this.retry();
      return;
    }
    this.source = es;
    for (const name of BUS_EVENT_NAMES) {
      es.addEventListener(name, (e) => {
        if (this.source !== es) return;
        const event = parseBusEvent(name, e.data);
        if (!event) return;
        if (event.type === 'hello') {
          this.status = 'open';
          this.retryMs = RETRY_MIN_MS;
        }
        const live = nextLive(this.live, event);
        if (live) this.live = live;
        this.emit(event);
      });
    }
    es.addEventListener('error', () => {
      if (this.source !== es) return;
      // A network error: the browser reconnects by itself (and we get a new `hello`).
      // An HTTP error closes the stream: reconnect later ourselves.
      this.status = es.readyState === CLOSED ? 'down' : 'connecting';
      if (es.readyState === CLOSED) {
        this.source = null;
        this.onClosed();
        this.retry();
      }
    });
  }

  private retry(): void {
    this.status = 'down';
    this.timer = setTimeout(() => this.connect(), this.retryMs);
    this.retryMs = Math.min(RETRY_MAX_MS, this.retryMs * 2);
  }

  private stop(): void {
    clearTimeout(this.timer);
    this.timer = undefined;
    this.source?.close();
    this.source = null;
    this.retryMs = RETRY_MIN_MS;
    this.status = 'idle';
    this.live = [];
  }
}

/** The app's bus. A refused stream checks the session, so an expired one signs out. */
export const bus = new Bus(
  () => new EventSource(BUS_URL),
  () => void api.me().catch(() => {}),
);
