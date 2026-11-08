<script lang="ts">
  import { onMount } from 'svelte';
  import { api, errorText, isAbort } from '../api/client';
  import type { Session, Status, Tree, TreeParams } from '../api/types';
  import { fmtTs } from '../format';
  import { prefs } from '../prefs.svelte';
  import { router } from '../router.svelte';
  import TreeNodes from './TreeNodes.svelte';

  let { current }: { current: string | null } = $props();

  type Span = '7d' | '30d' | 'all';
  const LIVE_POLL_MS = 5000;
  const TREE_REFRESH_MS = 60_000;

  let live = $state<Session[]>([]);
  let tree = $state<Tree | null>(null);
  let treeError = $state<string | null>(null);
  let status = $state<Status | null>(null);
  let span = $state<Span>('30d');
  /** Open tree branches; children render only when open (large legacy corpora). */
  let open = $state<Record<string, boolean>>({});

  const liveKey = $derived(live.map((s) => s.uuid).join(','));

  function treeParams(w: Span): TreeParams {
    const now = Math.floor(Date.now() / 1000);
    if (w === 'all') return {};
    return { from: now - (w === '7d' ? 7 : 30) * 86400 };
  }

  onMount(() => {
    let stopped = false;
    let timer: ReturnType<typeof setTimeout> | undefined;
    const poll = async () => {
      try {
        const page = await api.sessions({ live: true, limit: 50 });
        if (!stopped) live = page.sessions;
      } catch {
        // Keep the last list; the next poll retries.
      }
      if (!stopped) timer = setTimeout(poll, LIVE_POLL_MS);
    };
    void poll();
    api.status().then(
      (s) => (status = s),
      () => (status = null),
    );
    return () => {
      stopped = true;
      clearTimeout(timer);
    };
  });

  $effect(() => {
    const params = treeParams(span);
    void liveKey;
    const ctl = new AbortController();
    const load = () =>
      api.tree(params, ctl.signal).then(
        (t) => {
          tree = t;
          treeError = null;
        },
        (e) => {
          if (!isAbort(e)) treeError = errorText(e);
        },
      );
    void load();
    const timer = setInterval(load, TREE_REFRESH_MS);
    return () => {
      ctl.abort();
      clearInterval(timer);
    };
  });

  const isOpen = (key: string, fallback: boolean) => open[key] ?? fallback;
  const toggle = (key: string, e: Event) => (open[key] = (e.currentTarget as HTMLDetailsElement).open);
  const where = (s: Session) =>
    s.tmux.pane ? `${s.tmux.session ?? '?'}:${s.tmux.window ?? '?'} ${s.tmux.pane}` : (s.holder_comm ?? s.tty ?? '');
</script>

