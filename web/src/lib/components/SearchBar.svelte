<script lang="ts">
  import type { SearchIn } from '../api/types';
  import {
    chips,
    clearFilter,
    DATE_LABELS,
    normalizeExit,
    stateFromQuery,
    stateToQuery,
    type DatePreset,
    type FilterKey,
    type SearchState,
  } from '../filters';
  import { fmtTs } from '../format';
  import { prefs } from '../prefs.svelte';
  import { router } from '../router.svelte';

  let { onsubmit }: { onsubmit?: () => void } = $props();

  const cur = $derived(stateFromQuery(router.query));
  const active = $derived(chips(cur, (ts) => fmtTs(ts, prefs.utc).slice(0, 16)));
  let input: HTMLInputElement | undefined = $state();
  let filtersOpen = $state(false);

  export function focus(): void {
    input?.focus();
    input?.select();
  }

  function update(patch: Partial<SearchState>): void {
    router.setQuery(stateToQuery({ ...cur, ...patch }, router.query), { replace: true });
  }

  function remove(key: FilterKey): void {
    router.setQuery(stateToQuery(clearFilter(cur, key), router.query), { replace: true });
  }

  function onkeydown(e: KeyboardEvent): void {
    if (e.key === 'Enter') {
      e.preventDefault();
      input?.blur();
      onsubmit?.();
    }
  }

  const MODES: { value: SearchIn; label: string; title: string }[] = [
    { value: 'cmd', label: 'cmd', title: 'Search commands (typed, expanded, cwd)' },
    { value: 'out', label: 'out', title: 'Search command output' },
    { value: 'both', label: 'both', title: 'Search commands and output' },
  ];

  // `datetime-local` works in local time; the URL keeps epoch seconds.
  const toLocalInput = (ts: number | null) => {
    if (ts === null) return '';
    const d = new Date(ts * 1000);
    const p = (n: number) => String(n).padStart(2, '0');
    return `${d.getFullYear()}-${p(d.getMonth() + 1)}-${p(d.getDate())}T${p(d.getHours())}:${p(d.getMinutes())}`;
  };
  const fromLocalInput = (v: string): number | null => {
    if (!v) return null;
    const t = new Date(v).getTime();
    return Number.isFinite(t) ? Math.floor(t / 1000) : null;
  };

  function setDate(preset: DatePreset): void {
    if (preset === 'custom') {
      const now = Math.floor(Date.now() / 1000);
      update({ date: 'custom', from: cur.from ?? now - 86400, to: cur.to });
    } else {
      update({ date: preset, from: null, to: null });
    }
  }

  const exitChoice = $derived(cur.exit === '' || cur.exit === 'ok' || cur.exit === 'fail' ? cur.exit : 'code');
</script>

