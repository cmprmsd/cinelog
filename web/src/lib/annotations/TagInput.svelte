<script lang="ts" module>
  let instances = 0;
  const nextId = () => (instances += 1);
</script>

<script lang="ts">
  // Tag chips plus a text field with autocomplete from the known tags. Enter, comma or
  // space adds what is typed; Backspace in the empty field removes the last chip.
  import type { Tag } from '../api/types';
  import { MAX_TAGS, normalizeTag, sortTags, suggestTags, tagColor } from './annotations';

  interface Props {
    tags: string[];
    /** Every known tag (suggestions and colours). */
    known: readonly Tag[];
    label?: string;
    /** Tags that cannot be removed here (an annotation's existing tags when only adding). */
    fixed?: readonly string[];
  }
  let { tags = $bindable([]), known, label = 'Tags', fixed = [] }: Props = $props();

  const id = `tag-input-${nextId()}`;
  let draft = $state('');
  let error = $state<string | null>(null);
  const suggestions = $derived(suggestTags(known, draft, [...tags, ...fixed]));
  const colorOf = (name: string) => tagColor(name, known.find((t) => t.name === name)?.color);

  /** Adds what is typed. False when it is not a valid tag (the error is shown). */
  export function flush(): boolean {
    const pieces = draft.split(/[\s,]+/).filter(Boolean);
    const next = [...tags];
    for (const p of pieces) {
      const n = normalizeTag(p);
      if (!n) {
        error = `"${p}" is not a valid tag: 1–40 letters, digits or - _ . : / + # @.`;
        return false;
      }
      next.push(n);
    }
    const all = sortTags(next);
    if (all.length + fixed.filter((f) => !all.includes(f)).length > MAX_TAGS) {
      error = `At most ${MAX_TAGS} tags.`;
      return false;
    }
    tags = all;
    draft = '';
    error = null;
    return true;
  }

  function remove(name: string): void {
    tags = tags.filter((t) => t !== name);
  }

  function onkeydown(e: KeyboardEvent): void {
    if ((e.key === 'Enter' && !e.ctrlKey && !e.metaKey) || e.key === ',') {
      if (!draft.trim()) return;
      e.preventDefault();
      flush();
    } else if (e.key === 'Backspace' && draft === '' && tags.length) {
      tags = tags.slice(0, -1);
    }
  }

  function oninput(e: Event): void {
    draft = (e.currentTarget as HTMLInputElement).value;
    error = null;
    // A suggestion picked from the list replaces the text at once: take it.
    const kind = (e as InputEvent).inputType;
    if (kind === 'insertReplacementText' || (kind === undefined && suggestions.some((s) => s.name === draft))) flush();
    else if (/[\s,]/.test(draft)) flush();
  }
</script>

<div class="tags" data-testid="tag-input">
  <label class="sr-only" for={id}>{label}</label>
  {#each tags as t (t)}
    <span class="chip" style:--c={colorOf(t)}
      >#{t}<button type="button" class="x" aria-label={`Remove tag ${t}`} onclick={() => remove(t)}>×</button></span
    >
  {/each}
  <input
    {id}
    type="text"
    list={`${id}-list`}
    autocomplete="off"
    spellcheck="false"
    placeholder={tags.length ? 'more tags…' : 'tags (Enter adds)'}
    value={draft}
    {onkeydown}
    {oninput}
    aria-invalid={error !== null}
    aria-describedby={error ? `${id}-err` : undefined}
  />
  <datalist id={`${id}-list`}>
    {#each suggestions as s (s.name)}
      <option value={s.name}>{s.count ? `${s.count}×` : ''}</option>
    {/each}
  </datalist>
  {#if error}<span class="err" id={`${id}-err`} role="alert">{error}</span>{/if}
</div>

<style>
  .tags {
    display: flex;
    flex-wrap: wrap;
    gap: 0.25rem;
    align-items: center;
  }
  input {
    flex: 1 1 8rem;
    min-width: 6rem;
  }
  .chip {
    display: inline-flex;
    align-items: center;
    gap: 0.15rem;
    padding: 0 0.15rem 0 0.45rem;
    border-radius: 999px;
    border: 1px solid var(--c);
    border-left-width: 4px;
    font-size: 0.85em;
  }
  .x {
    padding: 0 0.3rem;
    border: 0;
    background: transparent;
    line-height: 1.2;
  }
  .err {
    flex-basis: 100%;
    color: var(--fail);
    font-size: 0.85em;
  }
</style>
