<script lang="ts">
  import { SHORTCUTS } from '../keys';

  let dialog: HTMLDialogElement | undefined = $state();

  export function toggle(): void {
    if (!dialog) return;
    if (dialog.open) dialog.close();
    else dialog.showModal();
  }

  export function close(): boolean {
    if (!dialog?.open) return false;
    dialog.close();
    return true;
  }
</script>

<dialog bind:this={dialog} aria-labelledby="shortcuts-title">
  <h2 id="shortcuts-title">Keyboard shortcuts</h2>
  <table>
    <tbody>
      {#each SHORTCUTS as s (s.what)}
        <tr>
          <td>{#each s.keys as k, i (k)}{#if i > 0}<span class="muted"> / </span>{/if}<kbd>{k}</kbd>{/each}</td>
          <td>{s.what}</td>
        </tr>
      {/each}
    </tbody>
  </table>
  <p class="muted">Inside the player, its own keys apply (Space, arrows, <kbd>f</kbd> for full screen).</p>
  <form method="dialog"><button class="primary">Close</button></form>
</dialog>

<style>
  h2 {
    margin: 0 0 0.6rem;
    font-size: 1.1rem;
  }
  table {
    border-collapse: collapse;
  }
  td {
    padding: 0.2rem 0.8rem 0.2rem 0;
    vertical-align: top;
  }
  td:first-child {
    white-space: nowrap;
  }
</style>
