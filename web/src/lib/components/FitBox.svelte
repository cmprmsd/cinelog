<script lang="ts">
  // The player's box: fills its positioned parent, which the page sizes to what the window
  // leaves (so the rail stays in view). The player always fits the width of `mount`; for
  // "screen" this box narrows the mount until the whole terminal fits its height, for "width"
  // (or a font below 10 px) the mount takes the full width and the box scrolls. Mode and
  // terminal size only change the mount's width, never the box: a resize in the recording
  // does not move the page, and the player is not recreated.
  import { fitLayout, type FitMode } from '../fit';
  import { cellMetrics } from '../player';

  interface Props {
    /** The terminal's current size (0: not known yet). */
    cols: number;
    rows: number;
    mode: FitMode;
    /** Whether the player shows its control bar (it takes height from the box). */
    controls?: boolean;
    mount?: HTMLDivElement;
    /** The mode in effect. */
    effective?: FitMode;
  }

  let { cols, rows, mode, controls = true, mount = $bindable(), effective = $bindable('screen') }: Props = $props();

  let boxW = $state(0);
  let boxH = $state(0);

  // Measured once the box is laid out: the stylesheet is loaded by then.
  const cell = $derived(boxW > 0 ? cellMetrics() : undefined);
  const layout = $derived(fitLayout({ width: boxW, height: boxH }, { cols, rows }, mode, cell, controls ? undefined : 0));

  $effect(() => {
    effective = layout.effective;
  });
</script>

<div
  class="fit"
  bind:clientWidth={boxW}
  bind:clientHeight={boxH}
  data-testid="fit-box"
  data-fit={layout.effective}
  data-fit-mode={mode}
  data-term={`${cols}x${rows}`}
  data-font-px={layout.fontPx.toFixed(1)}
>
  <div class="scroller" class:scroll={layout.effective === 'width'}>
    <div class="mount" bind:this={mount} style:width={layout.effective === 'screen' ? `${layout.width}px` : undefined}></div>
  </div>
</div>

<style>
  .fit {
    position: absolute;
    inset: 0;
    overflow: hidden;
  }
  .scroller {
    position: absolute;
    inset: 0;
    overflow: hidden;
  }
  .scroller.scroll {
    overflow-y: auto;
    overscroll-behavior: contain;
    /* A scrollbar that comes and goes would narrow the terminal, which may then fit and
       drop the scrollbar again. */
    scrollbar-gutter: stable;
  }
  /* The control bar stays at the bottom of the box while the terminal scrolls: it sits in
     the flow under the terminal and sticks to the scroller (`clip` instead of the player's
     `hidden` keeps the player from becoming the bar's scroll container). */
  .scroll :global(.ap-player) {
    overflow: clip;
  }
  .scroll :global(.ap-control-bar) {
    position: sticky;
  }
  .mount {
    min-width: 0;
    margin-inline: auto;
  }
  .mount :global(.ap-player) {
    border-radius: var(--radius);
  }
</style>
