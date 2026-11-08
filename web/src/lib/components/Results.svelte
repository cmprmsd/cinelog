<script lang="ts">
  import { tick, untrack } from 'svelte';
  import { SvelteSet } from 'svelte/reactivity';
  import type { SearchHit } from '../api/types';
  import { exitClass } from '../chapters';
  import { fmtTs, oneLine } from '../format';
  import { prefs } from '../prefs.svelte';
  import { flattenRows, scrollToRow, visibleRange, type HitGroup } from '../results';
  import { isPlainClick, sessionHref } from '../route';
  import Snippet from './Snippet.svelte';

  interface Props {
    groups: HitGroup[];
    /** Keyboard cursor (row index). */
    active: number;
    /** Hit shown in the player. */
    openId: string | null;
    keep: URLSearchParams;
    onopen: (hit: SearchHit) => void;
    onactive: (i: number) => void;
    onmore: () => void;
  }
  let { groups, active, openId, keep, onopen, onactive, onmore }: Props = $props();

  const ROW = 56;
  const expanded = new SvelteSet<string>();
  let viewport: HTMLDivElement | undefined = $state();
  let scrollTop = $state(0);
  let height = $state(600);

  const rows = $derived(flattenRows(groups, expanded));
  const range = $derived(visibleRange(scrollTop, height, ROW, rows.length));
  const shown = $derived(rows.slice(range[0], range[1]).map((row, i) => ({ row, index: range[0] + i })));

  export function count(): number {
    return rows.length;
  }

  export function hitAt(i: number): SearchHit | undefined {
    return rows[i]?.hit;
  }

  export function hrefAt(i: number): string | undefined {
    const h = rows[i]?.hit;
    return h ? href(h) : undefined;
  }

  /** Scrolls row `i` into view and focuses it. */
  export async function reveal(i: number): Promise<void> {
    if (!viewport) return;
    const top = scrollToRow(i, viewport.scrollTop, viewport.clientHeight, ROW);
    if (top !== null) {
      viewport.scrollTop = top;
      scrollTop = top;
    }
    await tick();
    viewport.querySelector<HTMLAnchorElement>(`[data-row="${i}"] a.hit`)?.focus({ preventScroll: true });
  }

  // Only scrolling and new rows ask for the next page: `onmore` reads the loader's state,
  // which must not become a dependency (a failed page would re-trigger it at once).
  $effect(() => {
    if (range[1] >= rows.length - 4 && rows.length > 0) untrack(onmore);
  });

  function toggle(key: string): void {
    if (expanded.has(key)) expanded.delete(key);
    else expanded.add(key);
  }

  const href = (h: SearchHit) => sessionHref(h.session, { t: h.t }, keep);

  function click(e: MouseEvent, hit: SearchHit, i: number): void {
    if (!isPlainClick(e)) return;
    e.preventDefault();
    onactive(i);
    onopen(hit);
  }
</script>

<div
  class="viewport"
  bind:this={viewport}
  bind:clientHeight={height}
  onscroll={(e) => (scrollTop = e.currentTarget.scrollTop)}
  role="list"
  aria-label="Search results"
  data-testid="results"
>
  <div class="spacer" style:height={`${rows.length * ROW}px`}>
    {#each shown as { row, index } (row.hit.id)}
      {@const h = row.hit}
      <div
        class="row"
        role="listitem"
        data-row={index}
        data-hit={h.id}
        class:active={index === active}
        class:open={h.id === openId}
        class:sub={!row.head}
        style:top={`${index * ROW}px`}
        style:height={`${ROW}px`}
      >
        <a class="hit" href={href(h)} onclick={(e) => click(e, h, index)} onfocus={() => onactive(index)}>
          <span class="meta">
            <time datetime={new Date(h.ts * 1000).toISOString()}>{fmtTs(h.ts, prefs.utc)}</time>
            <span>{h.user}@{h.host}</span>
            {#if h.pane}<span class="muted">{h.tmux_session ? `${h.tmux_session} ` : ''}{h.pane}</span>{/if}
            <span class={`exit ${exitClass(h.exit)}`} title="exit status">{h.exit ?? '?'}</span>
            {#if h.approx}<span class="badge approx" title="approximate boundaries (legacy import)">approx</span>{/if}
            {#if h.legacy}<span class="badge legacy">legacy</span>{/if}
            {#if h.kind === 'output' && h.typed}<span class="typed muted">$ {oneLine(h.typed)}</span>{/if}
          </span>
          <span class="line">
            {#if h.kind === 'command'}
              <span class="prompt" aria-hidden="true">$</span>
            {:else}
              <span class="badge out">{h.chunk_kind === 'gap' ? 'unattributed output' : 'output'}</span>
            {/if}
            <Snippet parts={h.snippet} />
          </span>
        </a>
        {#if row.head && row.group.rest.length}
          <button
            type="button"
            class="more"
            aria-expanded={expanded.has(row.group.key)}
            onclick={() => toggle(row.group.key)}
            >{expanded.has(row.group.key) ? 'fewer' : `${row.group.rest.length} more hit${row.group.rest.length === 1 ? '' : 's'}`}</button
          >
        {/if}
      </div>
    {/each}
  </div>
</div>

<style>
  .viewport {
    position: relative;
    overflow-y: auto;
    height: 100%;
    min-height: 0;
    contain: strict;
  }
  .spacer {
    position: relative;
  }
  .row {
    position: absolute;
    left: 0;
    right: 0;
    display: flex;
    align-items: stretch;
    border-bottom: 1px solid var(--border);
  }
  .row.sub {
    padding-left: 1.2rem;
    background: var(--bg-sunk);
  }
  .row.active {
    background: var(--bg-hover);
  }
  .row.open {
    box-shadow: inset 3px 0 0 var(--accent);
  }
  .hit {
    flex: 1;
    min-width: 0;
    display: grid;
    align-content: center;
    gap: 0.1rem;
    padding: 0 0.6rem;
    color: var(--fg);
    text-decoration: none;
  }
  .hit:focus-visible {
    outline-offset: -2px;
  }
  .meta,
  .line {
    display: flex;
    gap: 0.5rem;
    align-items: center;
    min-width: 0;
    white-space: nowrap;
    overflow: hidden;
  }
  .meta {
    font-size: 0.8em;
    color: var(--fg-muted);
  }
  .meta time {
    font-family: var(--mono);
  }
  .typed {
    overflow: hidden;
    text-overflow: ellipsis;
    font-family: var(--mono);
  }
  .line {
    font-size: 0.95em;
  }
  .line :global(.snippet) {
    overflow: hidden;
    text-overflow: ellipsis;
  }
  .prompt {
    color: var(--fg-muted);
    font-family: var(--mono);
  }
  .badge.out {
    flex: none;
  }
  .more {
    align-self: center;
    margin-right: 0.5rem;
    font-size: 0.8em;
    padding: 0.1em 0.5em;
  }
</style>
