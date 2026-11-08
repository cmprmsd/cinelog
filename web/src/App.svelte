<script lang="ts">
  import type { Health } from './lib/api/types';

  let version = $state<string | null>(null);

  $effect(() => {
    fetch('/healthz')
      .then((r) => (r.ok ? (r.json() as Promise<Health>) : null))
      .then((h) => (version = h?.version ?? null))
      .catch(() => (version = null));
  });
</script>

<main>
  <h1>cinelog</h1>
  <p>Server version: {version ?? 'unknown'}</p>
</main>
