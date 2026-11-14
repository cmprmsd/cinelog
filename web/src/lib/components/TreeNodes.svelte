<script lang="ts">
  import type { TmuxAttach, TreeNode } from '../api/types';
  import { fmtTs } from '../format';
  import { prefs } from '../prefs.svelte';
  import { router } from '../router.svelte';
  import { tmuxSessionLabel } from '../tmux';
  import TreeNodes from './TreeNodes.svelte';

  interface Props {
    nodes: TreeNode[];
    current: string | null;
    /** Start of the session these are nested in (a `sudo` shell inside it). */
    parentStart?: number | null;
    /** A click on the tmux session a shell attached to. */
    onattached: (a: TmuxAttach, e: Event) => void;
  }
  let { nodes, current, parentStart = null, onattached }: Props = $props();
  const nested = $derived(parentStart !== null);

  /** `MM-DD HH:MM`; a nested session of the same day as its parent shows the time only. */
  function when(n: TreeNode): string {
    const day = (ts: number) => fmtTs(ts, prefs.utc).slice(5, 10);
    const full = fmtTs(n.started_at, prefs.utc).slice(5, 16);
    return parentStart !== null && day(parentStart) === day(n.started_at) ? full.slice(6) : full;
  }
</script>

<ul class="nodes">
  {#each nodes as n (n.uuid)}
    <li>
      <a
        href={`/s/${n.uuid}`}
        onclick={router.link}
        class="node"
        class:current={current === n.uuid}
        aria-current={current === n.uuid ? 'page' : undefined}
        title={`${nested ? 'nested shell: ' : ''}${n.user}${n.sudo_user ? ` (sudo by ${n.sudo_user})` : ''} · ${n.commands} commands`}
        data-nested={nested}
      >
        {#if nested}<span class="nest" aria-hidden="true">↳</span>{/if}
        {#if n.state === 'live'}<span class="live-dot" aria-label="live"></span>{/if}
        <span class="when">{when(n)}</span>
        <span class="who">{n.user}</span>
        {#if nested && n.sudo_user}<span class="badge">sudo</span>{/if}
        <span class="muted n">{n.commands}</span>
        {#if n.legacy}<span class="badge legacy">legacy</span>{/if}
        {#if n.hidden}<span class="badge">hidden</span>{/if}
      </a>
      {#each n.attached as a (`${a.server}/${a.session_id}`)}
        <a class="attach" href="#tmux" onclick={(e) => onattached(a, e)} title="Its tmux client attached to this session; the panes are recorded there" data-testid="tree-attached">
          <span aria-hidden="true">⇥</span>
          <span class="muted">tmux</span>
          <span class="to">{tmuxSessionLabel(a)}</span>
        </a>
      {/each}
      {#if n.children.length}
        <TreeNodes nodes={n.children} {current} parentStart={n.started_at} {onattached} />
      {/if}
    </li>
  {/each}
</ul>

<style>
  .nodes {
    list-style: none;
    margin: 0;
    padding: 0 0 0 0.6rem;
  }
  .nodes :global(.nodes) {
    border-left: 1px dashed var(--border);
    margin-left: 0.4rem;
  }
  .node {
    display: flex;
    align-items: center;
    gap: 0.35rem;
    padding: 0.1rem 0.3rem;
    border-radius: 4px;
    color: var(--fg);
    text-decoration: none;
    white-space: nowrap;
    font-size: 0.9em;
  }
  .node:hover {
    background: var(--bg-hover);
  }
  .node.current {
    background: var(--accent-soft);
  }
  .nest {
    opacity: 0.7;
  }
  .attach {
    display: flex;
    gap: 0.3rem;
    align-items: baseline;
    margin-left: 1rem;
    padding: 0 0.3rem;
    border-radius: 4px;
    color: var(--fg);
    text-decoration: none;
    font-size: 0.8em;
    white-space: nowrap;
  }
  .attach:hover {
    background: var(--bg-hover);
  }
  .attach .to {
    font-weight: 600;
    overflow: hidden;
    text-overflow: ellipsis;
  }
  .when {
    font-family: var(--mono);
    font-size: 0.92em;
  }
  .who {
    overflow: hidden;
    text-overflow: ellipsis;
  }
  .n {
    margin-left: auto;
    font-size: 0.85em;
  }
</style>
