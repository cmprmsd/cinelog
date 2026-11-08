<script lang="ts">
  import type { SnippetPart } from '../api/types';
  import { oneLine } from '../format';

  // Untrusted terminal text: rendered as text nodes only, hits inside <mark>.
  let { parts }: { parts: SnippetPart[] } = $props();
</script>

<span class="snippet">{#each parts as part, i (i)}{#if part.hit}<mark>{oneLine(part.text)}</mark>{:else}{oneLine(part.text)}{/if}{/each}</span>

<style>
  .snippet {
    font-family: var(--mono);
    white-space: pre;
  }
</style>
