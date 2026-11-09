<script lang="ts">
  import { stateFromQuery } from '../filters';
  import { router } from '../router.svelte';
  import SessionView from './SessionView.svelte';

  let { uuid }: { uuid: string } = $props();

  const search = $derived(stateFromQuery(router.query));
  const num = (v: string | null) => {
    if (v === null || v === '') return null;
    const n = Number(v);
    return Number.isFinite(n) && n >= 0 ? n : null;
  };
  // `?cmd=` addresses a command (the copy link), `?t=` a time; both from the URL.
  const tParam = $derived(router.query.get('t'));
  const cmdParam = $derived(router.query.get('cmd'));
  const target = $derived({ t: num(tParam), cmd: num(cmdParam) });
</script>

<div class="page">
  {#key uuid}
    <SessionView {uuid} {target} {search} />
  {/key}
</div>

<style>
  .page {
    height: 100%;
    overflow: auto;
    max-width: 90rem;
    /* The session view sizes its player to this pane's height (cqh). */
    container-type: size;
  }
</style>
