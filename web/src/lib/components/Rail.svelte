<script lang="ts">
  import type { Command } from '../api/types';
  import { exitClass, pct } from '../chapters';
  import { fmtClock } from '../format';
  import type { Band, NestedPoint } from '../markers';
  import { router } from '../router.svelte';
  import { rangeOf, type AnnotationSpan, type TimeRange } from '../spans';

  // A click seeks; a drag selects a span (`selection`, for annotations and exports) and
  // `Esc` in the session view clears it. Annotation spans are drawn as bands.
  interface Props {
    duration: number;
    commands: Command[];
    bands: Band[];
    nested: NestedPoint[];
    hits: number[];
    currentTime: number;
    onseek: (t: number) => void;
    selection?: TimeRange | null;
    annotations?: AnnotationSpan[];
  }
  let {
    duration,
    commands,
    bands,
    nested,
    hits,
    currentTime,
    onseek,
    selection = $bindable(null),
    annotations = [],
  }: Props = $props();

  let rail: HTMLDivElement | undefined = $state();

  /** Pointer travel (px) before a press becomes a drag instead of a click. */
  const DRAG_PX = 4;

  let press: { id: number; x: number; t: number; dragging: boolean } | null = null;
  /** The click that ends a drag must not seek. */
  let swallowClick = false;

  function timeAt(clientX: number): number {
    if (!rail || !(duration > 0)) return 0;
    const r = rail.getBoundingClientRect();
    return Math.max(0, Math.min(duration, ((clientX - r.left) / r.width) * duration));
  }

  function click(e: MouseEvent): void {
    if (swallowClick) {
      swallowClick = false;
      return;
    }
    if (!rail || !(duration > 0)) return;
    onseek(timeAt(e.clientX));
  }

  function pointerdown(e: PointerEvent): void {
    if (e.button !== 0 || !(duration > 0)) return;
    swallowClick = false;
    press = { id: e.pointerId, x: e.clientX, t: timeAt(e.clientX), dragging: false };
  }

  function pointermove(e: PointerEvent): void {
    if (!press || e.pointerId !== press.id) return;
    if (!press.dragging) {
      if (Math.abs(e.clientX - press.x) < DRAG_PX) return;
      press.dragging = true;
      rail?.setPointerCapture?.(e.pointerId);
    }
    selection = rangeOf(press.t, timeAt(e.clientX), duration);
  }

  function pointerup(e: PointerEvent): void {
    if (!press || e.pointerId !== press.id) return;
    if (press.dragging) {
      swallowClick = true;
      const s = rangeOf(press.t, timeAt(e.clientX), duration);
      selection = s.to > s.from ? s : null;
      if (rail?.hasPointerCapture?.(e.pointerId)) rail.releasePointerCapture(e.pointerId);
    }
    press = null;
  }

  function pointercancel(): void {
    press = null;
  }

  function key(e: KeyboardEvent): void {
    const step = e.shiftKey ? 30 : 5;
    if (e.key === 'ArrowLeft') onseek(Math.max(0, currentTime - step));
    else if (e.key === 'ArrowRight') onseek(Math.min(duration, currentTime + step));
    else if (e.key === 'Home') onseek(0);
    else if (e.key === 'End') onseek(duration);
    else return;
    e.preventDefault();
    e.stopPropagation();
  }

  const width = (b: Band) => Math.max(0.4, pct(b.to, duration) - pct(b.from, duration));
</script>

