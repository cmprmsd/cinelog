<script lang="ts">
  import type { Me } from '../api/types';
  import { tzLabel } from '../format';
  import { dispatch, useKeys } from '../keymap';
  import { focusKind, shortcutFor } from '../keys';
  import { nextTheme, prefs } from '../prefs.svelte';
  import LivePage from '../live/LivePage.svelte';
  import { liveHref, type Route } from '../route';
  import { router } from '../router.svelte';
  import TimelinePage from '../../routes/timeline/TimelinePage.svelte';
  import SearchBar from './SearchBar.svelte';
  import SearchPage from './SearchPage.svelte';
  import SessionPage from './SessionPage.svelte';
  import ShortcutSheet from './ShortcutSheet.svelte';
  import Sidebar from './Sidebar.svelte';

  let { me, route, onlogout }: { me: Me; route: Route; onlogout: () => void } = $props();

  let searchBar: SearchBar | undefined = $state();
  let sheet: ShortcutSheet | undefined = $state();
  let sidebarOpen = $state(false);

  const current = $derived(route.name === 'session' || route.name === 'live' ? route.uuid : null);

  function onkeydown(e: KeyboardEvent): void {
    const target = e.target;
    // A modal dialog handles its own keys (Esc closes it natively).
    if (target instanceof Element && target.closest('dialog[open]')) return;
    // Keys inside the player belong to the player.
    if (target instanceof Element && target.closest('.ap-player')) {
      if (e.key !== 'Escape') return;
    }
    const focus = focusKind(target);
    const action = shortcutFor(e, focus);
    if (!action) return;
    if (action === 'escape' && focus === 'typing' && target instanceof HTMLElement) {
      target.blur();
      e.preventDefault();
      return;
    }
    if (dispatch(action)) e.preventDefault();
  }

  $effect(() =>
    useKeys({
      'focus-search': () => searchBar?.focus(),
      help: () => sheet?.toggle(),
      timeline: () => router.goto('/timeline'),
      'toggle-live': () => {
        if (route.name === 'session') router.goto(liveHref(route.uuid));
        else if (route.name === 'live') router.goto(`/s/${route.uuid}`);
        else return false;
      },
      escape: () => {
        if (sheet?.close()) return;
        if (sidebarOpen) {
          sidebarOpen = false;
          return;
        }
        if (document.activeElement instanceof HTMLElement && document.activeElement !== document.body) {
          document.activeElement.blur();
          return;
        }
        return false;
      },
    }),
  );

  // On a session page, submitting the search goes to the result list with the same query.
  function submitSearch(): void {
    if (route.name === 'search') return;
    const qs = new URLSearchParams([...router.query].filter(([k]) => k !== 't' && k !== 'cmd')).toString();
    router.goto(`/${qs ? `?${qs}` : ''}`);
  }

  const themeLabel = $derived(prefs.theme === 'auto' ? `auto (${prefs.resolved})` : prefs.theme);
</script>

<svelte:window {onkeydown} />

