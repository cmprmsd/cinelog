<script lang="ts">
  import { normalizeTag } from '../annotations/annotations';
  import { knownTags } from '../annotations/tags.svelte';
  import type { SearchIn } from '../api/types';
  import {
    addTag,
    chips,
    clearFilter,
    DATE_LABELS,
    EMPTY,
    fromLocalInput,
    MAX_FILTER_TAGS,
    normalizeExit,
    stateFromQuery,
    stateToQuery,
    toLocalInput,
    type DatePreset,
    type FilterKey,
    type SearchState,
  } from '../filters';
  import { fmtTs } from '../format';
  import { prefs } from '../prefs.svelte';
  import { router } from '../router.svelte';

  interface Props {
    /** Enter in the field; with `detached` it gets the search to run. */
    onsubmit?: (query?: URLSearchParams) => void;
    /**
     * The page's URL is not a search (the annotations overview has its own `q` and `tag`):
     * the bar keeps its state to itself until Enter.
     */
    detached?: boolean;
  }
  let { onsubmit, detached = false }: Props = $props();

  let local = $state<SearchState>(EMPTY);
  const cur = $derived(detached ? local : stateFromQuery(router.query));
  const active = $derived(chips(cur, (ts) => fmtTs(ts, prefs.utc).slice(0, 16)));
  let input: HTMLInputElement | undefined = $state();
  let filtersOpen = $state(false);
  let tagError = $state<string | null>(null);

  // A fresh start whenever the bar detaches from (or re-attaches to) the URL.
  $effect(() => {
    void detached;
    local = EMPTY;
  });

  $effect(() => {
    if (filtersOpen || cur.tags.length) void knownTags.load();
  });

  export function focus(): void {
    input?.focus();
    input?.select();
  }

  function set(s: SearchState): void {
    if (detached) local = s;
    else router.setQuery(stateToQuery(s, router.query), { replace: true });
  }

  function update(patch: Partial<SearchState>): void {
    set({ ...cur, ...patch });
  }

  function remove(key: FilterKey, value?: string): void {
    set(clearFilter(cur, key, value));
  }

  function onkeydown(e: KeyboardEvent): void {
    if (e.key === 'Enter') {
      e.preventDefault();
      input?.blur();
      onsubmit?.(detached ? stateToQuery(cur) : undefined);
    }
  }

  /** Adds the tag typed or picked in the filter's tag field. */
  function takeTag(field: HTMLInputElement): void {
    const raw = field.value.trim().replace(/^#/, '');
    if (!raw) return;
    const n = normalizeTag(raw);
    if (!n) {
      tagError = `"${raw}" is not a tag: 1–40 letters, digits or - _ . : / + # @.`;
      return;
    }
    if (!cur.tags.includes(n) && cur.tags.length >= MAX_FILTER_TAGS) {
      tagError = `At most ${MAX_FILTER_TAGS} tags.`;
      return;
    }
    tagError = null;
    field.value = '';
    update({ tags: addTag(cur.tags, n) });
  }

  function tagInput(e: Event): void {
    const field = e.currentTarget as HTMLInputElement;
    tagError = null;
    // A suggestion picked from the list replaces the text at once: take it.
    const kind = (e as InputEvent).inputType;
    if (kind === 'insertReplacementText' || (kind === undefined && knownTags.list.some((t) => t.name === field.value))) takeTag(field);
  }

  function tagKey(e: KeyboardEvent & { currentTarget: HTMLInputElement }): void {
    if (e.key === 'Enter' || e.key === ',') {
      e.preventDefault();
      takeTag(e.currentTarget);
    }
  }

  const MODES: { value: SearchIn; label: string; title: string }[] = [
    { value: 'cmd', label: 'cmd', title: 'Search commands (typed, expanded, cwd)' },
    { value: 'out', label: 'out', title: 'Search command output' },
    { value: 'both', label: 'both', title: 'Search commands and output' },
  ];

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
      {#each active as c (`${c.key}:${c.value ?? ''}`)}
        <li class:tag={c.key === 'tag'} style:--c={c.value ? knownTags.color(c.value) : undefined} data-testid={c.key === 'tag' ? 'search-tag-chip' : undefined}>
          <span>{c.label}</span>
          <button type="button" class="x" aria-label={`Remove filter ${c.label}`} onclick={() => remove(c.key, c.value)}>×</button>
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
      <label title="Only commands (and spans) with an annotation carrying this tag; several tags: all of them"
        >tag <input
          type="text"
          list="search-tag-list"
          placeholder="#tag"
          autocomplete="off"
          spellcheck="false"
          aria-invalid={tagError !== null}
          aria-describedby={tagError ? 'search-tag-err' : undefined}
          oninput={tagInput}
          onkeydown={tagKey}
          onchange={(e) => takeTag(e.currentTarget)}
          data-testid="search-tag-input"
        /></label
      >
      <datalist id="search-tag-list">
        {#each knownTags.list.filter((t) => !cur.tags.includes(t.name)) as t (t.name)}
          <option value={t.name}>{t.count ? `${t.count}×` : ''}</option>
        {/each}
      </datalist>
      <label class="check"
        ><input type="checkbox" checked={cur.hidden} onchange={(e) => update({ hidden: e.currentTarget.checked })} /> include hidden</label
      >
      {#if tagError}<span class="err" id="search-tag-err" role="alert">{tagError}</span>{/if}
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
  .chips li.tag {
    border-color: var(--c);
    border-left-width: 4px;
    background: transparent;
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
  .filters .err {
    flex-basis: 100%;
    color: var(--fail);
    font-size: 0.85em;
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
