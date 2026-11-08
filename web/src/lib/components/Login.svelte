<script lang="ts">
  import { onMount } from 'svelte';
  import { api, ApiFailure, errorText } from '../api/client';
  import type { LoginInfo } from '../api/types';

  let { onlogin }: { onlogin: () => void } = $props();

  let info = $state<LoginInfo | null>(null);
  let token = $state('');
  let busy = $state(false);
  let error = $state<string | null>(null);
  let input: HTMLInputElement | undefined = $state();

  onMount(() => {
    input?.focus();
    api.loginInfo().then(
      (i) => (info = i),
      () => (info = null),
    );
  });

  async function submit(e: SubmitEvent) {
    e.preventDefault();
    if (!token.trim() || busy) return;
    busy = true;
    error = null;
    try {
      await api.login(token);
      token = '';
      onlogin();
    } catch (err) {
      if (err instanceof ApiFailure && err.status === 401) error = 'That token is not valid.';
      else if (err instanceof ApiFailure && err.status === 429)
        error = `Too many attempts. Try again in ${err.retryAfter ?? 60} s.`;
      else error = errorText(err);
      input?.select();
    } finally {
      busy = false;
    }
  }
</script>

<main class="login">
  <!-- No `name` on the field and method=post: the token can never end up in a URL. -->
  <form method="post" onsubmit={submit} aria-describedby="login-help">
    <h1>cinelog</h1>
    {#if info}
      <p class="muted">{info.scope === 'team' ? 'Team server' : 'Personal server'} · version {info.version}</p>
    {/if}
    <label for="token">Access token</label>
    <input
      id="token"
      bind:this={input}
      bind:value={token}
      type="password"
      autocomplete="off"
      autocapitalize="off"
      spellcheck="false"
      required
      aria-invalid={error ? 'true' : undefined}
    />
    {#if error}<p class="error" role="alert">{error}</p>{/if}
    <button class="primary" type="submit" disabled={busy}>{busy ? 'Signing in…' : 'Sign in'}</button>
    <p id="login-help" class="muted small">
      The token was printed once by <code>cinelog install</code> or <code>cinelog token new</code>. It is sent
      once; the browser then keeps an HttpOnly session cookie.
    </p>
    {#if info}
      <section class="fp">
        <h2>Server certificate</h2>
        <p class="muted small">Compare with the fingerprint <code>cinelog install</code> or <code>cinelog tls show</code> printed:</p>
        <code data-testid="fingerprint">{info.fingerprint}</code>
      </section>
    {/if}
  </form>
</main>

<style>
  .login {
    display: grid;
    place-items: center;
    min-height: 100%;
    padding: 1rem;
  }
  form {
    display: grid;
    gap: 0.6rem;
    width: min(28rem, 100%);
    padding: 1.5rem;
    border: 1px solid var(--border);
    border-radius: 10px;
    background: var(--bg-elev);
    box-shadow: var(--shadow);
  }
  h1 {
    margin: 0;
    font-size: 1.5rem;
  }
  h2 {
    margin: 0;
    font-size: 0.95rem;
  }
  p {
    margin: 0;
  }
  .small {
    font-size: 0.85em;
  }
  .error {
    color: var(--fail);
  }
  .fp {
    display: grid;
    gap: 0.3rem;
    padding-top: 0.6rem;
    border-top: 1px solid var(--border);
  }
  .fp code {
    word-break: break-all;
    font-size: 0.8em;
  }
  input {
    font-family: var(--mono);
  }
</style>
