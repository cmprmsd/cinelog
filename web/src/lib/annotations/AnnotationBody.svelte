<script lang="ts">
  // An annotation body: untrusted plain text, rendered as text nodes with http(s) URLs as
  // links that open in a new tab without referrer or opener (docs/API.md §9.4).
  import { linkify } from './annotations';

  let { body }: { body: string } = $props();
  const parts = $derived(linkify(body));
</script>

<p class="body" data-testid="annotation-body">{#each parts as p, i (i)}{#if p.href}<a
        href={p.href}
        rel="noopener noreferrer"
        target="_blank">{p.text}</a
      >{:else}{p.text}{/if}{/each}</p>

<style>
  .body {
    margin: 0;
    white-space: pre-wrap;
    overflow-wrap: anywhere;
  }
</style>
