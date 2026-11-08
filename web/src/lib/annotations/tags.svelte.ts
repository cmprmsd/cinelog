// The known tags (`GET /api/tags`) for views outside a session: chip colours on search
// results, the search tag filter and the annotations overview. Loaded on first use and kept
// current through the UI bus (`tag` upserts one, a fresh `hello` reloads).

import { api } from '../api/client';
import { bus } from '../api/events.svelte';
import type { Tag } from '../api/types';
import { tagColor, upsertTag } from './annotations';

class KnownTags {
  list = $state<Tag[]>([]);
  colors = $derived(new Map(this.list.map((t) => [t.name, t.color])));
  private loading: Promise<void> | null = null;
  private subscribed = false;

  /** Loads the list unless it is loaded or loading; `force` reloads. */
  load(force = false): Promise<void> {
    if (!this.subscribed) {
      this.subscribed = true;
      bus.subscribe((e) => {
        if (e.type === 'tag') this.list = upsertTag(this.list, e.data);
        else if (e.type === 'hello' || e.type === 'forgotten') void this.load(true);
      });
    }
    if (this.loading && !force) return this.loading;
    const p = api.tags().then(
      (r) => {
        if (this.loading === p) this.list = r.tags;
      },
      () => {
        // Colours and suggestions only; the next `hello` or a forced load retries.
        if (this.loading === p) this.loading = null;
      },
    );
    this.loading = p;
    return p;
  }

  /** The tag's colour (its own, or one derived from the name). */
  color(name: string): string {
    return tagColor(name, this.colors.get(name));
  }
}

export const knownTags = new KnownTags();
