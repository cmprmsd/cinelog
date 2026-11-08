<script lang="ts">
  import { untrack } from 'svelte';
  import { api, errorText, isAbort } from '../api/client';
  import type { PlayInfo } from '../api/types';
  import { createPlayer, onPlayerError, type Player } from '../player';

  interface Props {
    uuid: string;
    /** A seek request; a new object (or nonce) seeks again. */
    seek: { t: number; nonce: number };
    theme: string;
    keystrokeOverlay: boolean;
    currentTime?: number;
    duration?: number | null;
    playing?: boolean;
    oninput?: (data: string) => void;
  }

  let {
    uuid,
    seek,
    theme,
    keystrokeOverlay,
    currentTime = $bindable(0),
    duration = $bindable(null),
    playing = $bindable(false),
    oninput,
  }: Props = $props();

  const POLL_MS = 500;
  const TICK_MS = 200;

  let el: HTMLDivElement | undefined = $state();
  let info = $state<PlayInfo | null>(null);
  let error = $state<string | null>(null);
  /** What the player itself reports (unlike `currentTime`, never set from a seek request). */
  let reported = $state(0);
  let player: Player | null = null;
  let pendingStart: number | null = null;
  /** Seeks in flight: until they settle the player still reports the old position. */
  let seeking = 0;

  function seekPlayer(p: Player, t: number): void {
    seeking += 1;
    currentTime = t;
    void p
      .seek(t)
      .catch(() => {})
      .finally(() => {
        if (player === p) seeking -= 1;
      });
  }

  // Playback info; large casts without segments are built on demand, so poll until ready.
  $effect(() => {
    const id = uuid;
    const ctl = new AbortController();
    let timer: ReturnType<typeof setTimeout> | undefined;
    info = null;
    error = null;
    const load = async () => {
      try {
        const i = await api.play(id, ctl.signal);
        info = i;
        if (!i.ready) timer = setTimeout(load, POLL_MS);
      } catch (e) {
        if (!isAbort(e)) error = errorText(e);
      }
    };
    void load();
    return () => {
      ctl.abort();
      clearTimeout(timer);
    };
  });

  // (Re)creates the player: new session, theme or overlay. Keeps the position and play state.
  $effect(() => {
    if (!info?.ready || !el) return;
    const playInfo = info;
    const target = el;
    const opts = { theme, keystrokeOverlay };
    const { startAt, resume } = untrack(() => {
      const s = pendingStart ?? currentTime;
      pendingStart = null;
      return { startAt: s, resume: playing };
    });
    const p = createPlayer(playInfo, target, { ...opts, startAt });
    player = p;
    seeking = 0;
    error = null;
    onPlayerError(p, (m) => (error = m));
    p.addEventListener('play', () => (playing = true));
    p.addEventListener('playing', () => (playing = true));
    p.addEventListener('pause', () => (playing = false));
    p.addEventListener('ended', () => (playing = false));
    p.addEventListener('input', (e) => oninput?.(e.data));
    // `startAt` alone only applies once playback starts; seeking shows the frame right away.
    if (startAt > 0) seekPlayer(p, startAt);
    else currentTime = 0;
    if (resume) void p.play().catch(() => {});
    reported = 0;
    const tick = setInterval(() => {
      const t = p.getCurrentTime();
      if (!Number.isFinite(t)) return;
      if (Math.abs(t - reported) > 0.01) reported = t;
      if (seeking > 0) return;
      if (Math.abs(t - currentTime) > 0.01) currentTime = t;
      const d = p.getDuration();
      if (d !== undefined && d !== duration) duration = d;
    }, TICK_MS);
    return () => {
      clearInterval(tick);
      p.dispose();
      if (player === p) player = null;
      target.replaceChildren();
    };
  });

  // Seek requests from results, chapters and hit navigation.
  $effect(() => {
    const t = seek.t;
    void seek.nonce;
    untrack(() => {
      if (player) {
        seekPlayer(player, t);
      } else {
        pendingStart = t;
      }
    });
  });

  export function toggle(): void {
    if (!player) return;
    void (playing ? player.pause() : player.play()).catch(() => {});
  }

  export function step(delta: number): void {
    if (!player) return;
    const end = player.getDuration() ?? Infinity;
    seekPlayer(player, Math.min(end, Math.max(0, currentTime + delta)));
  }

  export function focus(): void {
    el?.querySelector<HTMLElement>('.ap-wrapper')?.focus();
  }
</script>

<div
  class="player"
  data-testid="player"
  data-mode={info?.mode ?? ''}
  data-ready={info?.ready && !error ? 'true' : 'false'}
  data-current-time={currentTime.toFixed(2)}
  data-player-time={reported.toFixed(2)}
>
  {#if error}
    <p class="banner warn" role="alert">Playback failed: {error}</p>
  {:else if info && !info.ready}
    <div class="building" role="status">
      <span>Preparing playback of a large recording…</span>
      <progress max="1" value={info.progress ?? undefined}></progress>
      {#if info.progress !== null}<span class="muted">{Math.round(info.progress * 100)}%</span>{/if}
    </div>
  {:else if !info}
    <p class="muted" role="status">Loading recording…</p>
  {/if}
  <div class="mount" bind:this={el}></div>
</div>

<style>
  .player {
    display: grid;
    grid-template-columns: minmax(0, 1fr);
    gap: 0.4rem;
    min-width: 0;
  }
  .mount {
    min-width: 0;
  }
  .mount :global(.ap-player) {
    border-radius: var(--radius);
  }
  .building {
    display: flex;
    gap: 0.6rem;
    align-items: center;
    padding: 0.6rem;
  }
  progress {
    flex: 1;
  }
</style>