<div class="searchbar">
  <div class="row">
    <label class="sr-only" for="q">Search</label>
    <input
      id="q"
      bind:this={input}
      type="search"
      placeholder="Search…  (press / )"
      autocomplete="off"
      spellcheck="false"
      value={cur.q}
      oninput={(e) => update({ q: e.currentTarget.value })}
      {onkeydown}
      data-testid="search-input"
    />
    <div class="modes" role="radiogroup" aria-label="Search in">
      {#each MODES as m (m.value)}
        <button
          type="button"
          role="radio"
          aria-checked={cur.in === m.value}
          title={m.title}
          class:on={cur.in === m.value}
          onclick={() => update({ in: m.value })}>{m.label}</button
        >
      {/each}
    </div>
    <button
      type="button"
      aria-pressed={cur.case}
      title="Case-sensitive"
      onclick={() => update({ case: !cur.case })}><span aria-hidden="true">Aa</span><span class="sr-only">Case-sensitive</span></button
    >
    <button type="button" aria-expanded={filtersOpen} aria-controls="filters" onclick={() => (filtersOpen = !filtersOpen)}
      >Filters{#if active.length}&nbsp;<span class="count">{active.length}</span>{/if}</button
    >
  </div>

  {#if active.length}
    <ul class="chips" aria-label="Active filters">
      {#each active as c (c.key)}
        <li>
          <span>{c.label}</span>
          <button type="button" class="x" aria-label={`Remove filter ${c.label}`} onclick={() => remove(c.key)}>×</button>
        </li>
      {/each}
    </ul>
  {/if}

  {#if filtersOpen}
    <div class="filters" id="filters">
      <label>user <input type="text" value={cur.user} onchange={(e) => update({ user: e.currentTarget.value.trim() })} /></label>
      <label>host <input type="text" value={cur.host} onchange={(e) => update({ host: e.currentTarget.value.trim() })} /></label>
      <label
        >date
        <select value={cur.date} onchange={(e) => setDate(e.currentTarget.value as DatePreset)}>
          {#each Object.entries(DATE_LABELS) as [value, label] (value)}<option {value}>{label}</option>{/each}
        </select>
      </label>
      {#if cur.date === 'custom'}
        <label
          >from <input
            type="datetime-local"
            value={toLocalInput(cur.from)}
            onchange={(e) => update({ from: fromLocalInput(e.currentTarget.value) })}
          /></label
        >
        <label
          >to <input
            type="datetime-local"
            value={toLocalInput(cur.to)}
            onchange={(e) => update({ to: fromLocalInput(e.currentTarget.value) })}
          /></label
        >
      {/if}
      <label
        >exit
        <select
          value={exitChoice}
          onchange={(e) => {
            const v = e.currentTarget.value;
            update({ exit: v === 'code' ? '1' : v });
          }}
        >
          <option value="">any</option>
          <option value="ok">0</option>
          <option value="fail">≠0</option>
          <option value="code">code…</option>
        </select>
      </label>
      {#if exitChoice === 'code'}
        <label
          >code <input
            type="number"
            class="narrow"
            value={cur.exit}
            onchange={(e) => update({ exit: normalizeExit(e.currentTarget.value) })}
          /></label
        >
      {/if}
      <label title="Commands run in this directory or below it"
        >cwd <input type="text" value={cur.cwd} placeholder="/dir (and below)" onchange={(e) => update({ cwd: e.currentTarget.value.trim() })} /></label>
      <label>pane <input type="text" class="narrow" value={cur.pane} placeholder="%3" onchange={(e) => update({ pane: e.currentTarget.value.trim() })} /></label>
      <label class="check"
        ><input type="checkbox" checked={cur.hidden} onchange={(e) => update({ hidden: e.currentTarget.checked })} /> include hidden</label
      >
    </div>
  {/if}
</div>

<style>
  .searchbar {
    display: grid;
    gap: 0.4rem;
    min-width: 0;
    flex: 1;
  }
  .row {
    display: flex;
    flex-wrap: wrap;
    gap: 0.4rem;
    align-items: center;
    min-width: 0;
  }
  input[type='search'] {
    flex: 1 1 10rem;
    min-width: 6rem;
    font-family: var(--mono);
    padding: 0.4em 0.6em;
  }
  .modes {
    display: inline-flex;
  }
  .modes button {
    border-radius: 0;
    margin-left: -1px;
  }
  .modes button:first-child {
    border-radius: var(--radius) 0 0 var(--radius);
    margin-left: 0;
  }
  .modes button:last-child {
    border-radius: 0 var(--radius) var(--radius) 0;
  }
  .modes button.on {
    background: var(--accent);
    border-color: var(--accent);
    color: var(--accent-fg);
    position: relative;
  }
  .count {
    display: inline-block;
    min-width: 1.3em;
    border-radius: 999px;
    background: var(--accent);
    color: var(--accent-fg);
    font-size: 0.8em;
    text-align: center;
  }
  .chips {
    display: flex;
    flex-wrap: wrap;
    gap: 0.3rem;
    margin: 0;
    padding: 0;
    list-style: none;
  }
  .chips li {
    display: inline-flex;
    align-items: center;
    gap: 0.2rem;
    padding: 0 0 0 0.6em;
    border: 1px solid var(--accent);
    border-radius: 999px;
    background: var(--accent-soft);
    font-size: 0.85em;
  }
  .chips .x {
    border: 0;
    background: transparent;
    padding: 0 0.5em;
    border-radius: 999px;
  }
  .filters {
    display: flex;
    flex-wrap: wrap;
    gap: 0.5rem 0.9rem;
    padding: 0.5rem;
    border: 1px solid var(--border);
    border-radius: var(--radius);
    background: var(--bg-elev);
  }
  .filters label {
    display: inline-flex;
    gap: 0.35rem;
    align-items: center;
    color: var(--fg-muted);
  }
  .filters input[type='text'] {
    width: 9rem;
  }
  .filters .narrow {
    width: 5rem;
  }
  @media (max-width: 760px) {
    input[type='search'] {
      flex-basis: 100%;
    }
  }
  /* Touch-sized controls. */
  @media (pointer: coarse), (max-width: 760px) {
    button,
    input,
    select {
      min-height: 40px;
    }
    .row button {
      min-width: 40px;
      justify-content: center;
    }
    .chips .x {
      min-width: 40px;
      justify-content: center;
    }
  }
</style>
