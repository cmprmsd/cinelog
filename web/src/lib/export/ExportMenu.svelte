<script lang="ts">
  // Export menu of the session view (work package M4-D; plan §8.4; docs/API.md §7, §9.5):
  // the current command, the rail selection or the whole session as .cast, .txt or .html.
  // Downloads go through `fetch` + a blob URL so the session cookie applies. `SessionView`
  // mounts it with exactly these props and calls `open()` for the `e` key.
  import { api, errorText } from '../api/client';
  import type { Command, ExportFormat, ExportParams } from '../api/types';
  import { download } from '../download';
  import { fmtClock, shortId } from '../format';
  import type { TimeRange } from '../spans';

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

  type Source = 'command' | 'selection' | 'session';
  const FORMATS: readonly ExportFormat[] = ['cast', 'txt', 'html'];

  let menu: HTMLDetailsElement | undefined = $state();

  /** Opens the menu with focus on its first enabled entry. */
  export function open(): boolean {
    if (!menu) return false;
    menu.open = true;
    menu.querySelector<HTMLButtonElement>('button:not(:disabled)')?.focus();
    return true;
  }

  /** Closes the menu; true when it was open. */
  export function close(): boolean {
    if (!menu?.open) return false;
    menu.open = false;
    return true;
  }

  function params(format: ExportFormat, source: Source): ExportParams | null {
    if (source === 'command') return command ? { format, cmd: command.seq } : null;
    if (source === 'selection') return selection ? { format, from: selection.from, to: selection.to } : null;
    return { format };
  }

  async function run(format: ExportFormat, source: Source): Promise<void> {
    close();
    const p = params(format, source);
    if (!p) return;
    try {
      const name = await download(api.exportUrl(uuid, p), `cinelog-${shortId(uuid)}.${format}`);
      onnotice(`Downloaded ${name}.`);
    } catch (e) {
      onnotice(`Export failed: ${errorText(e)}`);
    }
  }
</script>

<details class="menu" bind:this={menu} data-testid="export-menu">
  <summary class="btn">Export</summary>
  <div class="menu-body" role="menu">
    <span class="muted">Current command{command ? '' : ' (none)'}</span>
    {#each FORMATS as f (f)}
      <button type="button" role="menuitem" disabled={!command} onclick={() => run(f, 'command')}>.{f}</button>
    {/each}
    {#if selection}
      <span class="muted">Selection {fmtClock(selection.from)}–{fmtClock(selection.to)}</span>
      {#each FORMATS as f (f)}
        <button type="button" role="menuitem" onclick={() => run(f, 'selection')}>.{f}</button>
      {/each}
    {/if}
    <span class="muted">Whole session</span>
    {#each FORMATS as f (f)}
      <button type="button" role="menuitem" onclick={() => run(f, 'session')}>.{f}</button>
    {/each}
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
    grid-template-columns: repeat(3, auto);
    gap: 0.3rem;
    padding: 0.5rem;
    margin-top: 0.2rem;
    border: 1px solid var(--border);
    border-radius: var(--radius);
    background: var(--bg-elev);
    box-shadow: var(--shadow);
  }
  .menu-body span {
    grid-column: 1 / -1;
    font-size: 0.85em;
    white-space: nowrap;
  }
</style>
