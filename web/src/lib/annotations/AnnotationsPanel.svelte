<script lang="ts">
  // Annotations of one session (work package M4-C; plan §8.4; docs/API.md §9.4, §9.6).
  // Stand-in until M4-C lands: shows nothing. `SessionView` mounts it with exactly these
  // props and calls `annotate()` for the `a` key.
  import type { Command } from '../api/types';
  import type { AnnotationSpan, TimeRange } from '../spans';

  interface Props {
    session: string;
    commands: Command[];
    /** The command at the playhead. */
    current: Command | null;
    currentTime: number;
    /** The rail selection, if any. */
    selection: TimeRange | null;
    onseek: (t: number) => void;
    /** Out: what the rail draws. */
    spans?: AnnotationSpan[];
  }
  let { spans = $bindable([]) }: Props = $props();

  /** Starts an annotation of the selection, else the current command. False: nothing to annotate. */
  export function annotate(): boolean {
    return false;
  }

</script>
