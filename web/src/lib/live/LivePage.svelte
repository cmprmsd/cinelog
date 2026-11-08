<script lang="ts">
  // Route `/live/<uuid>` (plan §8.4; docs/API.md §9.1, §9.6), mounted by `Shell` with exactly
  // these props; the root keeps `data-testid="live-page"` and `data-uuid`. A new uuid gets a
  // fresh view (stream, timers and bus state start over).
  import LiveView from './LiveView.svelte';

  let { uuid }: { uuid: string } = $props();
</script>

<div class="page" data-testid="live-page" data-uuid={uuid}>
  {#key uuid}
    <LiveView {uuid} />
  {/key}
</div>

<style>
  .page {
    height: 100%;
    overflow: auto;
  }
</style>