<nav class="sidebar" aria-label="Sessions">
  <section aria-labelledby="live-h">
    <h2 id="live-h">Live <span class="muted">({live.length})</span></h2>
    {#if live.length === 0}
      <p class="muted empty">No live sessions.</p>
    {:else}
      <ul class="live" data-testid="live-list">
        {#each live as s (s.uuid)}
          <li>
            <a href={`/s/${s.uuid}`} onclick={router.link} class:current={current === s.uuid}>
              <span class="live-dot" aria-hidden="true"></span>
              <span class="who">{s.user}@{s.host}</span>
              <span class="muted where">{where(s)}</span>
            </a>
          </li>
        {/each}
      </ul>
    {/if}
  </section>

  <section aria-labelledby="tree-h" class="tree">
    <div class="tree-head">
      <h2 id="tree-h">Sessions</h2>
      <label class="sr-only" for="tree-window">Time window</label>
      <select id="tree-window" bind:value={span}>
        <option value="7d">7 days</option>
        <option value="30d">30 days</option>
        <option value="all">all</option>
      </select>
    </div>
    {#if treeError}
      <p class="muted empty">{treeError}</p>
    {:else if !tree}
      <p class="muted empty">Loading…</p>
    {:else if tree.hosts.length === 0}
      <p class="muted empty">No sessions in this window.</p>
    {:else}
      <div data-testid="session-tree">
        {#each tree.hosts as h (h.host)}
          {@const hk = `h:${h.host}`}
          <details open={isOpen(hk, true)} ontoggle={(e) => toggle(hk, e)}>
            <summary class="host">{h.host}</summary>
            {#if isOpen(hk, true)}
              {#each h.tmux_servers as srv (srv.server)}
                {@const sk = `${hk}/t:${srv.server}`}
                <details open={isOpen(sk, true)} ontoggle={(e) => toggle(sk, e)} class="lvl">
                  <summary>tmux <span class="muted mono">{srv.server.split(':')[0]}</span></summary>
                  {#if isOpen(sk, true)}
                    {#each srv.sessions as ts, i (ts.session_id ?? i)}
                      {@const tk = `${sk}/s:${ts.session_id ?? i}`}
                      <details open={isOpen(tk, false)} ontoggle={(e) => toggle(tk, e)} class="lvl">
                        <summary>{ts.name ?? ts.session_id ?? 'session'}</summary>
                        {#if isOpen(tk, false)}
                          {#each ts.windows as w, wi (w.window ?? wi)}
                            {@const wk = `${tk}/w:${w.window ?? wi}`}
                            <details open={isOpen(wk, true)} ontoggle={(e) => toggle(wk, e)} class="lvl">
                              <summary>{w.window ?? '?'}{#if w.name}: {w.name}{/if}</summary>
                              {#each w.panes as p, pi (p.pane ?? pi)}
                                <div class="lvl pane">
                                  <span class="muted mono">{p.pane ?? 'pane'}</span>
                                  <TreeNodes nodes={p.sessions} {current} />
                                </div>
                              {/each}
                            </details>
                          {/each}
                        {/if}
                      </details>
                    {/each}
                  {/if}
                </details>
              {/each}
              {#each h.shells as g, gi (g.holder_comm ?? `none-${gi}`)}
                {@const gk = `${hk}/g:${g.holder_comm ?? ''}`}
                <details open={isOpen(gk, false)} ontoggle={(e) => toggle(gk, e)} class="lvl">
                  <summary>shells: {g.holder_comm ?? 'other'} <span class="muted">({g.sessions.length})</span></summary>
                  {#if isOpen(gk, false)}
                    <TreeNodes nodes={g.sessions} {current} />
                  {/if}
                </details>
              {/each}
            {/if}
          </details>
        {/each}
      </div>
    {/if}
  </section>

  {#if status}
    <footer class="muted status" data-testid="status">
      <span>{status.scope} · v{status.version}</span>
      <span>{status.commands.toLocaleString()} commands</span>
      {#if status.backlog_bytes > 0}<span>indexing…</span>{/if}
      {#if status.last_error}<span class="err" title={status.last_error}>indexer error</span>{/if}
      <span>started {fmtTs(status.server_started_at, prefs.utc).slice(5, 16)}</span>
    </footer>
  {/if}
</nav>

<style>
  .sidebar {
    display: flex;
    flex-direction: column;
    gap: 0.8rem;
    padding: 0.6rem;
    overflow: auto;
    min-height: 0;
    height: 100%;
  }
  h2 {
    font-size: 0.8rem;
    text-transform: uppercase;
    letter-spacing: 0.05em;
    color: var(--fg-muted);
    margin: 0 0 0.3rem;
  }
  .empty {
    margin: 0;
    font-size: 0.9em;
  }
  .live {
    list-style: none;
    margin: 0;
    padding: 0;
  }
  .live a {
    display: flex;
    align-items: center;
    gap: 0.4rem;
    padding: 0.2rem 0.3rem;
    border-radius: 4px;
    color: var(--fg);
    text-decoration: none;
    white-space: nowrap;
    overflow: hidden;
  }
  .live a:hover {
    background: var(--bg-hover);
  }
  .live a.current {
    background: var(--accent-soft);
  }
  .where {
    font-size: 0.85em;
    overflow: hidden;
    text-overflow: ellipsis;
  }
  .tree {
    flex: 1;
    min-height: 0;
  }
  .tree-head {
    display: flex;
    justify-content: space-between;
    align-items: baseline;
  }
  .tree-head select {
    font-size: 0.85em;
    padding: 0.1em 0.3em;
  }
  summary {
    cursor: pointer;
    padding: 0.1rem 0.2rem;
    border-radius: 4px;
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
  }
  summary:hover {
    background: var(--bg-hover);
  }
  .host {
    font-weight: 600;
  }
  .lvl {
    margin-left: 0.7rem;
  }
  .pane {
    margin-top: 0.1rem;
  }
  .status {
    display: flex;
    flex-wrap: wrap;
    gap: 0.2rem 0.6rem;
    font-size: 0.8em;
    border-top: 1px solid var(--border);
    padding-top: 0.4rem;
  }
  .err {
    color: var(--fail);
  }
</style>