<div class="wrap">
  <div
    class="rail"
    bind:this={rail}
    role="slider"
    tabindex="0"
    aria-label="Session timeline"
    aria-valuemin={0}
    aria-valuemax={Math.round(duration)}
    aria-valuenow={Math.round(currentTime)}
    aria-valuetext={`${fmtClock(currentTime)} of ${fmtClock(duration)}`}
    onclick={click}
    onkeydown={key}
    onpointerdown={pointerdown}
    onpointermove={pointermove}
    onpointerup={pointerup}
    onpointercancel={pointercancel}
    title="Click to seek, drag to select a span"
    data-testid="rail"
  >
    {#each bands as b, i (i)}
      <span class={`band ${b.kind}`} style:left={`${pct(b.from, duration)}%`} style:width={`${width(b)}%`} title={b.label}></span>
    {/each}
    {#each annotations as a (a.uuid)}
      <span
        class="band note"
        style:left={`${pct(a.from, duration)}%`}
        style:width={`${Math.max(0.4, pct(a.to, duration) - pct(a.from, duration))}%`}
        style:background={a.color}
        title={a.label}
        data-testid="rail-annotation"
        data-uuid={a.uuid}
      ></span>
    {/each}
    {#if selection}
      <span
        class="band selection"
        style:left={`${pct(selection.from, duration)}%`}
        style:width={`${Math.max(0.4, pct(selection.to, duration) - pct(selection.from, duration))}%`}
        data-testid="rail-selection"
        data-from={selection.from}
        data-to={selection.to}
        title={`Selection ${fmtClock(selection.from)}–${fmtClock(selection.to)} (Esc clears)`}
      ></span>
    {/if}
    {#each commands as c (c.seq)}
      <span class={`tick ${exitClass(c.exit)}`} style:left={`${pct(c.t_start, duration)}%`}></span>
    {/each}
    {#each hits as h, i (i)}
      <span class="hit" style:left={`${pct(h, duration)}%`}></span>
    {/each}
    <span class="now" style:left={`${pct(currentTime, duration)}%`}></span>
  </div>
  {#if bands.length || nested.length}
    <ul class="legend" aria-label="Recorder markers" data-testid="markers">
      {#each bands as b, i (i)}
        <li>
          <button type="button" class={`mk ${b.kind}`} onclick={() => onseek(b.from)}>
            {b.label} <span class="muted">{fmtClock(b.from)}–{fmtClock(b.to)}</span>
          </button>
        </li>
      {/each}
      {#each nested as n, i (i)}
        <li>
          {#if n.external}
            <!-- The child is served by the other scope's server (a TEAM `sudo -i`): a new tab, its own sign-in. -->
            <a class="btn mk nested" href={n.external} target="_blank" rel="noopener noreferrer" title="Opens on the server that recorded it"
              >↗ {n.label} <span class="muted">{fmtClock(n.t)}</span></a
            >
          {:else if n.child}
            <a class="btn mk nested" href={`/s/${n.child}`} onclick={router.link}>↘ {n.label} <span class="muted">{fmtClock(n.t)}</span></a>
          {:else}
            <button type="button" class="mk nested" onclick={() => onseek(n.t)}>{n.label} <span class="muted">{fmtClock(n.t)}</span></button>
          {/if}
        </li>
      {/each}
    </ul>
  {/if}
</div>

<style>
  .wrap {
    display: grid;
    gap: 0.3rem;
  }
  .rail {
    position: relative;
    height: 18px;
    border-radius: 4px;
    background: var(--bg-sunk);
    border: 1px solid var(--border);
    cursor: pointer;
    overflow: hidden;
    /* Horizontal drags select; vertical swipes still scroll the page. */
    touch-action: pan-y;
    user-select: none;
  }
  .rail > span {
    position: absolute;
    top: 0;
    bottom: 0;
    pointer-events: none;
  }
  .band {
    opacity: 0.45;
  }
  .band.pause {
    background: repeating-linear-gradient(135deg, var(--pause) 0 4px, transparent 4px 8px);
  }
  .band.gap {
    background: var(--gap);
  }
  .band.forgotten {
    background: repeating-linear-gradient(45deg, var(--forgot) 0 3px, transparent 3px 6px);
  }
  .band.selection {
    background: var(--accent);
    opacity: 0.25;
  }
  .tick {
    width: 2px;
    margin-left: -1px;
    top: 9px !important;
  }
  .tick.ok {
    background: var(--ok);
  }
  .tick.fail {
    background: var(--fail);
  }
  .tick.unknown {
    background: var(--unknown);
  }
  .hit {
    width: 3px;
    margin-left: -1px;
    bottom: 9px !important;
    background: var(--mark-bg);
  }
  .now {
    width: 2px;
    margin-left: -1px;
    background: var(--accent);
  }
  .legend {
    display: flex;
    flex-wrap: wrap;
    gap: 0.3rem;
    margin: 0;
    padding: 0;
    list-style: none;
  }
  .mk {
    font-size: 0.8em;
    padding: 0.1em 0.5em;
    border-left-width: 4px;
  }
  .mk.pause {
    border-left-color: var(--pause);
  }
  .mk.gap {
    border-left-color: var(--gap);
  }
  .mk.forgotten {
    border-left-color: var(--forgot);
  }
  .mk.nested {
    border-left-color: var(--accent);
  }
</style>
