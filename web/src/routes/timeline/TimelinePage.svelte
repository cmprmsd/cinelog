<script lang="ts">
  // Team timeline (work package M4-B; plan §8.3, §15.2; docs/API.md §9.3). Route
  // `/timeline?from=&to=&group=user|pane`, mounted by `Shell` without props. A click on a
  // command opens its session below the chart (`SessionView`, commands and player); the
  // opened command is kept in the URL as `open=<uuid>&cmd=<seq>`.
  import { api, errorText, isAbort } from '../../lib/api/client';
  import { bus } from '../../lib/api/events.svelte';
  import type { Timeline, TimelineBlock, TimelineGroup } from '../../lib/api/types';
  import SessionView from '../../lib/components/SessionView.svelte';
  import { EMPTY } from '../../lib/filters';
  import { fmtDuration, fmtTs, shortId, tzLabel } from '../../lib/format';
  import { focusKind } from '../../lib/keys';
  import { prefs } from '../../lib/prefs.svelte';
  import { isUuid, sessionHref } from '../../lib/route';
  import { router } from '../../lib/router.svelte';
  import TimelineChart from './TimelineChart.svelte';
  import {
    DAY,
    HOUR,
    dayOf,
    dayWindow,
    panWindow,
    viewFromQuery,
    viewToQuery,
    withOpen,
    zoomWindow,
    type TimelineView,
  } from './view';

  /** Follow mode re-reads the window this often (and the bus triggers reloads sooner). */
  const FOLLOW_MS = 60_000;
  const BUS_DEBOUNCE_MS = 1000;

  const viewKey = $derived(JSON.stringify(viewFromQuery(router.query)));
  const view = $derived(JSON.parse(viewKey) as TimelineView);
  const follow = $derived(view.from === null);

  /** Never mutated, and a week holds ~20k blocks: no deep proxy. */
  let data = $state.raw<Timeline | null>(null);
  let receivedAt = $state(0);
  let loading = $state(false);
  let error = $state<string | null>(null);
  let reload = $state(0);
  let now = $state(Date.now() / 1000);
  let collapse = $state(true);

  $effect(() => {
    const v = JSON.parse(viewKey) as TimelineView;
    void reload;
    const ctl = new AbortController();
    loading = true;
    const params = v.from !== null && v.to !== null ? { from: v.from, to: v.to, group: v.group } : { group: v.group };
    api.timeline(params, ctl.signal).then(
      (tl) => {
        now = Date.now() / 1000;
        receivedAt = performance.now();
        data = tl;
        error = null;
        loading = false;
      },
      (e) => {
        if (isAbort(e)) return;
        error = errorText(e);
        loading = false;
      },
    );
    return () => ctl.abort();
  });

  $effect(() => {
    if (!follow) return;
    const id = setInterval(() => (reload += 1), FOLLOW_MS);
    return () => clearInterval(id);
  });

  $effect(() => {
    let timer: ReturnType<typeof setTimeout> | undefined;
    const off = bus.subscribe((ev) => {
      switch (ev.type) {
        case 'session-start':
        case 'session-end':
        case 'annotation':
        case 'forgotten':
          clearTimeout(timer);
          timer = setTimeout(() => (reload += 1), BUS_DEBOUNCE_MS);
      }
    });
    return () => {
      clearTimeout(timer);
      off();
    };
  });

  // ---------------------------------------------------------------- window

  /**
   * The window the controls act on: the requested one (so quick key presses compound while a
   * response is on its way), or in follow mode the one the server picked.
   */
  const shown = $derived(
    view.from !== null && view.to !== null ? { from: view.from, to: view.to } : data ? { from: data.from, to: data.to } : null,
  );

  function setView(v: TimelineView): void {
    router.setQuery(viewToQuery(v, router.query));
  }

  function setWindow(w: { from: number; to: number } | null): void {
    setView({ from: w?.from ?? null, to: w?.to ?? null, group: view.group });
  }

  function zoom(factor: number): void {
    if (shown) setWindow(zoomWindow(shown.from, shown.to, factor));
  }

  function pan(fraction: number): void {
    if (shown) setWindow(panWindow(shown.from, shown.to, fraction));
  }

  function last(span: number): void {
    const t = Date.now() / 1000;
    setWindow({ from: t - span, to: t });
  }

  function setGroup(group: TimelineGroup): void {
    setView({ ...view, group });
  }

  const day = $derived(shown ? dayOf(shown.from, prefs.utc) : '');

  function pickDay(e: Event): void {
    const w = dayWindow((e.currentTarget as HTMLInputElement).value, prefs.utc);
    if (w) setWindow(w);
  }

  function onkeydown(e: KeyboardEvent): void {
    if (e.ctrlKey || e.metaKey || e.altKey || e.defaultPrevented) return;
    const t = e.target;
    if (focusKind(t) === 'typing') return;
    if (t instanceof Element && t.closest('dialog[open], .ap-player')) return;
    if (e.key === '+' || e.key === '=') zoom(0.5);
    else if (e.key === '-') zoom(2);
    else return;
    e.preventDefault();
  }

  // ---------------------------------------------------------------- opened session

  const openUuid = $derived.by(() => {
    const u = router.query.get('open');
    return u && isUuid(u) ? u.toLowerCase() : null;
  });
  const openCmd = $derived.by(() => {
    const v = Number(router.query.get('cmd') ?? NaN);
    return Number.isInteger(v) && v >= 0 ? v : null;
  });
  /** A new object seeks again (a second click on the same command). */
  let target = $derived<{ cmd: number | null }>({ cmd: openCmd });

  function setOpen(uuid: string | null, cmd: number | null): void {
    router.setQuery(withOpen(router.query, uuid, cmd), { replace: true });
  }

  function openBlock(b: TimelineBlock): void {
    setOpen(b.session, b.seq);
    target = { cmd: b.seq };
  }

  const selected = $derived(openUuid && openCmd !== null ? { session: openUuid, seq: openCmd } : null);

  const rows = $derived(data ? data.lanes.reduce((n, l) => n + l.rows.length, 0) : 0);
  const windowText = $derived(shown ? `${fmtTs(shown.from, prefs.utc)} – ${fmtTs(shown.to, prefs.utc)}` : '');
