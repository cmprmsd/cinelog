<script lang="ts">
  // Export menu of the session view (plan §8.4; docs/API.md §7, §9.5): the current command,
  // the rail selection or the whole session as .cast, .txt or .html. Downloads go through
  // `fetch` + a blob URL so the session cookie applies. `SessionView` mounts it with exactly
  // these props and calls `open()` for the `e` key and `close()` for `Esc`.
  import { api, errorText } from '../api/client';
  import type { Command, ExportFormat } from '../api/types';
  import { download } from '../download';
  import type { TimeRange } from '../spans';
  import { EXPORT_FORMATS, entryLabel, exportChoices, exportParams, fallbackName, moveFocus, type ExportChoice } from './export';

  interface Props {
    uuid: string;
    /** The command at the playhead. */
    command: Command | null;
    /** The rail selection, if any. */
    selection: TimeRange | null;
    /** A short status line for the session view (download done or failed). */
    onnotice: (text: string) => void;
  }
  let { uuid, command, selection, onnotice }: Props = $props();

  let menu: HTMLDetailsElement | undefined = $state();
  let isOpen = $state(false);
  /** The entry being downloaded; the menu stays usable for the others. */
  let busy = $state<string | null>(null);

  const choices = $derived(exportChoices(command, selection, null));

  const entries = () => [...(menu?.querySelectorAll<HTMLButtonElement>('[role="menuitem"]:not(:disabled)') ?? [])];

  /** Opens the menu with focus on its first enabled entry. */
  export function open(): boolean {
    if (!menu) return false;
    menu.open = true;
    isOpen = true;
    entries()[0]?.focus();
    return true;
  }

  /** Closes the menu; true when it was open. Focus inside the menu returns to its button. */
  export function close(): boolean {
    if (!menu?.open) return false;
    const hadFocus = menu.contains(document.activeElement);
    menu.open = false;
    isOpen = false;
    if (hadFocus) menu.querySelector<HTMLElement>('summary')?.focus();
    return true;
  }

  async function run(choice: ExportChoice, format: ExportFormat): Promise<void> {
    const p = exportParams(choice, format);
    if (!p) return;
    const label = entryLabel(choice, format);
    close();
    busy = label;
    try {
      const name = await download(api.exportUrl(uuid, p), fallbackName(uuid, format, choice.range));
      onnotice(`Downloaded ${name}.`);
    } catch (e) {
      onnotice(`Export failed: ${errorText(e)}`);
    } finally {
      if (busy === label) busy = null;
    }
  }

  // Esc inside the open menu is ours alone (captured before the app's global Esc, which would
  // only blur the entry).
  function onkeydowncapture(e: KeyboardEvent): void {
    if (e.key !== 'Escape' || !isOpen || !(e.target instanceof Node) || !menu?.contains(e.target)) return;
    close();
    e.preventDefault();
    e.stopPropagation();
  }

  function onkeydown(e: KeyboardEvent): void {
    if (!isOpen) return;
    const list = entries();
    const at = list.indexOf(document.activeElement as HTMLButtonElement);
    const next = moveFocus(e.key, at < 0 ? -1 : at, list.length, EXPORT_FORMATS.length);
    if (next === null) return;
    e.preventDefault();
    list[at < 0 && e.key === 'ArrowDown' ? 0 : next]?.focus();
  }

  // A click anywhere else closes the menu.
  function onpointerdown(e: PointerEvent): void {
    if (isOpen && menu && e.target instanceof Node && !menu.contains(e.target)) close();
  }
</script>

<svelte:window {onpointerdown} {onkeydowncapture} />

<details class="menu" bind:this={menu} ontoggle={() => (isOpen = menu?.open ?? false)} data-testid="export-menu">
  <summary class="btn" aria-haspopup="menu" title="Export (e)" {onkeydown}>
    Export{#if busy}<span class="spin" aria-hidden="true"></span>{/if}
  </summary>
  <div class="menu-body" role="menu" aria-label="Export" tabindex="-1" {onkeydown}>
    {#each choices as c (c.source)}
      <div class="row" role="group" aria-label={c.title} data-source={c.source}>
        <div class="what">
          <span class="title">{c.title}</span>
          <span class="detail muted" class:mono={c.source === 'command' && c.query !== null}>{c.detail}</span>
        </div>
        {#each EXPORT_FORMATS as f (f.format)}
          <button
            type="button"
            role="menuitem"
            disabled={!c.query}
            aria-label={entryLabel(c, f.format)}
            title={f.hint}
            onclick={() => run(c, f.format)}>.{f.format}</button
          >
        {/each}
      </div>
    {/each}
    <p class="foot muted">
      {#if busy}<span role="status">Exporting {busy}…</span>{:else}Forgotten output stays masked in every format.{/if}
    </p>
  </div>
</details>

<style>
  .menu {
    position: relative;
  }
  .menu summary {
    list-style: none;
  }
  .menu summary::-webkit-details-marker {
    display: none;
  }
  .menu-body {
    position: absolute;
    right: 0;
    z-index: 10;
    display: grid;
    gap: 0.45rem;
    width: max-content;
    max-width: min(34rem, 92vw);
    padding: 0.6rem;
    margin-top: 0.2rem;
    border: 1px solid var(--border);
    border-radius: var(--radius);
    background: var(--bg-elev);
    box-shadow: var(--shadow);
  }
  .row {
    display: grid;
    grid-template-columns: minmax(0, 1fr) repeat(3, auto);
    gap: 0.3rem;
    align-items: center;
  }
  .what {
    display: grid;
    min-width: 0;
    margin-right: 0.4rem;
  }
  .title {
    font-weight: 600;
    font-size: 0.9em;
  }
  .detail {
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    font-size: 0.85em;
  }
  .foot {
    margin: 0;
    font-size: 0.8em;
  }
  .spin {
    width: 0.7em;
    height: 0.7em;
    border: 2px solid var(--fg-muted);
    border-right-color: transparent;
    border-radius: 50%;
    animation: spin 0.8s linear infinite;
  }
  @keyframes spin {
    to {
      transform: rotate(360deg);
    }
  }
  @media (prefers-reduced-motion: reduce) {
    .spin {
      animation: none;
    }
  }
</style>
