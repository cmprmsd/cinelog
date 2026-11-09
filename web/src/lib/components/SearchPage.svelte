<script lang="ts">
  import { knownTags } from '../annotations/tags.svelte';
  import { api, errorText, isAbort } from '../api/client';
  import type { SearchHit } from '../api/types';
  import { addTag, stateFromQuery, stateToQuery, toSearchParams } from '../filters';
  import { useKeys } from '../keymap';
  import { appendHits, removeHits, type HitGroup } from '../results';
  import { router } from '../router.svelte';
  import Results from './Results.svelte';
  import SessionView from './SessionView.svelte';

  const DEBOUNCE_MS = 200;

  const search = $derived(stateFromQuery(router.query));
  /** Changes only when the search itself changes (not on unrelated URL keys). */
  const key = $derived(JSON.stringify(search));

  let groups = $state<HitGroup[]>([]);
  let next = $state<string | null>(null);
  let partial = $state(false);
  let capped = $state(false);
  let loading = $state(false);
  let error = $state<string | null>(null);
  /** A cursor whose page failed: not fetched again until the user retries or searches anew. */
  let failedCursor = $state<string | null>(null);
  let active = $state(-1);
  let open = $state<{ hit: SearchHit; target: { t: number; cmd: number | null } } | null>(null);
  let results: Results | undefined = $state();
  let pageCtl: AbortController | null = null;
  let firstRun = true;

  $effect(() => {
    const k = key;
    const ctl = new AbortController();
    pageCtl?.abort();
    pageCtl = ctl;
    loading = true;
    const run = async () => {
      try {
        const r = await api.search(toSearchParams(JSON.parse(k), new Date(), { limit: 100 }), ctl.signal);
        groups = appendHits([], r.hits);
        next = r.next;
        partial = r.partial;
        capped = r.capped;
        error = null;
        failedCursor = null;
        active = -1;
      } catch (e) {
        if (isAbort(e)) return;
        error = errorText(e);
        failedCursor = null;
        groups = [];
        next = null;
      } finally {
        if (pageCtl === ctl) loading = false;
      }
    };
    const timer = setTimeout(run, firstRun ? 0 : DEBOUNCE_MS);
    firstRun = false;
    return () => {
      clearTimeout(timer);
      ctl.abort();
    };
  });

  async function more(): Promise<void> {
    if (!next || loading || next === failedCursor) return;
    const cursor = next;
    const ctl = new AbortController();
    pageCtl = ctl;
    loading = true;
    try {
      const r = await api.search(toSearchParams(search, new Date(), { limit: 100, cursor }), ctl.signal);
      groups = appendHits(groups, r.hits);
      next = r.next;
      partial ||= r.partial;
      capped ||= r.capped;
      error = null;
    } catch (e) {
      if (isAbort(e)) return;
      error = errorText(e);
      failedCursor = cursor;
    } finally {
      if (pageCtl === ctl) loading = false;
    }
  }

  function retryMore(): void {
    failedCursor = null;
    void more();
  }

  /** Forgotten content must leave the list at once, not on the next search. */
  function dropForgotten(session: string, seq: number): void {
    groups = removeHits(groups, (h) => h.session === session && h.cmd_seq === seq);
  }

  /** A result's tag chip narrows the search to that tag. */
  function filterTag(tag: string): void {
    router.setQuery(stateToQuery({ ...search, tags: addTag(search.tags, tag) }, router.query), { replace: true });
  }

  // Chip colours: the known tags, once annotated results show up.
  $effect(() => {
    if (groups.some((g) => g.head.tags.length || g.rest.some((h) => h.tags.length))) void knownTags.load();
  });

  function openHit(hit: SearchHit): void {
    open = { hit, target: { t: hit.t, cmd: hit.cmd_seq } };
  }

  function move(delta: number): void {
    const n = results?.count() ?? 0;
    if (n === 0) return;
    active = Math.max(0, Math.min(n - 1, (active < 0 ? (delta > 0 ? -1 : n) : active) + delta));
    void results?.reveal(active);
  }

  $effect(() =>
    useKeys({
      'next-result': () => move(1),
      'prev-result': () => move(-1),
      'open-result': () => {
        const h = results?.hitAt(active < 0 ? 0 : active);
        if (!h) return false;
        if (active < 0) active = 0;
        openHit(h);
      },
      'open-result-tab': () => {
        const href = results?.hrefAt(active < 0 ? 0 : active);
        if (!href) return false;
        window.open(href, '_blank', 'noopener');
      },
      escape: () => {
        if (!open) return false;
        open = null;
      },
    }),
  );

  const count = $derived(groups.reduce((n, g) => n + 1 + g.rest.length, 0));
