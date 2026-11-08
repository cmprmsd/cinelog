<script lang="ts">
  import type { TreeNode } from '../api/types';
  import { fmtTs } from '../format';
  import { prefs } from '../prefs.svelte';
  import { router } from '../router.svelte';
  import TreeNodes from './TreeNodes.svelte';

  let { nodes, current }: { nodes: TreeNode[]; current: string | null } = $props();
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
        title={`${n.user} · ${n.commands} commands`}
      >
        {#if n.state === 'live'}<span class="live-dot" aria-label="live"></span>{/if}
        <span class="when">{fmtTs(n.started_at, prefs.utc).slice(5, 16)}</span>
        <span class="who">{n.user}</span>
        <span class="muted n">{n.commands}</span>
        {#if n.legacy}<span class="badge legacy">legacy</span>{/if}
        {#if n.hidden}<span class="badge">hidden</span>{/if}
      </a>
      {#if n.children.length}
        <TreeNodes nodes={n.children} {current} />
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
