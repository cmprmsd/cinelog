<script lang="ts">
  import type { Command } from '../api/types';
  import { exitClass } from '../chapters';
  import { fmtDuration, oneLine } from '../format';

  interface Props {
    commands: Command[];
    /** Index of the chapter playing now. */
    current: number;
    /** Command of the selected hit. */
    focusSeq: number | null;
    onseek: (t: number) => void;
  }
  let { commands, current, focusSeq, onseek }: Props = $props();

  let list: HTMLOListElement | undefined = $state();

  $effect(() => {
    const i = current;
    const el = list?.children[i];
    if (!list || !el) return;
    // Only the strip scrolls (scrollIntoView would also move the page and the viewer).
    const r = el.getBoundingClientRect();
    const box = list.getBoundingClientRect();
    if (r.left < box.left) list.scrollLeft -= box.left - r.left;
    else if (r.right > box.right) list.scrollLeft += r.right - box.right;
  });

  const title = (c: Command) => {
    const parts = [`$ ${c.typed}`];
    if (c.expanded) parts.push(`→ ${c.expanded}`);
    parts.push(`exit ${c.exit ?? '?'}`);
    if (c.t_end !== null) parts.push(fmtDuration(c.t_end - c.t_start));
    if (c.cwd) parts.push(c.cwd);
    if (c.approx) parts.push(`approximate boundaries${c.confidence !== null ? ` (${Math.round(c.confidence * 100)}%)` : ''}`);
    return parts.join('\n');
  };
</script>

{#if commands.length}
  <ol class="strip" bind:this={list} aria-label="Commands" data-testid="command-strip">
    {#each commands as c, i (c.seq)}
      <li>
        <button
          type="button"
          class={`chap ${exitClass(c.exit)}`}
          class:approx={c.approx}
          class:current={i === current}
          class:focus={c.seq === focusSeq}
          aria-current={i === current ? 'step' : undefined}
          title={title(c)}
          data-seq={c.seq}
          onclick={() => onseek(c.t_start)}
        >
          <span class="text">$ {oneLine(c.typed)}</span>
          {#if c.approx}<span class="badge approx" title="approximate (legacy)">≈</span>{/if}
        </button>
      </li>
    {/each}
  </ol>
{:else}
  <p class="muted none">No commands recorded in this session.</p>
{/if}

<style>
  .strip {
    display: flex;
    gap: 0.3rem;
    overflow-x: auto;
    list-style: none;
    margin: 0;
    padding: 0.2rem 0 0.4rem;
    scrollbar-width: thin;
  }
  .chap {
    max-width: 16rem;
    font: 0.85em var(--mono);
    border-left-width: 4px;
    padding: 0.15em 0.5em;
  }
  .chap.ok {
    border-left-color: var(--ok);
  }
  .chap.fail {
    border-left-color: var(--fail);
  }
  .chap.unknown {
    border-left-color: var(--unknown);
  }
  .chap.approx {
    background-image: repeating-linear-gradient(135deg, transparent 0 5px, var(--unknown-soft) 5px 9px);
  }
  .chap.current {
    background-color: var(--accent-soft);
    border-color: var(--accent);
  }
  .chap.focus {
    outline: 2px solid var(--mark-bg);
    outline-offset: -2px;
  }
  .text {
    overflow: hidden;
    text-overflow: ellipsis;
  }
  .none {
    margin: 0;
  }
</style>
