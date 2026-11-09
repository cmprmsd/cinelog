<script lang="ts">
  // The live view of one session (plan §8.4; docs/API.md §9.1, §9.2). The player's
  // `eventsource` driver reads `/api/sessions/<uuid>/live`: `LiveInit` shows the current
  // screen to a late joiner, events follow, `LiveOffline` + `done` end it. Read-only: the API
  // has no input path and the page offers none. The badge combines the stream with the
  // recorder status from the UI bus (`LIVE`, `paused (tmux)`, `offline`, …). A refused
  // stream (503 and the like) shows the recording so far and retries with backoff. A session
  // younger than the indexer's lag has no metadata yet (404): it streams while the page asks again.
  import * as AsciinemaPlayer from 'asciinema-player';
  import { untrack } from 'svelte';
  import { api, ApiFailure, errorText, isAbort } from '../api/client';
  import { bus } from '../api/events.svelte';
  import type { BusEvent, Session, SessionDetail } from '../api/types';
  import FitBox from '../components/FitBox.svelte';
  import FitToggle from '../components/FitToggle.svelte';
  import SessionView from '../components/SessionView.svelte';
  import type { FitMode } from '../fit';
  import { stateFromQuery } from '../filters';
  import { fmtClock, fmtTs, keyLabel, tzLabel } from '../format';
  import { useKeys } from '../keymap';
  import { onTermSize } from '../player';
  import { playerTheme, prefs } from '../prefs.svelte';
  import { sessionHref } from '../route';
  import { router } from '../router.svelte';
  import {
    badgeFor,
    childLiveHref,
    detailAbsent,
    initialView,
    liveOptions,
    liveSource,
    needsReconnect,
    notIndexedDelay,
    onPlayerEvent,
    probeOutcome,
    probeStream,
    reduce,
    retryDelay,
    type LiveInput,
  } from './live';

  let { uuid }: { uuid: string } = $props();

  /** How long a broken stream may take to come back by itself before it is probed. */
  const GRACE_MS = 2500;
  /** Session metadata refresh while the UI bus is down. */
  const POLL_MS = 5000;
  const TICK_MS = 500;

  let view = $state(initialView());
  let detail = $state<SessionDetail | null>(null);
  let el: HTMLDivElement | undefined = $state();
  let generation = $state(0);
  let clock = $state<number | null>(null);
  let keys = $state<string[]>([]);
  let retryIn = $state<number | null>(null);
  /** The terminal's size from `LiveInit` and the stream's resizes (0: not joined yet). */
  let term = $state({ cols: 0, rows: 0 });
  let fitEffective = $state<FitMode>('screen');

  const badge = $derived(badgeFor(view));
  const streaming = $derived(view.stream !== 'unavailable' && view.stream !== 'missing');
  const s = $derived(detail?.session ?? view.recorder?.session ?? null);
  const childLink = $derived(badge.child ? childLiveHref(badge.child, view.recorder, s?.scope ?? null, location.hostname) : null);
  const search = $derived(stateFromQuery(router.query));

  function apply(input: LiveInput): void {
    view = reduce(view, input, uuid);
  }

  // ---- stream trouble: probe, back off, reconnect -----------------------------------------

  let disposed = false;
  let troubleTimer: ReturnType<typeof setTimeout> | undefined;
  let retryTimer: ReturnType<typeof setTimeout> | undefined;
  let countdown: ReturnType<typeof setInterval> | undefined;
  /** Refused probes in a row (backoff of the retries). */
  let attempt = 0;
  /** Reconnects in a row that did not reach `LiveInit` (a stream that accepts, then breaks). */
  let flaps = 0;
  let probing = false;

  function clearTimers(): void {
    clearTimeout(troubleTimer);
    clearTimeout(retryTimer);
    clearInterval(countdown);
    troubleTimer = retryTimer = countdown = undefined;
    retryIn = null;
  }

  function reconnect(): void {
    clearTimers();
    apply({ type: 'reconnect' });
    generation += 1;
  }

  function onTrouble(): void {
    apply({ type: 'stream-error' });
    if (!probing && troubleTimer === undefined && retryTimer === undefined) troubleTimer = setTimeout(check, GRACE_MS);
  }

  /** Why the stream is down: a network blip heals by itself, an HTTP refusal does not. */
  async function check(): Promise<void> {
    if (probing) return;
    clearTimers();
    probing = true;
    const status = await probeStream(api.liveUrl(uuid));
    probing = false;
    if (disposed || view.stream === 'open' || view.stream === 'ended' || view.stream === 'offline') return;
    apply({ type: 'probe', status });
    switch (probeOutcome(status)) {
      case 'retry-now': {
        attempt = 0;
        const wait = flaps === 0 ? 0 : retryDelay(flaps - 1);
        flaps += 1;
        if (wait === 0) reconnect();
        else retryTimer = setTimeout(reconnect, wait);
        break;
      }
      case 'signed-out':
        // The client's 401 handler signs the app out.
        void api.me().catch(() => {});
        break;
      case 'unavailable': {
        const wait = retryDelay(attempt);
        attempt += 1;
        const due = Date.now() + wait;
        retryIn = Math.ceil(wait / 1000);
        countdown = setInterval(() => (retryIn = Math.max(0, Math.ceil((due - Date.now()) / 1000))), 1000);
        retryTimer = setTimeout(check, wait);
        break;
      }
      case 'missing':
        break;
    }
  }

  // ---- the player --------------------------------------------------------------------------

  $effect(() => {
    if (!el || !streaming) return;
    void generation;
    const target = el;
    const opts = liveOptions({ theme: playerTheme(prefs.resolved), keystrokeOverlay: prefs.keystrokeOverlay });
    const p = untrack(() => AsciinemaPlayer.create(liveSource(api.liveUrl(uuid)), target, opts));
    p.addEventListener('playing', () => {
      clearTimeout(troubleTimer);
      troubleTimer = undefined;
      attempt = 0;
      flaps = 0;
      apply({ type: 'stream-open' });
    });
    onPlayerEvent(p, 'loading', onTrouble);
    onPlayerEvent(p, 'offline', () => apply({ type: 'stream-offline' }));
    p.addEventListener('ended', () => apply({ type: 'stream-done' }));
    onTermSize(p, (cols, rows) => {
      if (cols !== term.cols || rows !== term.rows) term = { cols, rows };
    });
    p.addEventListener('input', (e) => (keys = [...keys, keyLabel(e.data)].slice(-40)));
    const add = p.addEventListener as unknown as (name: string, fn: () => void) => void;
    add.call(p, 'error', onTrouble);
    const tick = setInterval(() => {
      const t = p.getCurrentTime();
      if (Number.isFinite(t) && t > 0) clock = t;
    }, TICK_MS);
    return () => {
      clearInterval(tick);
      clearTimeout(troubleTimer);
      troubleTimer = undefined;
      p.dispose();
      target.replaceChildren();
    };
  });

  // ---- session metadata: initial load, and polling while the bus is down -------------------

  /** A 404 on the metadata: the session is missing, or only not indexed yet. */
  async function notIndexed(id: string, signal: AbortSignal): Promise<boolean> {
    const listed = () => view.recorder !== null || bus.live.some((l) => l.session.uuid === id);
    let out = detailAbsent(view.stream, listed());
    if (out === 'probe') {
      const status = await probeStream(api.liveUrl(id));
      if (signal.aborted) return true;
      out = detailAbsent(view.stream, listed(), status);
    }
    return out === 'not-indexed';
  }

  $effect(() => {
    const id = uuid;
    const ctl = new AbortController();
    let timer: ReturnType<typeof setTimeout> | undefined;
    let misses = 0;
    const load = async () => {
      try {
        const d = await api.session(id, ctl.signal);
        detail = d;
        apply({ type: 'detail', state: d.session.state });
      } catch (e) {
        if (isAbort(e)) return;
        if (e instanceof ApiFailure && e.status === 404) {
          if (await notIndexed(id, ctl.signal)) {
            if (!ctl.signal.aborted) timer = setTimeout(load, notIndexedDelay(misses++));
          } else if (!ctl.signal.aborted) {
            apply({ type: 'probe', status: 404 });
          }
          return;
        }
        console.warn('live view: session metadata', errorText(e));
      }
      const final = view.stream === 'ended' || view.stream === 'offline' || view.stream === 'missing';
      if (!final) timer = setTimeout(poll, POLL_MS);
    };
    const poll = () => {
      if (bus.status === 'open') timer = setTimeout(poll, POLL_MS);
      else void load();
    };
    void load();
    return () => {
      ctl.abort();
      clearTimeout(timer);
    };
  });

  /**
   * The shared bus sends `hello` once per connection. When it is already open (the user
   * came here in-app), that `hello` went by before this view subscribed: take the recorder
   * status from the bus's live list, so a pause that began earlier shows at once.
   */
  function seedRecorder(): void {
    if (bus.status === 'open') apply({ type: 'seed', live: bus.live });
  }

  $effect(() => {
    const off = bus.subscribe((e: BusEvent) => {
      apply({ type: 'bus', event: e });
      if ((e.type === 'session-start' || e.type === 'session-update') && e.data.session.uuid === uuid && detail) {
        detail = { ...detail, session: e.data.session };
      }
      // Something of this session was forgotten: a fresh join gets the masked screen.
      if (needsReconnect(e, uuid) && streaming) reconnect();
    });
    untrack(seedRecorder);
    return off;
  });

  $effect(() => () => {
    disposed = true;
    clearTimers();
  });

  $effect(() => useKeys({ 'toggle-fit': () => prefs.toggleFit() }));

  const where = (session: Session) => {
    const t = session.tmux;
    if (t.pane) return `tmux ${t.session ?? t.session_id ?? '?'}:${t.window ?? '?'}${t.window_name ? ` (${t.window_name})` : ''} ${t.pane}`;
    return session.holder_comm ?? session.tty ?? '';
  };
