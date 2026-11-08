<script lang="ts">
  // Annotations of one session (work package M4-C; plan §8.4; docs/API.md §9.4, §9.6):
  // shared notes and tags on a command or a time span. Lists the session's annotations,
  // creates, edits and deletes them (edit and delete for the author only; anyone adds
  // tags), gives the rail its spans and stays in sync through the UI bus (polling while the
  // bus is down). `SessionView` calls `annotate()` for the `a` key.
  import { tick, untrack } from 'svelte';
  import { api, errorText } from '../api/client';
  import { bus } from '../api/events.svelte';
  import type { Annotation, AnnotationEvent, BusEvent, Command, Tag } from '../api/types';
  import { fmtClock, fmtTs, oneLine } from '../format';
  import { prefs } from '../prefs.svelte';
  import { principal } from '../principal.svelte';
  import type { AnnotationSpan, TimeRange } from '../spans';
  import AnnotationBody from './AnnotationBody.svelte';
  import {
    applyEvent,
    bodyBytes,
    covers,
    dropDeleted,
    MAX_BODY_BYTES,
    rangeOfAnnotation,
    spansOf,
    tagColor,
    targetLabel,
    type Tombstones,
    upsert,
    upsertTag,
    withoutDeleted,
  } from './annotations';
  import TagInput from './TagInput.svelte';

  interface Props {
    session: string;
    commands: Command[];
    /** The command at the playhead. */
    current: Command | null;
    currentTime: number;
    /** The rail selection, if any. */
    selection: TimeRange | null;
    onseek: (t: number) => void;
    /** Out: what the rail draws. */
    spans?: AnnotationSpan[];
  }
  let { session, commands, current, currentTime, selection, onseek, spans = $bindable([]) }: Props = $props();

  const PAGE = 500;
  const MAX_PAGES = 20;
  /** Refetch interval while the bus is not open. */
  const POLL_MS = 5000;

  /** The open form: a new annotation, or an edit of one. */
  interface Draft {
    uuid: string | null;
    kind: 'cmd' | 'span';
    cmdSeq: number | null;
    from: number;
    to: number;
    body: string;
    tags: string[];
  }

  let annotations = $state<Annotation[]>([]);
  let tags = $state<Tag[]>([]);
  let loaded = $state(false);
  let loadError = $state<string | null>(null);
  let draft = $state<Draft | null>(null);
  let formError = $state<string | null>(null);
  let busy = $state(false);
  /** The annotation a non-author is adding tags to. */
  let adding = $state<{ uuid: string; tags: string[] } | null>(null);
  let bodyField: HTMLTextAreaElement | undefined = $state();
  let draftTags: TagInput | undefined = $state();
  let addTags: TagInput | undefined = $state();
  let panel: HTMLElement | undefined = $state();

  const me = $derived(principal.me?.principal ?? null);
  const colors = $derived(new Map(tags.map((t) => [t.name, t.color])));
  const currentSeq = $derived(current?.seq ?? null);

  $effect(() => {
    spans = spansOf(annotations, commands, colors);
  });

  // ---- loading and sync ------------------------------------------------------------------

  /** Load generation: a newer load or a session change drops older results. */
  let gen = 0;
  /** Events seen while a load runs; replayed onto its result (it may predate them). */
  let pending: AnnotationEvent[] | null = null;
  /** Annotations of this session known to be deleted (see `Tombstones`). */
  let gone: Tombstones = new Set();

  async function fetchAll(id: string): Promise<Annotation[]> {
    const out: Annotation[] = [];
    let cursor: string | undefined;
    for (let i = 0; i < MAX_PAGES; i += 1) {
      const page = await api.annotations({ session: id, limit: PAGE, cursor });
      out.push(...page.annotations);
      if (!page.next) break;
      cursor = page.next;
    }
    return out;
  }

  async function load(): Promise<void> {
    const my = (gen += 1);
    const id = session;
    pending = [];
    try {
      let list = await fetchAll(id);
      if (my !== gen) return;
      for (const ev of pending ?? []) list = applyEvent(list, ev, id, gone);
      annotations = withoutDeleted(list, gone);
      loadError = null;
    } catch (e) {
      if (my === gen) loadError = `Annotations could not be loaded: ${errorText(e)}`;
    } finally {
      if (my === gen) {
        pending = null;
        loaded = true;
      }
    }
  }

  async function loadTags(): Promise<void> {
    try {
      tags = (await api.tags()).tags;
    } catch {
      // Autocomplete and colours only; the next bus `hello` or poll retries.
    }
  }

  $effect(() => {
    void session;
    untrack(() => {
      annotations = [];
      gone = new Set();
      loaded = false;
      draft = null;
      adding = null;
      void load();
    });
    return () => {
      gen += 1;
    };
  });

  $effect(() => {
    untrack(() => void loadTags());
  });

  $effect(() =>
    bus.subscribe((e: BusEvent) => {
      if (e.type === 'annotation') {
        pending?.push(e.data);
        annotations = applyEvent(annotations, e.data, session, gone);
      } else if (e.type === 'tag') {
        tags = upsertTag(tags, e.data);
      } else if (e.type === 'hello') {
        // A (re)connection or a resync after lag: events may have been missed.
        void load();
        void loadTags();
      }
    }),
  );

  $effect(() => {
    if (bus.status === 'open') return;
    const timer = setInterval(() => {
      void load();
      void loadTags();
    }, POLL_MS);
    return () => clearInterval(timer);
  });

  // ---- creating and editing --------------------------------------------------------------

  async function focusBody(): Promise<void> {
    await tick();
    bodyField?.focus();
  }

  /** Starts an annotation of the selection, else the current command. False: nothing to annotate. */
  export function annotate(): boolean {
    if (draft) {
      void focusBody();
      return true;
    }
    if (!selection && !current) return false;
    adding = null;
    formError = null;
    draft = {
      uuid: null,
      kind: selection ? 'span' : 'cmd',
      cmdSeq: current?.seq ?? null,
      from: round(selection?.from ?? currentTime),
      to: round(selection?.to ?? currentTime),
      body: '',
      tags: [],
    };
    void focusBody();
    return true;
  }

  // A new selection while composing retargets the new annotation to it.
  $effect(() => {
    const sel = selection;
    untrack(() => {
      if (!sel || !draft || draft.uuid !== null) return;
      draft.kind = 'span';
      draft.from = round(sel.from);
      draft.to = round(sel.to);
    });
  });

  const round = (t: number) => Math.round(t * 100) / 100;

  function edit(a: Annotation): void {
    adding = null;
    formError = null;
    draft = {
      uuid: a.uuid,
      kind: a.cmd_seq !== null ? 'cmd' : 'span',
      cmdSeq: a.cmd_seq,
      from: a.t_from ?? 0,
      to: a.t_to ?? 0,
      body: a.body,
      tags: [...a.tags],
    };
    void focusBody();
  }

  function cancel(): void {
    draft = null;
    formError = null;
  }

  /** The request problem in words, or null when the draft can be sent. */
  function problem(d: Draft): string | null {
    if (!d.body.trim() && !d.tags.length) return 'Write something or add a tag.';
    if (bodyBytes(d.body) > MAX_BODY_BYTES) return `The text is longer than ${MAX_BODY_BYTES / 1024} KiB.`;
    if (d.uuid === null && d.kind === 'cmd' && d.cmdSeq === null) return 'There is no command to attach to.';
    if (d.uuid === null && d.kind === 'span') {
      if (!(Number.isFinite(d.from) && Number.isFinite(d.to)) || d.from < 0 || d.from > d.to) {
        return 'The span needs 0 ≤ from ≤ to.';
      }
    }
    return null;
  }

  async function save(e?: SubmitEvent): Promise<void> {
    e?.preventDefault();
    const d = draft;
    if (!d || busy) return;
    if (draftTags && !draftTags.flush()) return;
    const why = problem(d);
    if (why) {
      formError = why;
      return;
    }
    busy = true;
    formError = null;
    try {
      const body = d.body.trimEnd();
      const saved =
        d.uuid !== null
          ? await api.updateAnnotation(d.uuid, { body, tags: d.tags })
          : await api.createAnnotation({
              session_uuid: session,
              ...(d.kind === 'cmd' ? { cmd_seq: d.cmdSeq! } : { t_from: d.from, t_to: d.to }),
              body,
              tags: d.tags,
            });
      annotations = upsert(annotations, saved, gone);
      draft = null;
    } catch (err) {
      formError = `Not saved: ${errorText(err)}`;
    } finally {
      busy = false;
    }
  }

  // Esc and Ctrl+Enter inside our forms, ahead of the app's shortcuts (which would clear the
  // selection or close the session on Esc).
  function formKeys(e: KeyboardEvent): void {
    const form = e.target instanceof Element ? e.target.closest<HTMLElement>('[data-annotation-form]') : null;
    if (!form || !panel?.contains(form)) return;
    const kind = form.dataset.annotationForm;
    if (e.key === 'Escape') {
      e.preventDefault();
      e.stopPropagation();
      if (kind === 'add') adding = null;
      else cancel();
    } else if (e.key === 'Enter' && (e.ctrlKey || e.metaKey) && kind === 'compose') {
      e.preventDefault();
      e.stopPropagation();
      void save();
    }
  }

  async function remove(a: Annotation): Promise<void> {
    const what = a.body ? `"${oneLine(a.body).slice(0, 60)}"` : 'this annotation';
    if (!window.confirm(`Delete ${what}? Everyone stops seeing it.`)) return;
    try {
      await api.deleteAnnotation(a.uuid);
      annotations = dropDeleted(annotations, a.uuid, gone);
      loadError = null;
    } catch (err) {
      loadError = `Not deleted: ${errorText(err)}`;
    }
  }

  async function startAdding(a: Annotation): Promise<void> {
    draft = null;
    adding = { uuid: a.uuid, tags: [] };
    await tick();
    document.getElementById(`add-${a.uuid}`)?.querySelector('input')?.focus();
  }

  async function saveAdded(e: SubmitEvent): Promise<void> {
    e.preventDefault();
    const add = adding;
    if (!add || busy) return;
    if (addTags && !addTags.flush()) return;
    if (!add.tags.length) {
      adding = null;
      return;
    }
    busy = true;
    try {
      annotations = upsert(annotations, await api.updateAnnotation(add.uuid, { add_tags: add.tags }), gone);
      adding = null;
      loadError = null;
    } catch (err) {
      loadError = `Tags not added: ${errorText(err)}`;
    } finally {
      busy = false;
    }
  }

  function seek(a: Annotation): void {
    const r = rangeOfAnnotation(a, commands);
    if (r) onseek(r.from);
  }

  const draftCommand = $derived(draft?.cmdSeq != null ? commands.find((c) => c.seq === draft!.cmdSeq) : undefined);
  const bytesLeft = $derived(draft ? MAX_BODY_BYTES - bodyBytes(draft.body) : MAX_BODY_BYTES);
