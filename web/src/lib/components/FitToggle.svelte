<script lang="ts">
  // The header button for the fit mode (`w`). Shows the mode in effect when "screen" had to
  // fall back to the width because the terminal would be too small to read.
  import { MIN_FONT_PX, type FitMode } from '../fit';
  import { prefs } from '../prefs.svelte';

  let { effective }: { effective: FitMode } = $props();

  const fallback = $derived(prefs.fit === 'screen' && effective === 'width');
  const title = $derived(
    fallback
      ? `Fitted to the screen the terminal would be smaller than ${MIN_FONT_PX} px, so it fills the width and scrolls (w)`
      : prefs.fit === 'screen'
        ? 'The whole terminal fits the window; switch to fill the width and scroll (w)'
        : 'The terminal fills the width and scrolls; switch to fit the whole terminal into the window (w)',
  );
</script>

<button
  type="button"
  onclick={() => prefs.toggleFit()}
  {title}
  data-testid="fit-toggle"
  data-fit-mode={prefs.fit}
  data-fit={effective}
  >Fit: {prefs.fit}{#if fallback}<span class="fallback" data-testid="fit-fallback"> → width</span>{/if}</button
>

<style>
  .fallback {
    color: var(--fg-muted);
  }
</style>