</script>

<svelte:window {onkeydown} />

<section class="timeline" class:with-session={openUuid !== null} aria-labelledby="timeline-h" data-testid="timeline">
  <header class="bar">
    <h1 id="timeline-h">Timeline</h1>
    <div class="group" role="radiogroup" aria-label="Lanes">
      <label><input type="radio" name="tl-group" checked={view.group === 'user'} onchange={() => setGroup('user')} /> by person</label>
      <label><input type="radio" name="tl-group" checked={view.group === 'pane'} onchange={() => setGroup('pane')} /> by pane</label>
    </div>
    <div class="nav" role="group" aria-label="Window">
      <button type="button" title="Earlier" aria-label="Earlier" onclick={() => pan(-0.5)} data-testid="tl-earlier">◀</button>
      <button type="button" title="Zoom out (-)" aria-label="Zoom out" onclick={() => zoom(2)} data-testid="tl-zoom-out">−</button>
      <button type="button" title="Zoom in (+)" aria-label="Zoom in" onclick={() => zoom(0.5)} data-testid="tl-zoom-in">+</button>
      <button type="button" title="Later" aria-label="Later" onclick={() => pan(0.5)} data-testid="tl-later">▶</button>
      <button type="button" onclick={() => last(HOUR)}>1 h</button>
      <button type="button" aria-pressed={follow} onclick={() => setWindow(null)} data-testid="tl-follow">24 h</button>
      <button type="button" onclick={() => last(7 * DAY)}>7 d</button>
      <label class="day">
        <span class="sr-only">Day</span>
        <input type="date" value={day} onchange={pickDay} data-testid="tl-day" />
      </label>
      <label class="collapse"><input type="checkbox" bind:checked={collapse} /> collapse idle</label>
    </div>
    <p class="summary" aria-live="polite" data-testid="tl-summary">
      <span>{windowText} <span class="tz">{tzLabel(prefs.utc)}</span></span>
      {#if data}
        <span>{data.commands} commands in {rows} {rows === 1 ? 'row' : 'rows'}</span>
        {#if data.aggregated && data.bin_secs}<span class="badge">density, {fmtDuration(data.bin_secs)} bins</span>{/if}
      {/if}
      {#if loading}<span class="muted">loading…</span>{/if}
    </p>
  </header>

  {#if error}
    <p class="error" role="alert" data-testid="tl-error">{error}</p>
  {/if}

  <div class="legend" aria-label="Legend">
    <span><i class="sw ok"></i>exit 0</span>
    <span><i class="sw fail"></i>failed</span>
    <span><i class="sw unknown"></i>unknown</span>
    <span><i class="sw approx"></i>approximate</span>
    <span><i class="sw pause"></i>paused</span>
    <span><i class="sw skip"></i>unrecorded command</span>
    <span><i class="sw nested"></i>nested session</span>
    <span><i class="sw gap"></i>output dropped</span>
    <span><i class="sw forgotten"></i>forgotten</span>
    <span><i class="sw anno"></i>annotation</span>
  </div>

  <div class="chart">
    {#if data}
      {#if data.lanes.length === 0}
        <p class="empty" data-testid="tl-empty">No recorded sessions in this window.</p>
      {:else}
        <TimelineChart
          timeline={data}
          utc={prefs.utc}
          {collapse}
          {now}
          {selected}
          {receivedAt}
          onopen={openBlock}
          onopensession={(uuid) => setOpen(uuid, null)}
          onzoom={(from, to) => setWindow({ from, to })}
        />
      {/if}
    {:else if !error}
      <p class="empty">Loading the timeline…</p>
    {/if}
  </div>

  {#if openUuid}
    <div class="session" data-testid="tl-session">
      <p class="session-bar">
        <a href={sessionHref(openUuid, { cmd: openCmd })} onclick={router.link}>Open session {shortId(openUuid)}</a>
      </p>
      {#key openUuid}
        <SessionView uuid={openUuid} {target} search={EMPTY} onclose={() => setOpen(null, null)} />
      {/key}
    </div>
  {/if}
</section>

<style>
  .timeline {
    display: flex;
    flex-direction: column;
    gap: 0.5rem;
    height: 100%;
    padding: 0.6rem 0.8rem;
    overflow: auto;
  }
  .bar {
    display: flex;
    flex-wrap: wrap;
    gap: 0.4rem 1rem;
    align-items: center;
  }
  h1 {
    margin: 0;
    font-size: 1.2rem;
  }
  .group,
  .nav {
    display: flex;
    flex-wrap: wrap;
    gap: 0.35rem;
    align-items: center;
  }
  .group label,
  .collapse {
    display: inline-flex;
    gap: 0.25rem;
    align-items: center;
    white-space: nowrap;
  }
  .summary {
    display: flex;
    flex-wrap: wrap;
    gap: 0.3rem 0.9rem;
    margin: 0;
    color: var(--fg-muted);
    font-size: 0.9em;
  }
  .tz {
    font-size: 0.85em;
  }
  .badge {
    padding: 0 0.35rem;
    border-radius: var(--radius);
    background: var(--accent-soft);
    color: var(--fg);
  }
  .muted {
    color: var(--fg-muted);
  }
  .error {
    margin: 0;
    padding: 0.4rem 0.6rem;
    border-radius: var(--radius);
    background: var(--fail-soft);
    color: var(--fg);
  }
  .legend {
    display: flex;
    flex-wrap: wrap;
    gap: 0.2rem 0.8rem;
    font-size: 0.8em;
    color: var(--fg-muted);
  }
  .legend span {
    display: inline-flex;
    gap: 0.3rem;
    align-items: center;
  }
  .sw {
    display: inline-block;
    width: 0.9rem;
    height: 0.6rem;
    border-radius: 2px;
  }
  .sw.ok {
    background: var(--ok);
  }
  .sw.fail {
    background: var(--fail);
  }
  .sw.unknown {
    background: var(--unknown);
  }
  .sw.approx {
    background: repeating-linear-gradient(45deg, var(--ok) 0 3px, var(--bg-elev) 3px 5px);
  }
  .sw.pause {
    background: repeating-linear-gradient(-45deg, var(--pause) 0 1.5px, transparent 1.5px 5px);
    outline: 1px solid var(--pause);
  }
  .sw.skip {
    width: 0;
    height: 0;
    border-left: 0.35rem solid transparent;
    border-right: 0.35rem solid transparent;
    border-top: 0.45rem solid var(--pause);
    border-radius: 0;
  }
  .sw.nested {
    width: 0.55rem;
    height: 0.55rem;
    background: var(--accent);
    transform: rotate(45deg);
  }
  .sw.gap {
    background: var(--gap);
    opacity: 0.5;
  }
  .sw.forgotten {
    background: var(--forgot);
    opacity: 0.6;
  }
  .sw.anno {
    background: var(--accent-soft);
    outline: 1px solid var(--accent);
  }
  .chart {
    flex: 1 1 20rem;
    min-height: 12rem;
    display: flex;
    flex-direction: column;
  }
  .with-session .chart {
    flex: 0 0 auto;
    max-height: 45vh;
  }
  .empty {
    margin: 1rem 0;
    color: var(--fg-muted);
  }
  .session {
    border-top: 1px solid var(--border);
    padding-top: 0.4rem;
  }
  .session-bar {
    margin: 0 0 0.3rem;
  }
  .sr-only {
    position: absolute;
    width: 1px;
    height: 1px;
    overflow: hidden;
    clip-path: inset(50%);
    white-space: nowrap;
  }
</style>