</script>

<section class="live" aria-label="Live session" data-testid="live-view" data-stream={view.stream}>
  <header class="head">
    <div class="title">
      <h2>
        {#if badge.kind === 'live'}<span class="live-dot" aria-hidden="true"></span>{/if}
        {#if s}<span>{s.user}@{s.host}</span>{#if s.sudo_user}<span class="muted">(sudo by {s.sudo_user})</span>{/if}{:else}<span
            class="mono">{uuid}</span
          >{/if}
        <span class={`state ${badge.kind}`} title={badge.title} data-testid="live-badge" data-kind={badge.kind}>{badge.label}</span>
      </h2>
      <div class="meta muted">
        {#if s}
          <span>started {fmtTs(s.started_at, prefs.utc)} {tzLabel(prefs.utc)}</span>
          {#if where(s)}<span>{where(s)}</span>{/if}
          <span>{s.scope}</span>
        {/if}
        {#if clock !== null && view.stream !== 'unavailable'}<span data-testid="live-clock">at {fmtClock(clock)}</span>{/if}
        <span title="Watching cannot send input to the session">view only</span>
      </div>
      {#if childLink}
        <div class="links">
          {#if childLink.external}
            <!-- Recorded by the other scope (a TEAM `sudo -i`): its server, a new tab, its own sign-in. -->
            <a href={childLink.href} target="_blank" rel="noopener noreferrer" title="Opens on the server that records it" data-testid="live-child"
              >↗ watch the nested session</a
            >
          {:else}
            <a href={childLink.href} onclick={router.link} data-testid="live-child">↘ watch the nested session</a>
          {/if}
        </div>
      {/if}
    </div>
    <div class="actions">
      <a class="btn" href={sessionHref(uuid)} onclick={router.link} data-testid="open-recording" title="The recording with chapters and search (l)"
        >Open recording</a
      >
      <button
        type="button"
        aria-pressed={prefs.keystrokeOverlay}
        onclick={() => prefs.setKeystrokeOverlay(!prefs.keystrokeOverlay)}
        title="Show keystrokes over the terminal">Keys overlay</button
      >
      <FitToggle effective={fitEffective} />
    </div>
  </header>

  {#if view.stream === 'missing'}
    <div class="banner warn" role="alert">This session does not exist or was forgotten.</div>
    <p><a href="/" onclick={router.link}>Back to search</a></p>
  {:else if view.stream === 'unavailable'}
    <div class="banner warn" role="status" data-testid="live-unavailable">
      {view.refused === 0 ? 'The server is not reachable.' : 'The server does not stream this session right now.'}
      {#if retryIn !== null}Retrying in {retryIn} s.{/if}
      <button type="button" onclick={() => void check()}>Retry now</button>
    </div>
  {:else if view.stream === 'offline'}
    <div class="banner warn" role="status">The recorder stopped without ending the session. This is the last screen it recorded.</div>
  {:else if view.stream === 'ended'}
    <div class="banner" role="status">
      The session ended. <a href={sessionHref(uuid)} onclick={router.link}>Watch the recording</a> with chapters and search.
    </div>
  {/if}

  <div class="player" class:hidden={!streaming} data-testid="live-player">
    <FitBox cols={term.cols} rows={term.rows} mode={prefs.fit} controls={false} bind:mount={el} bind:effective={fitEffective} />
  </div>

  {#if keys.length && streaming}
    <div class="keys" aria-label="Recent keystrokes" data-testid="live-keystrokes">
      {#each keys as k, i (i)}<kbd>{k}</kbd>{/each}
    </div>
  {/if}

  {#if view.stream === 'unavailable' && view.refused !== 0}
    <div class="fallback" data-testid="live-fallback">
      <p class="muted">The recording so far:</p>
      <SessionView {uuid} target={{}} {search} />
    </div>
  {/if}
</section>

<style>
  /* As tall as the page's pane: the player takes what the header and banners leave. (Not
     `.live`, which is also the badge's class.) */
  section.live {
    display: flex;
    flex-direction: column;
    gap: 0.5rem;
    min-width: 0;
    min-height: 100cqh;
    padding: 0.6rem;
  }
  section.live > * {
    min-width: 0;
  }
  .head {
    display: flex;
    flex-wrap: wrap;
    gap: 0.4rem 1rem;
    justify-content: space-between;
  }
  h2 {
    display: flex;
    flex-wrap: wrap;
    gap: 0.4rem;
    align-items: center;
    margin: 0;
    font-size: 1.05rem;
  }
  .meta,
  .links {
    display: flex;
    flex-wrap: wrap;
    gap: 0.1rem 0.8rem;
    font-size: 0.88em;
  }
  .actions {
    display: flex;
    flex-wrap: wrap;
    gap: 0.3rem;
    align-items: flex-start;
  }
  .state {
    padding: 0 0.5em;
    border-radius: 999px;
    border: 1px solid var(--border);
    font-size: 0.75rem;
    font-weight: 600;
    line-height: 1.6;
    white-space: nowrap;
    color: var(--fg-muted);
  }
  .state.live {
    background: var(--live);
    border-color: var(--live);
    color: #fff;
    letter-spacing: 0.04em;
  }
  .state.paused {
    border-color: var(--pause);
    color: var(--pause);
  }
  .state.degraded,
  .state.offline,
  .state.missing,
  .state.unavailable {
    border-color: var(--fail);
    color: var(--fail);
  }
  .banner {
    display: flex;
    flex-wrap: wrap;
    gap: 0.3rem 0.6rem;
    align-items: center;
  }
  .player {
    position: relative;
    flex: 1 0 12rem;
  }
  .player.hidden {
    display: none;
  }
  .keys {
    display: flex;
    gap: 0.15rem;
    overflow: hidden;
    max-width: 100%;
    font-size: 0.9em;
  }
  .fallback {
    display: grid;
    gap: 0.3rem;
    border-top: 1px solid var(--border);
    padding-top: 0.4rem;
  }
  .fallback p {
    margin: 0;
  }
</style>
