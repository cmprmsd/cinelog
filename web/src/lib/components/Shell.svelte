<script lang="ts">
  import { tick, untrack } from 'svelte';
  import { MediaQuery } from 'svelte/reactivity';
  import type { Me } from '../api/types';
  import { dragOffset, EDGE, settle, Swipe } from '../drawer';
  import { tzLabel } from '../format';
  import { dispatch, useKeys } from '../keymap';
  import { focusKind, shortcutFor } from '../keys';
  import { nextTheme, prefs } from '../prefs.svelte';
  import LivePage from '../live/LivePage.svelte';
  import { liveHref, type Route } from '../route';
  import { router } from '../router.svelte';
  import AnnotationsPage from '../../routes/annotations/AnnotationsPage.svelte';
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
  let toolsOpen = $state(false);
  let menuBtn: HTMLButtonElement | undefined = $state();
  let aside: HTMLElement | undefined = $state();
  let toolsEl: HTMLElement | undefined = $state();
  let moreBtn: HTMLButtonElement | undefined = $state();

  const current = $derived(route.name === 'session' || route.name === 'live' ? route.uuid : null);

  // The sidebar is an overlay drawer on narrow screens and on touch tablets and foldables;
  // the CSS keys off the `drawer` class so this query is the only breakpoint for it.
  const drawerQuery = new MediaQuery('(max-width: 900px), (pointer: coarse) and (max-width: 1100px)');
  const drawer = $derived(drawerQuery.current);
  const modal = $derived(drawer && sidebarOpen);
  /** The drawer's offset (px) while a finger drags it; null otherwise. */
  let drag = $state<number | null>(null);
  let swipe: Swipe | null = null;
  /** Set by a drag, so the click that may end it does not follow a link. */
  let dragged = false;

  async function openDrawer(): Promise<void> {
    toolsOpen = false;
    sidebarOpen = true;
    await tick();
    aside?.focus({ preventScroll: true });
  }

  /** Closes the drawer; focus inside it (which goes inert) or lost to a tap on the
      backdrop returns to the toggle. */
  async function closeDrawer(): Promise<void> {
    if (!sidebarOpen) return;
    const active = document.activeElement;
    const inside = !active || active === document.body || (aside?.contains(active) ?? false);
    sidebarOpen = false;
    if (!inside || !drawer) return;
    await tick();
    menuBtn?.focus({ preventScroll: true });
  }

  // Picking a session (or any navigation) closes the drawer and the tools menu.
  $effect(() => {
    void route;
    untrack(() => {
      toolsOpen = false;
      void closeDrawer();
    });
  });

  // Back to the wide layout: no drawer state left behind.
  $effect(() => {
    if (!drawer) {
      untrack(() => {
        sidebarOpen = false;
        drag = null;
        swipe = null;
      });
    }
  });

  const width = () => aside?.offsetWidth || 1;

  // Swiping the open drawer left closes it; vertical moves scroll the session tree
  // (`touch-action: pan-y` leaves them to the browser).
  function onpointerdown(e: PointerEvent): void {
    if (!modal || e.pointerType === 'mouse' || !e.isPrimary) return;
    swipe = new Swipe(e.clientX, e.clientY, e.timeStamp);
    dragged = false;
  }

  function onpointermove(e: PointerEvent): void {
    if (!swipe || e.pointerType === 'mouse') return;
    const axis = swipe.move(e.clientX, e.clientY, e.timeStamp);
    if (axis === 'y') {
      swipe = null;
    } else if (axis === 'x') {
      if (drag === null) aside?.setPointerCapture(e.pointerId);
      dragged = true;
      drag = dragOffset(true, swipe.dx, width());
    }
  }

  function onpointerup(e: PointerEvent): void {
    if (!swipe) return;
    const s = swipe;
    swipe = null;
    if (drag === null) return;
    drag = null;
    if (e.type === 'pointerup' && !settle(true, s.dx, s.velocity(), width())) void closeDrawer();
  }

  function onclickcapture(e: MouseEvent): void {
    if (!dragged) return;
    dragged = false;
    e.preventDefault();
    e.stopPropagation();
  }

  // A swipe from the left screen edge pulls the drawer open. Touch events, as the page
  // itself may claim the gesture for scrolling (which cancels pointer events).
  $effect(() => {
    if (!drawer) return;
    let edge: Swipe | null = null;
    const onmove = (e: TouchEvent) => {
      const t = e.touches[0];
      if (!edge || !t) return;
      const axis = edge.move(t.clientX, t.clientY, e.timeStamp);
      if (axis === 'y') {
        end();
      } else if (axis === 'x') {
        if (e.cancelable) e.preventDefault();
        drag = dragOffset(false, edge.dx, width());
      }
    };
    const end = (e?: TouchEvent) => {
      window.removeEventListener('touchmove', onmove);
      const s = edge;
      edge = null;
      if (!s || drag === null) return;
      drag = null;
      if (e?.type === 'touchend' && settle(false, s.dx, s.velocity(), width())) void openDrawer();
    };
    const onstart = (e: TouchEvent) => {
      const t = e.touches[0];
      if (sidebarOpen || e.touches.length !== 1 || !t || t.clientX > EDGE) return;
      edge = new Swipe(t.clientX, t.clientY, e.timeStamp);
      // Not passive: a horizontal edge drag must not also scroll or navigate back.
      window.addEventListener('touchmove', onmove, { passive: false });
    };
    window.addEventListener('touchstart', onstart, { passive: true });
    window.addEventListener('touchend', end);
    window.addEventListener('touchcancel', end);
    return () => {
      end();
      window.removeEventListener('touchstart', onstart);
      window.removeEventListener('touchend', end);
      window.removeEventListener('touchcancel', end);
    };
  });

  // The compact header's tools menu closes on a tap elsewhere.
  $effect(() => {
    if (!toolsOpen) return;
    const onout = (e: PointerEvent) => {
      const t = e.target;
      if (t instanceof Node && (toolsEl?.contains(t) || moreBtn?.contains(t))) return;
      toolsOpen = false;
    };
    window.addEventListener('pointerdown', onout, true);
    return () => window.removeEventListener('pointerdown', onout, true);
  });

  function onkeydown(e: KeyboardEvent): void {
    const target = e.target;
    // A modal dialog handles its own keys (Esc closes it natively).
    if (target instanceof Element && target.closest('dialog[open]')) return;
    if (e.key === 'Escape' && modal) {
      e.preventDefault();
      void closeDrawer();
      return;
    }
    if (e.key === 'Escape' && toolsOpen) {
      e.preventDefault();
      toolsOpen = false;
      moreBtn?.focus();
      return;
    }
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
        if (document.activeElement instanceof HTMLElement && document.activeElement !== document.body) {
          document.activeElement.blur();
          return;
        }
        return false;
      },
    }),
  );

  // On a session page, submitting the search goes to the result list with the same query.
  // The annotations overview's URL has its own `q` and `tag`: the bar is detached there and
  // hands over the search to run.
  const detached = $derived(route.name === 'annotations');

  function submitSearch(query?: URLSearchParams): void {
    if (route.name === 'search') return;
    const sp = query ?? new URLSearchParams([...router.query].filter(([k]) => k !== 't' && k !== 'cmd'));
    const qs = sp.toString();
    router.goto(`/${qs ? `?${qs}` : ''}`);
  }

  const themeLabel = $derived(prefs.theme === 'auto' ? `auto (${prefs.resolved})` : prefs.theme);
