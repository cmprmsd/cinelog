<script lang="ts">
  // Annotations overview (docs/API.md §9.4 "Overview", §9.6): every annotation of this
  // server (TEAM: of all members), newest first, with where it sits. Route
  // `/annotations?tag=&author=&q=&date=&from=&to=`, mounted by `Shell` without props. Filters
  // live in the URL (a change pushes a history entry, typing in the text field replaces it);
  // pages follow `next` on scroll; the UI bus keeps the list current without a reload.
  import { tick, untrack } from 'svelte';
  import {
    appendPage,
    belongsLoaded,
    clearOverview,
    insertDesc,
    isFiltered,
    markText,
    matches,
    MAX_Q,
    OVERVIEW_EMPTY,
    overviewChips,
    overviewFromQuery,
    overviewToQuery,
    preview,
    toAnnotationParams,
    type OverviewKey,
    type OverviewState,
  } from '../../lib/annotations/overview';
  import type { Tombstones } from '../../lib/annotations/annotations';
  import { knownTags } from '../../lib/annotations/tags.svelte';
  import { api, errorText, isAbort } from '../../lib/api/client';
  import { bus } from '../../lib/api/events.svelte';
  import type { Annotation, AnnotationContext, AnnotationEvent, AnnotationParams, Author } from '../../lib/api/types';
  import { exitClass } from '../../lib/chapters';
  import { addTag, DATE_LABELS, fromLocalInput, MAX_FILTER_TAGS, toLocalInput, type DatePreset } from '../../lib/filters';
  import { fmtClock, fmtTs, oneLine, shortId, tzLabel } from '../../lib/format';
  import { useKeys } from '../../lib/keymap';
  import { prefs } from '../../lib/prefs.svelte';
  import { parseRoute, sessionHref } from '../../lib/route';
  import { router } from '../../lib/router.svelte';

  const Q_DEBOUNCE_MS = 250;
  const BUS_DEBOUNCE_MS = 1000;
  /** Load the next page when the list's end is this close (px). */
  const NEAR_END = 400;

  const viewKey = $derived(JSON.stringify(overviewFromQuery(router.query)));
  const view = $derived(JSON.parse(viewKey) as OverviewState);

  /** Never mutated in place: no deep proxies. */
  let rows = $state.raw<Annotation[]>([]);
  let contexts = $state.raw<Record<string, AnnotationContext>>({});
  let next = $state<string | null>(null);
  let loading = $state(false);
  /** The first page of the current filters arrived (or failed). */
  let loaded = $state(false);
  let error = $state<string | null>(null);
  /** A cursor whose page failed: not fetched again until the user retries. */
  let failedCursor = $state<string | null>(null);
  let active = $state(-1);
  let reload = $state(0);
  let authors = $state<Author[]>([]);
  let scroller: HTMLElement | undefined = $state();
  let listEl: HTMLElement | undefined = $state();

  /** Load generation: a newer first page drops older results. */
  let gen = 0;
  let pageCtl: AbortController | null = null;
  /** The first page's parameters: later pages and bus lookups use the same bounds. */
  let base: AnnotationParams = {};
  /** Events seen while the first page loads; applied to it afterwards. */
  let pending: AnnotationEvent[] | null = null;
  /** Deleted annotations: a later copy of one (a stale page, a late event) stays out. */
  // eslint-disable-next-line svelte/prefer-svelte-reactivity -- not rendered, only read by the loaders
  const gone: Tombstones = new Set();

  // ---------------------------------------------------------------- loading

  $effect(() => {
    const s = JSON.parse(viewKey) as OverviewState;
    void reload;
    const my = (gen += 1);
    pageCtl?.abort();
    const ctl = new AbortController();
    pageCtl = ctl;
    loading = true;
    pending = [];
    const params = toAnnotationParams(s, new Date());
    api.annotations(params, ctl.signal).then(
      (page) => {
        if (my !== gen) return;
        base = params;
        rows = page.annotations.filter((a) => !gone.has(a.uuid));
        contexts = { ...page.contexts };
        next = page.next;
        error = null;
        failedCursor = null;
        active = -1;
        loaded = true;
        loading = false;
        const evs = pending ?? [];
        pending = null;
        for (const ev of evs) onAnnotation(ev);
        void fill();
      },
      (e) => {
        if (isAbort(e) || my !== gen) return;
        error = errorText(e);
        rows = [];
        contexts = {};
        next = null;
        loaded = true;
        loading = false;
        pending = null;
      },
    );
    return () => ctl.abort();
  });

  async function more(): Promise<void> {
    if (!next || loading || next === failedCursor) return;
    const cursor = next;
    const my = gen;
    const ctl = new AbortController();
    pageCtl = ctl;
    loading = true;
    try {
      const page = await api.annotations({ ...base, cursor }, ctl.signal);
      if (my !== gen) return;
      rows = appendPage(rows, page.annotations.filter((a) => !gone.has(a.uuid)));
      contexts = { ...contexts, ...page.contexts };
      next = page.next;
      error = null;
    } catch (e) {
      if (isAbort(e) || my !== gen) return;
      error = errorText(e);
      failedCursor = cursor;
    } finally {
      if (pageCtl === ctl) loading = false;
    }
    void fill();
  }

  /**
   * Keeps loading while the end of the list is in view: a page may come back short (even
   * empty) while `next` is set, and a short list must fill the screen.
   */
  async function fill(): Promise<void> {
    await tick();
    if (scroller && scroller.scrollHeight - scroller.scrollTop - scroller.clientHeight < NEAR_END) void more();
  }

  function retry(): void {
    if (failedCursor) {
      failedCursor = null;
      void more();
    } else {
      reload += 1;
    }
  }

  async function loadAuthors(): Promise<void> {
    try {
      authors = (await api.annotationAuthors()).authors;
    } catch {
      // The picker only; the URL's author still filters.
    }
  }

  $effect(() => {
    untrack(() => {
      void loadAuthors();
      void knownTags.load();
    });
  });

  // ---------------------------------------------------------------- live updates

  /** Applies one bus `annotation` event (docs/API.md §9.4, Overview). */
  function onAnnotation(ev: AnnotationEvent): void {
    if (pending) {
      pending.push(ev);
      return;
    }
    const a = ev.annotation;
    const listed = rows.some((x) => x.uuid === a.uuid);
    if (ev.op === 'deleted' || a.deleted_at !== null) {
      gone.add(a.uuid);
      if (listed) rows = rows.filter((x) => x.uuid !== a.uuid);
      return;
    }
    if (gone.has(a.uuid)) return;
    const ok = matches(a, view, new Date());
    if (listed) {
      // Its target is fixed, so the context stays as listed.
      rows = ok ? insertDesc(rows, a) : rows.filter((x) => x.uuid !== a.uuid);
    } else if (ok && belongsLoaded(rows, a, next !== null)) {
      void addOne(a.uuid);
    }
  }

  /** Fetches one annotation with its context and inserts it (the server confirms the match). */
  async function addOne(uuid: string): Promise<void> {
    const my = gen;
    try {
      const page = await api.annotations({ ...base, cursor: undefined, limit: 1, uuid });
      const a = page.annotations[0];
      if (my !== gen || !a || gone.has(a.uuid) || !belongsLoaded(rows, a, next !== null)) return;
      rows = insertDesc(rows, a);
      contexts = { ...contexts, ...page.contexts };
    } catch {
      // The next load shows it.
    }
  }

  $effect(() => {
    let timer: ReturnType<typeof setTimeout> | undefined;
    // On a fresh page load the stream connects alongside the first page: its first `hello`
    // is no resync. Later ones (reconnections) are.
    let initial = untrack(() => bus.status) !== 'open';
    const off = bus.subscribe((e) => {
      if (e.type === 'annotation') {
        onAnnotation(e.data);
        // Picker counts.
        clearTimeout(timer);
        timer = setTimeout(() => {
          void loadAuthors();
          void knownTags.load(true);
        }, BUS_DEBOUNCE_MS);
      } else if (e.type === 'hello' && initial) {
        initial = false;
      } else if (e.type === 'forgotten' || (e.type === 'hello' && loaded)) {
        // Rows may be gone with the forgotten content, or events were missed.
        reload += 1;
        void loadAuthors();
      }
    });
    return () => {
      clearTimeout(timer);
      off();
    };
  });

  // ---------------------------------------------------------------- filters

  /** A filter change: a new history entry (back and forward restore filters). */
  function setFilters(s: OverviewState, replace = false): void {
    router.setQuery(overviewToQuery(s), { replace });
  }

  function patch(p: Partial<OverviewState>): void {
    setFilters({ ...view, ...p });
  }

  function toggleTag(tag: string): void {
    patch({ tags: view.tags.includes(tag) ? view.tags.filter((t) => t !== tag) : addTag(view.tags, tag) });
  }

  function remove(key: OverviewKey, value?: string): void {
    setFilters(clearOverview(view, key, value));
  }

  function reset(): void {
    qDraft = '';
    setFilters(OVERVIEW_EMPTY);
  }

  function setDate(preset: DatePreset): void {
    if (preset === 'custom') {
      const now = Math.floor(Date.now() / 1000);
      patch({ date: 'custom', from: view.from ?? now - 7 * 86400, to: view.to });
    } else {
      patch({ date: preset, from: null, to: null });
    }
  }

  // The text field: typing replaces the history entry after a pause.
  let qDraft = $state('');
  let qTimer: ReturnType<typeof setTimeout> | undefined;

  $effect(() => {
    const q = view.q;
    untrack(() => {
      if (qTimer === undefined) qDraft = q;
    });
  });

  function flushQ(): void {
    clearTimeout(qTimer);
    qTimer = undefined;
    // A blur while the page goes away must not write into the next page's URL.
    if (parseRoute(router.path).name !== 'annotations') return;
    const q = qDraft.slice(0, MAX_Q);
    if (q !== view.q) setFilters({ ...view, q }, true);
  }

  function onq(e: Event & { currentTarget: HTMLInputElement }): void {
    qDraft = e.currentTarget.value;
    clearTimeout(qTimer);
    qTimer = setTimeout(flushQ, Q_DEBOUNCE_MS);
  }

  function qKey(e: KeyboardEvent): void {
    if (e.key === 'Enter') {
      e.preventDefault();
      flushQ();
    }
  }

  $effect(() => () => clearTimeout(qTimer));

  // The tag picker: every tag with its count, filtered by what is typed.
  let pickerOpen = $state(false);
  let tagFind = $state('');
  let pickerBtn: HTMLButtonElement | undefined = $state();
  let pickerEl: HTMLElement | undefined = $state();
  const pickable = $derived.by(() => {
    const f = tagFind.trim().toLowerCase().replace(/^#/, '');
    const list = knownTags.list.filter((t) => !f || t.name.includes(f));
    // The URL may name tags nobody uses (any more): they stay removable here.
    for (const t of view.tags) if (!list.some((x) => x.name === t) && (!f || t.includes(f))) list.push({ name: t, color: null, count: 0 });
    return list;
  });

  async function openPicker(): Promise<void> {
    pickerOpen = !pickerOpen;
    if (!pickerOpen) return;
    tagFind = '';
    void knownTags.load(true);
    await tick();
    pickerEl?.querySelector('input')?.focus();
  }

  function closePicker(focusBtn = true): void {
    pickerOpen = false;
    if (focusBtn) pickerBtn?.focus();
  }

  function pickerKey(e: KeyboardEvent): void {
    if (e.key === 'Escape') {
      e.preventDefault();
      e.stopPropagation();
      closePicker();
    }
  }

  $effect(() => {
    if (!pickerOpen) return;
    const onout = (e: PointerEvent) => {
      const t = e.target;
      if (t instanceof Node && (pickerEl?.contains(t) || pickerBtn?.contains(t))) return;
      closePicker(false);
    };
    window.addEventListener('pointerdown', onout, true);
    return () => window.removeEventListener('pointerdown', onout, true);
  });

  /** Authors for the picker, the URL's one included even when it has none (left). */
  const authorOptions = $derived(view.author && !authors.some((a) => a.name === view.author) ? [...authors, { name: view.author, count: 0 }] : authors);

  // ---------------------------------------------------------------- rows and keys

  const href = (a: Annotation) => (a.cmd_seq !== null ? sessionHref(a.session, { cmd: a.cmd_seq }) : sessionHref(a.session, { t: a.t_from }));

  /** The span's time range: wall time when known, else session time. */
  function spanText(a: Annotation, c: AnnotationContext | undefined): string {
    if (c) {
      const from = fmtTs(c.ts_from, prefs.utc);
      if (c.ts_to === null) return `${from} – …`;
      const to = fmtTs(c.ts_to, prefs.utc);
      return `${from} – ${to.slice(0, 10) === from.slice(0, 10) ? to.slice(11) : to}`;
    }
    return `${fmtClock(a.t_from ?? 0)}–${fmtClock(a.t_to ?? 0)}`;
  }

  async function focusRow(i: number): Promise<void> {
    await tick();
    const link = listEl?.querySelector<HTMLAnchorElement>(`[data-row="${i}"] a.open`);
    link?.focus({ preventScroll: true });
    link?.closest('li')?.scrollIntoView({ block: 'nearest' });
    if (i >= rows.length - 3) void more();
  }

  function move(delta: number): void {
    const n = rows.length;
    if (n === 0) return;
    active = Math.max(0, Math.min(n - 1, (active < 0 ? (delta > 0 ? -1 : n) : active) + delta));
    void focusRow(active);
  }

  $effect(() =>
    useKeys({
      'next-result': () => move(1),
      'prev-result': () => move(-1),
      'open-result': () => {
        const a = rows[active < 0 ? 0 : active];
        if (!a) return false;
        router.goto(href(a));
      },
      'open-result-tab': () => {
        const a = rows[active < 0 ? 0 : active];
        if (!a) return false;
        window.open(href(a), '_blank', 'noopener');
      },
      escape: () => {
        if (!pickerOpen) return false;
        closePicker();
      },
    }),
  );

  const chipList = $derived(overviewChips(view, (ts) => fmtTs(ts, prefs.utc).slice(0, 16)));
  const filtered = $derived(isFiltered(view));
</script>

<section
  class="overview"
  aria-labelledby="annotations-h"
  data-testid="annotations-page"
  bind:this={scroller}
  onscroll={(e) => {
    const el = e.currentTarget;
    if (el.scrollHeight - el.scrollTop - el.clientHeight < NEAR_END) void more();
  }}
>
  <header class="bar">
    <h1 id="annotations-h">Annotations</h1>
    <div class="filters" role="group" aria-label="Filters">
      <label class="q">
        <span class="sr-only">Text in the note</span>
        <input
          type="search"
          placeholder="Text in the note…"
          autocomplete="off"
          spellcheck="false"
          maxlength={MAX_Q}
          value={qDraft}
          oninput={onq}
          onkeydown={qKey}
          onblur={flushQ}
          data-testid="anno-q"
        />
      </label>
      <div class="picker-wrap">
        <button
          type="button"
          aria-expanded={pickerOpen}
          aria-controls="anno-tag-picker"
          bind:this={pickerBtn}
          onclick={openPicker}
          data-testid="anno-tag-picker"
          >Tags{#if view.tags.length}&nbsp;<span class="count">{view.tags.length}</span>{/if} <span aria-hidden="true">▾</span></button
        >
        {#if pickerOpen}
          <!-- svelte-ignore a11y_no_noninteractive_element_interactions -->
          <div class="picker" id="anno-tag-picker" role="group" aria-label="Tags" bind:this={pickerEl} onkeydown={pickerKey} data-testid="anno-tag-list">
            <label>
              <span class="sr-only">Find a tag</span>
              <input type="search" placeholder="Find a tag…" autocomplete="off" spellcheck="false" bind:value={tagFind} />
            </label>
            {#if pickable.length}
              <ul>
                {#each pickable as t (t.name)}
                  {@const on = view.tags.includes(t.name)}
                  <li>
                    <button
                      type="button"
                      aria-pressed={on}
                      disabled={!on && view.tags.length >= MAX_FILTER_TAGS}
                      onclick={() => toggleTag(t.name)}
                      data-testid="anno-pick-tag"
                      data-tag={t.name}
                      ><span class="chip" style:--c={knownTags.color(t.name)}>#{t.name}</span><span class="n">{t.count}</span></button
                    >
                  </li>
                {/each}
              </ul>
            {:else}
              <p class="muted none">{knownTags.list.length ? 'No such tag.' : 'No tags yet.'}</p>
            {/if}
          </div>
        {/if}
      </div>
      <label>
        <span class="sr-only">Author</span>
        <select value={view.author} onchange={(e) => patch({ author: e.currentTarget.value })} data-testid="anno-author">
          <option value="">any author</option>
          {#each authorOptions as a (a.name)}<option value={a.name}>{a.name} ({a.count})</option>{/each}
        </select>
      </label>
      <label>
        <span class="sr-only">Written</span>
        <select value={view.date} onchange={(e) => setDate(e.currentTarget.value as DatePreset)} data-testid="anno-date">
          {#each Object.entries(DATE_LABELS) as [value, label] (value)}<option {value}>{label}</option>{/each}
        </select>
      </label>
      {#if view.date === 'custom'}
        <label class="range"
          >from <input
            type="datetime-local"
            value={toLocalInput(view.from)}
            onchange={(e) => patch({ from: fromLocalInput(e.currentTarget.value) })}
            data-testid="anno-from"
          /></label
        >
        <label class="range"
          >to <input
            type="datetime-local"
            value={toLocalInput(view.to)}
            onchange={(e) => patch({ to: fromLocalInput(e.currentTarget.value) })}
            data-testid="anno-to"
          /></label
        >
      {/if}
    </div>
    {#if chipList.length}
      <ul class="chips" aria-label="Active filters" data-testid="anno-chips">
        {#each chipList as c (`${c.key}:${c.value ?? ''}`)}
          <li class:tag={c.key === 'tag'} style:--c={c.value ? knownTags.color(c.value) : undefined}>
            <span>{c.label}</span>
            <button type="button" class="x" aria-label={`Remove filter ${c.label}`} onclick={() => remove(c.key, c.value)}>×</button>
          </li>
        {/each}
        <li class="reset"><button type="button" class="link" onclick={reset}>Clear all</button></li>
      </ul>
    {/if}
    <p class="summary" role="status" aria-live="polite">
      {#if loaded && !error}
        <span data-testid="anno-count">{rows.length}{next ? '+' : ''} annotation{rows.length === 1 && !next ? '' : 's'}</span>
      {/if}
      <span class="tz">newest first · {tzLabel(prefs.utc)}</span>
      {#if loading}<span class="muted">loading…</span>{/if}
    </p>
  </header>

  {#if error}
    <div class="error" role="alert" data-testid="anno-error">
      <span>Annotations could not be loaded: {error}</span>
      <button type="button" onclick={retry} data-testid="anno-retry">Retry</button>
    </div>
  {/if}

  {#if rows.length}
    <ol class="list" aria-label="Annotations" bind:this={listEl} data-testid="anno-list">
      {#each rows as a, i (a.uuid)}
        {@const c = contexts[a.uuid]}
        {@const body = preview(a.body)}
        <li
          class="row"
          class:active={i === active}
          style:--c={a.tags[0] ? knownTags.color(a.tags[0]) : 'var(--border)'}
          data-row={i}
          data-uuid={a.uuid}
          data-testid="anno-row"
        >
          <div class="head">
            <span class="author" data-testid="anno-row-author">{a.author}</span>
            <time datetime={new Date(a.created_at * 1000).toISOString()} data-testid="anno-created">{fmtTs(a.created_at, prefs.utc)}</time>
            {#if a.updated_at > a.created_at}
              <span class="muted edited" title={`edited ${fmtTs(a.updated_at, prefs.utc)}`}
                >edited <time datetime={new Date(a.updated_at * 1000).toISOString()}>{fmtTs(a.updated_at, prefs.utc)}</time></span
              >
            {/if}
            {#if a.tags.length}
              <span class="tags">
                {#each a.tags as t (t)}
                  <button
                    type="button"
                    class="chip"
                    class:on={view.tags.includes(t)}
                    style:--c={knownTags.color(t)}
                    title={view.tags.includes(t) ? `Filtering by #${t}` : `Only annotations tagged #${t}`}
                    onclick={() => !view.tags.includes(t) && toggleTag(t)}
                    data-testid="anno-tag">#{t}</button
                  >
                {/each}
              </span>
            {/if}
          </div>
          <a class="open" href={href(a)} onclick={router.link} onfocus={() => (active = i)} data-testid="anno-open">
            <span class="ctx">
              {#if c}
                <span class="who">{c.user}@{c.host}</span>
                <span title="session start">session {fmtTs(c.started_at, prefs.utc)}</span>
                {#if c.pane}<span>{c.tmux_session ? `${c.tmux_session} ` : ''}{c.pane}</span>{/if}
              {:else}
                <span>session {shortId(a.session)}</span>
              {/if}
            </span>
            <span class="target" data-testid="anno-target">
              {#if a.cmd_seq !== null}
                {#if c?.command}
                  <span class="prompt" aria-hidden="true">$</span><code class="typed">{oneLine(c.command.typed)}</code>
                  <span class={`exit ${exitClass(c.command.exit)}`} title="exit status">{c.command.exit ?? '?'}</span>
                {:else}
                  <span class="muted">command {a.cmd_seq}</span>
                {/if}
              {:else}
                <span class="badge">span</span>
                <span class="range-text" title={`${fmtClock(a.t_from ?? 0)}–${fmtClock(a.t_to ?? 0)} into the session`}>{spanText(a, c)}</span>
              {/if}
            </span>
            {#if body.text}
              <span class="body" data-testid="anno-body"
                >{#each markText(body.text, view.q) as p, j (j)}{#if p.hit}<mark>{p.text}</mark>{:else}{p.text}{/if}{/each}{#if body.more}<span
                    class="muted"> …</span
                  >{/if}</span
              >
            {/if}
          </a>
        </li>
      {/each}
    </ol>
  {/if}

  {#if next && !loading && failedCursor === null}
    <button type="button" class="more" onclick={more} data-testid="anno-more">Load more</button>
  {/if}

  {#if loaded && !error && !loading && rows.length === 0 && !next}
    <div class="empty" data-testid="anno-empty">
      {#if filtered}
        <p>No annotations match these filters.</p>
        <button type="button" onclick={reset} data-testid="anno-reset">Clear filters</button>
      {:else}
        <p>No annotations yet.</p>
        <p class="muted">Open a session and press <kbd>a</kbd> to annotate the current command, or drag on the rail to annotate a span.</p>
      {/if}
    </div>
  {:else if !loaded && !error}
    <p class="muted empty">Loading annotations…</p>
  {/if}
</section>

<style>
  .overview {
    display: flex;
    flex-direction: column;
    gap: 0.5rem;
    height: 100%;
    padding: 0.6rem 0.8rem 1.5rem;
    overflow: auto;
  }
  .bar {
    display: grid;
    gap: 0.45rem;
  }
  h1 {
    margin: 0;
    font-size: 1.2rem;
  }
  .filters {
    display: flex;
    flex-wrap: wrap;
    gap: 0.4rem;
    align-items: center;
  }
  .filters label {
    display: inline-flex;
    gap: 0.3rem;
    align-items: center;
    color: var(--fg-muted);
  }
  .q {
    flex: 1 1 14rem;
    min-width: 0;
  }
  .q input {
    flex: 1;
    min-width: 0;
    padding: 0.4em 0.6em;
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
  .picker-wrap {
    position: relative;
  }
  .picker {
    position: absolute;
    z-index: 20;
    top: calc(100% + 0.25rem);
    left: 0;
    display: grid;
    grid-template-rows: auto minmax(0, 1fr);
    gap: 0.35rem;
    width: min(20rem, calc(100vw - 1.6rem));
    max-height: min(24rem, 60vh);
    padding: 0.5rem;
    border: 1px solid var(--border);
    border-radius: var(--radius);
    background: var(--bg-elev);
    box-shadow: var(--shadow);
  }
  .picker label input {
    width: 100%;
  }
  .picker ul {
    display: grid;
    gap: 0.15rem;
    margin: 0;
    padding: 0;
    list-style: none;
    overflow-y: auto;
    min-height: 0;
  }
  .picker li button {
    display: flex;
    justify-content: space-between;
    width: 100%;
    border-color: transparent;
    background: transparent;
  }
  .picker li button:hover {
    background: var(--bg-hover);
  }
  .picker li button[aria-pressed='true'] {
    background: var(--accent-soft);
    border-color: var(--accent);
  }
  .picker .n {
    color: var(--fg-muted);
    font-size: 0.85em;
    font-variant-numeric: tabular-nums;
  }
  .picker .none {
    margin: 0.2rem;
  }
  .chip {
    padding: 0 0.45rem;
    border: 1px solid var(--c);
    border-left-width: 4px;
    border-radius: 999px;
    background: transparent;
    font-size: 0.8em;
    line-height: 1.5;
    overflow: hidden;
    text-overflow: ellipsis;
  }
  button.chip:hover {
    background: var(--bg-hover);
  }
  button.chip.on {
    background: var(--accent-soft);
    cursor: default;
  }
  .chips {
    display: flex;
    flex-wrap: wrap;
    gap: 0.3rem;
    align-items: center;
    margin: 0;
    padding: 0;
    list-style: none;
  }
  .chips li:not(.reset) {
    display: inline-flex;
    align-items: center;
    gap: 0.2rem;
    padding: 0 0 0 0.6em;
    border: 1px solid var(--accent);
    border-radius: 999px;
    background: var(--accent-soft);
    font-size: 0.85em;
    max-width: 100%;
  }
  .chips li > span {
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
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
  .link {
    padding: 0 0.3rem;
    border: 0;
    background: none;
    color: var(--accent);
    font-size: 0.85em;
  }
  .link:hover {
    text-decoration: underline;
    background: none;
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
    font-size: 0.9em;
  }
  .error {
    display: flex;
    flex-wrap: wrap;
    gap: 0.5rem;
    align-items: center;
    padding: 0.4rem 0.6rem;
    border-radius: var(--radius);
    background: var(--fail-soft);
  }
  .list {
    display: grid;
    gap: 0.4rem;
    margin: 0;
    padding: 0;
    list-style: none;
  }
  .row {
    display: grid;
    gap: 0.15rem;
    min-width: 0;
    padding: 0.4rem 0.6rem;
    border: 1px solid var(--border);
    border-left: 3px solid var(--c);
    border-radius: var(--radius);
    background: var(--bg-elev);
  }
  .row.active {
    background: var(--bg-hover);
  }
  .head {
    display: flex;
    flex-wrap: wrap;
    gap: 0.2rem 0.6rem;
    align-items: center;
    min-width: 0;
    font-size: 0.85em;
    color: var(--fg-muted);
  }
  .author {
    color: var(--fg);
    font-weight: 600;
  }
  .head time {
    font-family: var(--mono);
  }
  .tags {
    display: flex;
    flex-wrap: wrap;
    gap: 0.25rem;
    min-width: 0;
  }
  .tags .chip {
    max-width: 14rem;
    font-size: 0.95em;
    color: var(--fg);
  }
  .open {
    display: grid;
    gap: 0.15rem;
    min-width: 0;
    color: var(--fg);
    text-decoration: none;
    border-radius: 4px;
  }
  .open:hover .target,
  .open:focus-visible .target {
    text-decoration: underline;
    text-decoration-color: var(--fg-muted);
  }
  .ctx,
  .target {
    display: flex;
    gap: 0.5rem;
    align-items: center;
    min-width: 0;
    white-space: nowrap;
    overflow: hidden;
  }
  .ctx {
    flex-wrap: wrap;
    gap: 0 0.6rem;
    font-size: 0.8em;
    color: var(--fg-muted);
  }
  .ctx .who {
    color: var(--fg);
  }
  .target {
    font-size: 0.92em;
  }
  .prompt {
    color: var(--fg-muted);
    font-family: var(--mono);
  }
  .typed,
  .range-text {
    min-width: 0;
    overflow: hidden;
    text-overflow: ellipsis;
  }
  .range-text {
    font-family: var(--mono);
    font-size: 0.92em;
  }
  .target .exit,
  .target .badge {
    flex: none;
  }
  .body {
    display: -webkit-box;
    -webkit-box-orient: vertical;
    -webkit-line-clamp: 3;
    line-clamp: 3;
    overflow: hidden;
    white-space: pre-wrap;
    overflow-wrap: anywhere;
  }
  .more {
    align-self: center;
  }
  .empty {
    margin: 1rem 0;
    color: var(--fg-muted);
  }
  .empty p {
    margin: 0 0 0.4rem;
  }
  .sr-only {
    position: absolute;
    width: 1px;
    height: 1px;
    overflow: hidden;
    clip-path: inset(50%);
    white-space: nowrap;
  }

  /* Touch-sized controls. */
  @media (pointer: coarse), (max-width: 760px) {
    .filters button,
    .filters input,
    .filters select,
    .picker li button,
    .more,
    .empty button,
    .error button {
      min-height: 40px;
    }
    .chips .x {
      min-width: 40px;
      min-height: 32px;
      justify-content: center;
    }
    .tags .chip {
      min-height: 30px;
      padding: 0 0.6rem;
    }
  }
  @media (max-width: 760px) {
    .overview {
      padding: 0.5rem 0.6rem 1.5rem;
    }
    .q {
      flex-basis: 100%;
    }
    .range {
      flex: 1 1 100%;
    }
    .range input {
      flex: 1;
    }
  }
</style>