<div class="shell">
  <header class="top">
    <button
      type="button"
      class="menu-btn"
      aria-label="Sessions"
      aria-expanded={sidebarOpen}
      aria-controls="sidebar"
      onclick={() => (sidebarOpen = !sidebarOpen)}>☰</button
    >
    <a class="brand" href="/" onclick={router.link}>cinelog</a>
    <SearchBar bind:this={searchBar} onsubmit={submitSearch} />
    <nav class="tabs" aria-label="Views">
      <a href="/" onclick={router.link} aria-current={route.name === 'search' ? 'page' : undefined}>Search</a>
      <a href="/timeline" onclick={router.link} aria-current={route.name === 'timeline' ? 'page' : undefined} data-testid="nav-timeline"
        >Timeline</a
      >
    </nav>
    <div class="tools">
      <button
        type="button"
        aria-pressed={prefs.utc}
        title="Show times in UTC"
        onclick={() => prefs.setUtc(!prefs.utc)}
        data-testid="tz-toggle">{tzLabel(prefs.utc)}</button
      >
      <button
        type="button"
        title="Theme: light, dark or follow the system"
        onclick={() => prefs.setTheme(nextTheme(prefs.theme))}
        data-testid="theme-toggle"
        data-theme-choice={prefs.theme}
        ><svg class="icon" viewBox="0 0 16 16" aria-hidden="true"
          >{#if prefs.resolved === 'dark'}<path d="M10.5 1.5a6.5 6.5 0 1 0 4 11.6A7 7 0 0 1 10.5 1.5z" />{:else}<circle
              cx="8"
              cy="8"
              r="3.2"
            /><path
              d="M8 .8v2.1M8 13.1v2.1M.8 8h2.1M13.1 8h2.1M2.9 2.9l1.5 1.5M11.6 11.6l1.5 1.5M2.9 13.1l1.5-1.5M11.6 4.4l1.5-1.5"
            />{/if}</svg
        >
        {themeLabel}</button
      >
      <button type="button" title="Keyboard shortcuts" aria-label="Keyboard shortcuts" onclick={() => sheet?.toggle()}>?</button>
      <span class="who" title={`${me.scope} server on ${me.host}, version ${me.version}`}>
        <span class="badge">{me.scope}</span>
        {me.principal}
      </span>
      <button type="button" onclick={onlogout}>Sign out</button>
    </div>
  </header>

  <div class="body">
    <aside id="sidebar" class:open={sidebarOpen}>
      <Sidebar {current} />
    </aside>
    <main class="main">
      {#if route.name === 'search'}
        <SearchPage />
      {:else if route.name === 'session'}
        <SessionPage uuid={route.uuid} />
      {:else if route.name === 'timeline'}
        <TimelinePage />
      {:else if route.name === 'live'}
        <LivePage uuid={route.uuid} />
      {:else}
        <div class="notfound">
          <h1>Not found</h1>
          <p><a href="/" onclick={router.link}>Back to search</a></p>
        </div>
      {/if}
    </main>
  </div>
</div>

<ShortcutSheet bind:this={sheet} />

<style>
  .shell {
    display: grid;
    grid-template-rows: auto minmax(0, 1fr);
    height: 100%;
  }
  .top {
    display: flex;
    gap: 0.6rem;
    align-items: flex-start;
    padding: 0.5rem 0.7rem;
    border-bottom: 1px solid var(--border);
    background: var(--bg-elev);
  }
  .brand {
    font-weight: 700;
    font-size: 1.1rem;
    color: var(--fg);
    text-decoration: none;
    padding-top: 0.25rem;
  }
  .tabs {
    display: flex;
    gap: 0.5rem;
    padding-top: 0.3rem;
    white-space: nowrap;
  }
  .tabs a {
    color: var(--fg-muted);
    text-decoration: none;
  }
  .tabs a[aria-current='page'] {
    color: var(--fg);
    font-weight: 600;
  }
  .tools {
    display: flex;
    gap: 0.35rem;
    align-items: center;
    flex-wrap: wrap;
    justify-content: flex-end;
  }
  .who {
    display: inline-flex;
    gap: 0.3rem;
    align-items: center;
    font-size: 0.9em;
  }
  .menu-btn {
    display: none;
  }
  .icon {
    width: 1em;
    height: 1em;
    fill: currentColor;
    stroke: currentColor;
    stroke-width: 1.4;
    stroke-linecap: round;
  }
  .body {
    display: grid;
    grid-template-columns: 16rem minmax(0, 1fr);
    min-height: 0;
  }
  aside {
    border-right: 1px solid var(--border);
    background: var(--bg-elev);
    min-height: 0;
    overflow: hidden;
  }
  .main {
    min-width: 0;
    min-height: 0;
  }
  .notfound {
    padding: 2rem;
  }
  @media (max-width: 900px) {
    .top {
      flex-wrap: wrap;
    }
    .menu-btn {
      display: inline-flex;
    }
    .body {
      grid-template-columns: minmax(0, 1fr);
    }
    aside {
      display: none;
      position: fixed;
      z-index: 20;
      top: 0;
      bottom: 0;
      left: 0;
      width: min(20rem, 85vw);
      box-shadow: var(--shadow);
    }
    aside.open {
      display: block;
    }
  }
</style>
