<script lang="ts">
  // Live view (work package M4-D; plan §8.4; docs/API.md §9.1, §9.6). Stand-in until M4-D
  // lands: shows the recording so far. Route `/live/<uuid>`, mounted by `Shell` with
  // exactly these props; the root keeps `data-testid="live-page"` and `data-uuid`.
  import SessionView from '../components/SessionView.svelte';
  import { stateFromQuery } from '../filters';
  import { router } from '../router.svelte';

  let { uuid }: { uuid: string } = $props();

  const search = $derived(stateFromQuery(router.query));
</script>

<div class="page" data-testid="live-page" data-uuid={uuid}>
  {#key uuid}
    <SessionView {uuid} target={{}} {search} />
  {/key}
</div>

<style>
  .page {
    height: 100%;
    overflow: auto;
  }
</style>