</script>

<div class="page" class:split={open !== null}>
  <section class="results" aria-label="Results">
    <div class="summary" role="status" aria-live="polite">
      {#if error}
        <span class="banner warn">{error}</span>
        {#if failedCursor}
          <button type="button" class="retry" onclick={retryMore} data-testid="retry-more">Load more</button>
        {/if}
      {:else if loading && groups.length === 0}
        <span class="muted">Searching…</span>
      {:else}
        <span class="muted" data-testid="result-count"
          >{count}{next ? '+' : ''} {search.q ? `hit${count === 1 ? '' : 's'}` : 'recent commands'}{loading ? ' …' : ''}</span
        >
      {/if}
    </div>
    {#if partial}
      <p class="banner" data-testid="partial">
        Short search term: only the newest matches found within 500 ms are shown. Older matches may exist; use 3 or more
        characters for a complete search.
      </p>
    {/if}
    {#if capped}
      <p class="banner">More than 5000 candidates. Refine the query or add filters.</p>
    {/if}
    {#if !error && !loading && groups.length === 0}
      <p class="muted empty" data-testid="no-results">
        {#if search.tags.length}No {search.q ? 'matches' : 'commands'} annotated {search.tags.map((t) => `#${t}`).join(' ')}.{:else}{search.q
            ? 'No matches.'
            : 'Nothing indexed yet.'}{/if}
      </p>
    {/if}
    <div class="list">
      <Results
        bind:this={results}
        {groups}
        {active}
        openId={open?.hit.id ?? null}
        keep={router.query}
        onopen={openHit}
        onactive={(i) => (active = i)}
        onmore={more}
        ontag={filterTag}
        filtered={search.tags}
      />
    </div>
  </section>
  {#if open}
    <div class="viewer">
      {#key open.hit.session}
        <SessionView
          uuid={open.hit.session}
          target={open.target}
          {search}
          onclose={() => (open = null)}
          onforget={dropForgotten}
        />
      {/key}
    </div>
  {/if}
</div>

<style>
  .page {
    display: grid;
    grid-template-columns: minmax(0, 1fr);
    height: 100%;
    min-height: 0;
  }
  .page.split {
    grid-template-columns: minmax(18rem, 2fr) minmax(0, 3fr);
  }
  .results {
    display: flex;
    flex-direction: column;
    min-height: 0;
    min-width: 0;
    border-right: 1px solid var(--border);
  }
  .summary {
    padding: 0.3rem 0.6rem;
    font-size: 0.85em;
  }
  .banner {
    margin: 0.2rem 0.6rem;
    font-size: 0.85em;
  }
  .retry {
    margin-left: 0.4rem;
    font-size: 0.85em;
    padding: 0.1em 0.5em;
  }
  .empty {
    padding: 0 0.6rem;
  }
  .list {
    flex: 1;
    min-height: 0;
  }
  .viewer {
    overflow: auto;
    min-width: 0;
    min-height: 0;
    /* The session view sizes its player to this pane's height (cqh). */
    container-type: size;
  }
  @media (max-width: 900px) {
    .page.split {
      grid-template-columns: minmax(0, 1fr);
      grid-template-rows: minmax(12rem, 2fr) minmax(0, 3fr);
    }
    .results {
      border-right: 0;
      border-bottom: 1px solid var(--border);
    }
  }
</style>