</script>

{#snippet form(d: Draft)}
  <form class="compose" onsubmit={save} data-annotation-form="compose" data-testid="annotation-form" aria-label={d.uuid ? 'Edit annotation' : 'New annotation'}>
    {#if d.uuid === null}
      <fieldset class="target">
        <legend class="sr-only">Attach to</legend>
        <label>
          <input
            type="radio"
            name="target"
            value="cmd"
            checked={d.kind === 'cmd'}
            onchange={() => (d.kind = 'cmd')}
            disabled={d.cmdSeq === null}
          />
          {#if draftCommand}command <code>$ {oneLine(draftCommand.typed).slice(0, 60)}</code>{:else}command{/if}
        </label>
        <label>
          <input type="radio" name="target" value="span" checked={d.kind === 'span'} onchange={() => (d.kind = 'span')} />
          span
        </label>
        {#if d.kind === 'span'}
          <span class="span-fields">
            <label>from <input type="number" min="0" step="0.1" bind:value={d.from} aria-label="Span start (s)" /></label>
            <label>to <input type="number" min="0" step="0.1" bind:value={d.to} aria-label="Span end (s)" /></label>
            <span class="muted">{fmtClock(d.from || 0)}–{fmtClock(d.to || 0)}</span>
          </span>
        {/if}
      </fieldset>
    {/if}
    <label class="sr-only" for={`body-${session}`}>Annotation text</label>
    <textarea
      id={`body-${session}`}
      bind:this={bodyField}
      bind:value={d.body}
      rows="3"
      placeholder="Note (plain text; links work). Ctrl+Enter saves, Esc cancels."
      data-testid="annotation-text"
    ></textarea>
    <TagInput bind:this={draftTags} bind:tags={d.tags} known={tags} />
    <div class="form-actions">
      <button type="submit" class="primary" disabled={busy} data-testid="annotation-save">{d.uuid ? 'Save' : 'Annotate'}</button>
      <button type="button" onclick={cancel}>Cancel</button>
      {#if bytesLeft < 1024}<span class="muted">{bytesLeft} bytes left</span>{/if}
      {#if formError}<span class="err" role="alert">{formError}</span>{/if}
    </div>
  </form>
{/snippet}

<svelte:window onkeydowncapture={formKeys} />

<section class="panel" bind:this={panel} aria-labelledby={`notes-${session}`} data-testid="annotations-panel">
  <header>
    <h3 id={`notes-${session}`}>Annotations <span class="muted">{annotations.length || ''}</span></h3>
    <button
      type="button"
      onclick={annotate}
      disabled={!selection && !current}
      title="Annotate the rail selection or the current command (a)"
      data-testid="annotate">Annotate <kbd>a</kbd></button
    >
  </header>

  {#if draft && draft.uuid === null}{@render form(draft)}{/if}
  {#if loadError}<p class="err" role="alert">{loadError}</p>{/if}

  {#if annotations.length}
    <ul class="list">
      {#each annotations as a (a.uuid)}
        {@const mine = me !== null && a.author === me}
        <li class:here={covers(a, commands, currentTime, currentSeq)} data-testid="annotation" data-uuid={a.uuid}>
          <div class="line">
            <button type="button" class="target-btn" onclick={() => seek(a)} title="Play from here" disabled={!rangeOfAnnotation(a, commands)}
              >{targetLabel(a, commands)}</button
            >
            {#each a.tags as t (t)}
              <span class="chip" style:--c={tagColor(t, colors.get(t))} data-testid="annotation-tag">#{t}</span>
            {/each}
          </div>
          {#if draft && draft.uuid === a.uuid}
            {@render form(draft)}
          {:else}
            {#if a.body}<AnnotationBody body={a.body} />{/if}
            <div class="meta muted">
              <span data-testid="annotation-author">{a.author}</span>
              <time datetime={new Date(a.created_at * 1000).toISOString()}>{fmtTs(a.created_at, prefs.utc)}</time>
              {#if a.updated_at > a.created_at}<span title={fmtTs(a.updated_at, prefs.utc)}>edited</span>{/if}
              <span class="acts">
                {#if mine}
                  <button type="button" class="link" onclick={() => edit(a)}>Edit</button>
                  <button type="button" class="link" onclick={() => remove(a)}>Delete</button>
                {:else}
                  <button type="button" class="link" onclick={() => startAdding(a)}>Add tags</button>
                {/if}
              </span>
            </div>
            {#if adding && adding.uuid === a.uuid}
              <form id={`add-${a.uuid}`} class="add" onsubmit={saveAdded} data-annotation-form="add" aria-label="Add tags">
                <TagInput bind:this={addTags} bind:tags={adding.tags} known={tags} fixed={a.tags} label="Tags to add" />
                <button type="submit" disabled={busy}>Add</button>
                <button type="button" onclick={() => (adding = null)}>Cancel</button>
              </form>
            {/if}
          {/if}
        </li>
      {/each}
    </ul>
  {:else if loaded && !draft}
    <p class="muted empty">No annotations yet. Press <kbd>a</kbd> to annotate the current command, or drag on the rail to select a span first.</p>
  {/if}
</section>

<style>
  .panel {
    display: grid;
    gap: 0.4rem;
    border-top: 1px solid var(--border);
    padding-top: 0.5rem;
  }
  header {
    display: flex;
    justify-content: space-between;
    align-items: center;
    gap: 0.5rem;
  }
  h3 {
    margin: 0;
    font-size: 0.95rem;
  }
  .list {
    display: grid;
    gap: 0.35rem;
    margin: 0;
    padding: 0;
    list-style: none;
  }
  li {
    display: grid;
    gap: 0.2rem;
    padding: 0.35rem 0.5rem;
    border: 1px solid var(--border);
    border-left: 3px solid var(--border);
    border-radius: var(--radius);
    background: var(--bg-elev);
  }
  li.here {
    border-left-color: var(--accent);
  }
  .line {
    display: flex;
    flex-wrap: wrap;
    gap: 0.3rem;
    align-items: center;
  }
  .target-btn {
    font-family: var(--mono);
    font-size: 0.85em;
    padding: 0 0.4rem;
    max-width: 100%;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .chip {
    padding: 0 0.45rem;
    border-radius: 999px;
    border: 1px solid var(--c);
    border-left-width: 4px;
    font-size: 0.8em;
  }
  .meta {
    display: flex;
    flex-wrap: wrap;
    gap: 0.1rem 0.6rem;
    align-items: center;
    font-size: 0.82em;
  }
  .acts {
    display: flex;
    gap: 0.4rem;
    margin-left: auto;
  }
  .link {
    padding: 0;
    border: 0;
    background: none;
    color: var(--accent);
    font-size: inherit;
  }
  .link:hover {
    text-decoration: underline;
  }
  .compose,
  .add {
    display: grid;
    gap: 0.35rem;
  }
  .add {
    grid-template-columns: 1fr auto auto;
    align-items: center;
  }
  .target {
    display: flex;
    flex-wrap: wrap;
    gap: 0.2rem 0.8rem;
    align-items: center;
    margin: 0;
    padding: 0;
    border: 0;
    font-size: 0.88em;
  }
  .target code {
    font-size: 0.95em;
  }
  .span-fields {
    display: inline-flex;
    gap: 0.4rem;
    align-items: center;
  }
  .span-fields input {
    width: 6.5rem;
  }
  textarea {
    width: 100%;
    box-sizing: border-box;
    resize: vertical;
    font: inherit;
  }
  .form-actions {
    display: flex;
    flex-wrap: wrap;
    gap: 0.4rem;
    align-items: center;
  }
  .err {
    margin: 0;
    color: var(--fail);
    font-size: 0.88em;
  }
  .empty {
    margin: 0;
    font-size: 0.88em;
  }
</style>
