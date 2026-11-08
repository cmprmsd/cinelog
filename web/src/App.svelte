<script lang="ts">
  import { onMount } from 'svelte';
  import { api, ApiFailure, errorText, onUnauthorized } from './lib/api/client';
  import type { Me } from './lib/api/types';
  import Login from './lib/components/Login.svelte';
  import Shell from './lib/components/Shell.svelte';
  import { parseRoute, safeNext } from './lib/route';
  import { router } from './lib/router.svelte';

  type Auth = { state: 'checking' } | { state: 'in'; me: Me } | { state: 'out' } | { state: 'error'; message: string };

  let auth = $state<Auth>({ state: 'checking' });
  const route = $derived(parseRoute(router.path));

  async function check(): Promise<void> {
    try {
      auth = { state: 'in', me: await api.me() };
    } catch (e) {
      auth = e instanceof ApiFailure && e.status === 401 ? { state: 'out' } : { state: 'error', message: errorText(e) };
    }
  }

  onMount(() => {
    onUnauthorized(() => (auth = { state: 'out' }));
    void check();
    return () => onUnauthorized(null);
  });

  // Signed out anywhere: go to the login page and come back afterwards.
  $effect(() => {
    if (auth.state === 'out' && route.name !== 'login') {
      router.goto(`/login?next=${encodeURIComponent(location.pathname + location.search)}`, { replace: true });
    } else if (auth.state === 'in' && route.name === 'login') {
      router.goto(safeNext(router.query.get('next')), { replace: true });
    }
  });

  async function logout(): Promise<void> {
    try {
      await api.logout();
    } catch {
      // The cookie may already be gone; the login page is right either way.
    }
    auth = { state: 'out' };
  }
</script>

{#if route.name === 'login'}
  {#if auth.state !== 'in'}
    <Login onlogin={check} />
  {/if}
{:else if auth.state === 'in'}
  <Shell me={auth.me} {route} onlogout={logout} />
{:else if auth.state === 'error'}
  <main class="center">
    <p role="alert">The server could not be reached: {auth.message}</p>
    <button type="button" onclick={check}>Retry</button>
  </main>
{:else}
  <main class="center"><p class="muted" role="status">Loading…</p></main>
{/if}

<style>
  .center {
    display: grid;
    place-content: center;
    justify-items: center;
    gap: 0.6rem;
    min-height: 100%;
  }
</style>