</script>

<svelte:window {onkeydown} />

<div class="shell" class:drawer>
  <header class="top" inert={modal}>
    <button
      type="button"
      class="menu-btn"
      aria-label="Sessions"
      aria-expanded={sidebarOpen}
      aria-controls="sidebar"
      bind:this={menuBtn}
      onclick={() => (sidebarOpen ? closeDrawer() : openDrawer())}>☰</button
    >
    <a class="brand" href="/" onclick={router.link}>cinelog</a>
    <div class="search">
      <SearchBar bind:this={searchBar} onsubmit={submitSearch} {detached} />
    </div>
    <nav class="tabs" aria-label="Views">
      <a href="/" onclick={router.link} aria-current={route.name === 'search' ? 'page' : undefined}>Search</a>
      <a href="/timeline" onclick={router.link} aria-current={route.name === 'timeline' ? 'page' : undefined} data-testid="nav-timeline"
        >Timeline</a
      >
      <a
        href="/annotations"
        onclick={router.link}
        aria-current={route.name === 'annotations' ? 'page' : undefined}
        data-testid="nav-annotations">Annotations</a
      >
    </nav>
    <button
      type="button"
      class="more-btn"
      aria-label="More"
      aria-expanded={toolsOpen}
      aria-controls="tools"
      bind:this={moreBtn}
      onclick={() => (toolsOpen = !toolsOpen)}
      data-testid="more-toggle">⋯</button
    >
    <div class="tools" id="tools" class:open={toolsOpen} bind:this={toolsEl}>
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
    {#if drawer && (sidebarOpen || drag !== null)}
      <div
        class="scrim"
        aria-hidden="true"
        style:opacity={drag === null ? undefined : 1 + drag / width()}
        onclick={() => closeDrawer()}
        data-testid="drawer-scrim"
      ></div>
    {/if}
    <!-- As a drawer it is a dialog that takes focus itself when opened. -->
    <!-- svelte-ignore a11y_no_noninteractive_tabindex -->
    <aside
      id="sidebar"
      class:open={sidebarOpen}
      class:dragging={drag !== null}
      style:transform={drag === null ? undefined : `translateX(${drag}px)`}
      role={drawer ? 'dialog' : undefined}
      aria-modal={modal ? 'true' : undefined}
      aria-label={drawer ? 'Sessions' : undefined}
      tabindex={drawer ? -1 : undefined}
      inert={drawer && !sidebarOpen}
      bind:this={aside}
      {onpointerdown}
      {onpointermove}
      {onpointerup}
      onpointercancel={onpointerup}
      {onclickcapture}
    >
      <Sidebar {current} />
    </aside>
    <main class="main" inert={modal}>
      {#if route.name === 'search'}
        <SearchPage />
      {:else if route.name === 'session'}
        <SessionPage uuid={route.uuid} />
      {:else if route.name === 'timeline'}
        <TimelinePage />
      {:else if route.name === 'annotations'}
        <AnnotationsPage />
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
    position: relative;
    display: flex;
    flex-wrap: wrap;
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
  .search {
    display: grid;
    flex: 1 1 22rem;
    min-width: 0;
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
    margin-left: auto;
  }
  .who {
    display: inline-flex;
    gap: 0.3rem;
    align-items: center;
    font-size: 0.9em;
  }
  .menu-btn,
  .more-btn {
    display: none;
    font-size: 1.15em;
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
    /* Its z-indexed parts (the player's control bar, the session header) stay inside it,
       under the drawer and its scrim. */
    isolation: isolate;
  }
  .notfound {
    padding: 2rem;
  }

  /* Below the wide layout the search bar takes a row of its own. */
  @media (max-width: 1100px) {
    .search {
      order: 1;
      flex-basis: 100%;
    }
  }

  /* Compact header: the tools fold into a menu. */
  @media (max-width: 760px) {
    .top {
      gap: 0.4rem;
      align-items: center;
    }
    .brand,
    .tabs {
      padding-top: 0;
    }
    .more-btn {
      display: inline-flex;
      justify-content: center;
      margin-left: auto;
    }
    .tools {
      display: none;
    }
    .tools.open {
      display: flex;
      position: absolute;
      z-index: 30;
      top: calc(100% - 0.3rem);
      right: 0.7rem;
      flex-direction: column;
      align-items: stretch;
      gap: 0.3rem;
      min-width: 12rem;
      max-width: calc(100vw - 1.4rem);
      padding: 0.5rem;
      border: 1px solid var(--border);
      border-radius: var(--radius);
      background: var(--bg-elev);
      box-shadow: var(--shadow);
    }
    .tools.open .who {
      order: -1;
      padding: 0.2rem 0.3rem;
    }
  }

  /* Touch-sized controls. */
  @media (pointer: coarse), (max-width: 760px) {
    .top button,
    .brand,
    .tabs a {
      min-height: 40px;
    }
    .brand,
    .tabs a {
      display: inline-flex;
      align-items: center;
      padding-top: 0;
    }
    .tabs {
      padding-top: 0;
      gap: 0.2rem;
    }
    .tabs a {
      padding: 0 0.4rem;
    }
    .menu-btn,
    .more-btn,
    .tools button {
      min-width: 40px;
      justify-content: center;
    }
  }

  .drawer .menu-btn {
    display: inline-flex;
    justify-content: center;
  }
  .drawer .body {
    grid-template-columns: minmax(0, 1fr);
  }
  /* The drawer and its scrim cover everything, header and player included. */
  .drawer aside {
    position: fixed;
    z-index: 40;
    top: 0;
    bottom: 0;
    left: 0;
    width: min(20rem, 85vw);
    box-shadow: var(--shadow);
    transform: translateX(-100%);
    visibility: hidden;
    transition:
      transform 0.2s ease,
      visibility 0s linear 0.2s;
    /* Horizontal drags are the drawer's; vertical ones scroll the session tree. */
    touch-action: pan-y;
  }
  .drawer aside :global(.sidebar) {
    /* Chromium stops the touch-action walk at the scroller. */
    touch-action: pan-y;
  }
  .drawer aside:focus {
    outline: none;
  }
  .drawer aside.open {
    transform: none;
    visibility: visible;
    transition: transform 0.2s ease;
  }
  .drawer aside.dragging {
    visibility: visible;
    transition: none;
  }
  .scrim {
    position: fixed;
    z-index: 39;
    inset: 0;
    background: rgb(0 0 0 / 0.45);
    touch-action: none;
    animation: fade-in 0.2s ease;
  }
  @keyframes fade-in {
    from {
      opacity: 0;
    }
  }
  @media (prefers-reduced-motion: reduce) {
    .drawer aside,
    .drawer aside.open {
      transition: none;
    }
    .scrim {
      animation: none;
    }
  }
</style>
