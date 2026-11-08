<script lang="ts">
  import { api, ApiFailure, errorText, isAbort } from '../api/client';
  import AnnotationsPanel from '../annotations/AnnotationsPanel.svelte';
  import type { BusEvent, Command, SessionDetail } from '../api/types';
  import { bus } from '../api/events.svelte';
  import { chapterAt, nextChapter, prevChapter, stepHit } from '../chapters';
  import ExportMenu from '../export/ExportMenu.svelte';
  import { toSearchParams, type SearchState } from '../filters';
  import { fmtDuration, fmtTs, keyLabel, oneLine, tzLabel } from '../format';
  import { useKeys } from '../keymap';
  import { markerBands } from '../markers';
  import { collectHitTimes } from '../results';
  import { playerTheme, prefs } from '../prefs.svelte';
  import { liveHref, sessionHref } from '../route';
  import { router } from '../router.svelte';
  import type { AnnotationSpan, TimeRange } from '../spans';
  import CommandStrip from './CommandStrip.svelte';
  import Player from './Player.svelte';
  import Rail from './Rail.svelte';

  interface Props {
    uuid: string;
    /** Where to start: a time, or a command (its start). A new object seeks again. */
    target: { t?: number | null; cmd?: number | null };
    search: SearchState;
    onclose?: () => void;
    /** A command of this session was forgotten (lets the result list drop its hits). */
    onforget?: (session: string, seq: number) => void;
  }
  let { uuid, target, search, onclose, onforget }: Props = $props();

  const HIT_DEBOUNCE_MS = 200;
  const HIT_PAGE = 500;
  /** At most this many pages of session hits (10 000) feed `n`/`N` and the rail. */
  const HIT_PAGES = 20;

  let detail = $state<SessionDetail | null>(null);
  let commands = $state<Command[]>([]);
  let loadError = $state<string | null>(null);
  let hitTimes = $state<number[]>([]);
  let hitsTruncated = $state(false);
  let currentTime = $state(0);
  let duration = $state<number | null>(null);
  let playing = $state(false);
  let keys = $state<string[]>([]);
  let seek = $state({ t: 0, nonce: 0 });
  let notice = $state<string | null>(null);
  let exportMenu: ExportMenu | undefined = $state();
  let annotations: AnnotationsPanel | undefined = $state();
  let annotationSpans = $state<AnnotationSpan[]>([]);
  let selection = $state<TimeRange | null>(null);
  let player: Player | undefined = $state();
  let reload = $state(0);

  const s = $derived(detail?.session ?? null);
  const total = $derived(duration ?? s?.duration ?? commands.at(-1)?.t_end ?? 0);
  const current = $derived(chapterAt(commands, currentTime));
  const currentCmd = $derived(current >= 0 ? commands[current] : undefined);
  const focusSeq = $derived(target.cmd ?? null);
  const marks = $derived(markerBands(detail?.markers ?? [], detail?.masked ?? [], total));

  $effect(() => {
    const id = uuid;
    void reload;
    const ctl = new AbortController();
    loadError = null;
    Promise.all([api.session(id, ctl.signal), api.commands(id, ctl.signal)]).then(
      ([d, c]) => {
        detail = d;
        commands = c.commands;
      },
      (e) => {
        if (isAbort(e)) return;
        detail = null;
        commands = [];
        loadError =
          e instanceof ApiFailure && e.status === 404 ? 'This session does not exist or was forgotten.' : errorText(e);
      },
    );
    return () => ctl.abort();
  });

  // Something of this session was forgotten elsewhere (another browser, the CLI): reload.
  // Our own forget reloads by itself, so its event is skipped once.
  let ownForget: string | null = null;
  const forgetKey = (seq: number) => `${uuid}:${seq}`;
  $effect(() =>
    bus.subscribe((e: BusEvent) => {
      if (e.type !== 'forgotten' || e.data.target.uuid !== uuid) return;
      const t = e.data.target;
      if (t.kind === 'command' && ownForget === forgetKey(t.seq)) {
        ownForget = null;
        return;
      }
      seekTo(currentTime);
      reload += 1;
    }),
  );

  // Seek target: a time wins; a command resolves once the chapters are known. Each target
  // object seeks once (a reload after forget does not jump back).
  let seekedTarget: Props['target'] | null = null;
  let nonce = 0;
  $effect(() => {
    const tg = target;
    if (tg === seekedTarget) return;
    if (tg.t !== null && tg.t !== undefined) {
      seekedTarget = tg;
      seekTo(tg.t);
    } else if (tg.cmd !== null && tg.cmd !== undefined) {
      const c = commands.find((x) => x.seq === tg.cmd);
      if (c) {
        seekedTarget = tg;
        seekTo(c.t_start);
      }
    } else {
      seekedTarget = tg;
    }
  });

  // Hits of the current query inside this session, for `n`/`N` and the rail. Every cursor
  // page is followed: the API is newest-first, so one page would lose the oldest hits.
  const hitQuery = $derived(search.q ? JSON.stringify(search) : '');
  let firstHitQuery = true;
  $effect(() => {
    const id = uuid;
    const key = hitQuery;
    void reload;
    if (!key) {
      hitTimes = [];
      hitsTruncated = false;
      return;
    }
    const ctl = new AbortController();
    const state = JSON.parse(key) as SearchState;
    const run = () => {
      const now = new Date();
      collectHitTimes(
        (cursor) => api.search(toSearchParams(state, now, { session: id, limit: HIT_PAGE, cursor }), ctl.signal),
        HIT_PAGES,
      ).then(
        (r) => {
          hitTimes = r.times;
          hitsTruncated = r.truncated;
        },
        () => {},
      );
    };
    const timer = setTimeout(run, firstHitQuery ? 0 : HIT_DEBOUNCE_MS);
    firstHitQuery = false;
    return () => {
      clearTimeout(timer);
      ctl.abort();
    };
  });

  function seekTo(t: number): void {
    nonce += 1;
    seek = { t, nonce };
  }

  function flash(text: string): void {
    notice = text;
    setTimeout(() => {
      if (notice === text) notice = null;
    }, 2500);
  }

  $effect(() =>
    useKeys({
      'next-chapter': () => {
        const c = nextChapter(commands, currentTime);
        if (c) seekTo(c.t_start);
      },
      'prev-chapter': () => {
        const c = prevChapter(commands, currentTime);
        if (c) seekTo(c.t_start);
      },
      'next-hit': () => {
        const t = stepHit(hitTimes, currentTime, 1);
        if (t !== undefined) seekTo(t);
        else if (hitTimes.length) flash('No further hits in this session.');
      },
      'prev-hit': () => {
        const t = stepHit(hitTimes, currentTime, -1);
        if (t !== undefined) seekTo(t);
        else if (hitTimes.length) flash('No earlier hits in this session.');
      },
      'toggle-play': () => player?.toggle(),
      'back-5': () => player?.step(-5),
      'forward-5': () => player?.step(5),
      export: () => exportMenu?.open() ?? false,
      annotate: () => annotations?.annotate() ?? false,
      escape: () => {
        if (exportMenu?.close()) return;
        if (selection) {
          selection = null;
          return;
        }
        if (!onclose) return false;
        onclose();
      },
    }),
  );

  const linkFor = (c: Command | undefined) => location.origin + sessionHref(uuid, c ? { cmd: c.seq } : {});

  async function copyLink(): Promise<void> {
    const url = linkFor(currentCmd);
    try {
      await navigator.clipboard.writeText(url);
      flash('Link copied.');
    } catch {
      window.prompt('Copy this link:', url);
    }
  }

  async function forgetCurrent(): Promise<void> {
    const c = currentCmd;
    if (!c) return;
    const ok = window.confirm(
      `Forget "$ ${oneLine(c.typed).slice(0, 80)}"?\n\nIt is removed from search, and playback and export mask its output. The cast file on disk is not changed.`,
    );
    if (!ok) return;
    ownForget = forgetKey(c.seq);
    try {
      await api.forget({ session_uuid: uuid, cmd_seq: c.seq });
      flash('Forgotten.');
      // Reloads the chapters, markers and hits, and remounts the player so the masked
      // recording replaces the one already loaded; the new player resumes at this position.
      seekTo(currentTime);
      reload += 1;
      onforget?.(uuid, c.seq);
    } catch (e) {
      ownForget = null;
      flash(`Forget failed: ${errorText(e)}`);
    }
  }

  function onKey(data: string): void {
    keys = [...keys, keyLabel(data)].slice(-40);
  }

  const location_ = (d: SessionDetail) => {
    const t = d.session.tmux;
    if (t.pane) return `tmux ${t.session ?? t.session_id ?? '?'}:${t.window ?? '?'}${t.window_name ? ` (${t.window_name})` : ''} ${t.pane}`;
    return d.session.holder_comm ?? d.session.tty ?? '';
  };
</script>

<section class="session" aria-label="Session" data-testid="session-view" data-uuid={uuid}>
  {#if loadError}
    <div class="banner warn" role="alert">{loadError}</div>
    {#if onclose}<button type="button" onclick={onclose}>Close</button>{/if}
  {:else if !detail || !s}
    <p class="muted" role="status">Loading session…</p>
  {:else}
    <header class="head">
      <div class="title">
        <h2>
          {#if s.state === 'live'}<span class="live-dot" aria-label="live"></span>{/if}
          <span>{s.user}@{s.host}</span>
          {#if s.sudo_user}<span class="muted">(sudo by {s.sudo_user})</span>{/if}
        </h2>
        <div class="meta muted">
          <span>{fmtTs(s.started_at, prefs.utc)} {tzLabel(prefs.utc)}</span>
          <span>{fmtDuration(s.duration)}</span>
          <span>{location_(detail)}</span>
          <span>{s.scope}</span>
          {#if s.state !== 'ended'}<span class="badge">{s.state}</span>{/if}
          {#if s.legacy}<span class="badge legacy">legacy</span>{/if}
          {#if detail.play_mode === 'segmented'}<span class="badge" title="large cast, loaded in segments">segmented</span>{/if}
        </div>
        {#if detail.parent || detail.children.length}
          <div class="links">
            {#if detail.parent}
              <a href={`/s/${detail.parent.uuid}`} onclick={router.link} data-testid="parent-link"
                >↖ back to parent ({detail.parent.user})</a
              >
            {/if}
            {#each detail.children as c (c.uuid)}
              <a href={`/s/${c.uuid}`} onclick={router.link}>↘ nested: {c.user} {fmtTs(c.started_at, prefs.utc).slice(11, 19)}</a>
            {/each}
          </div>
        {/if}
      </div>
      <div class="actions">
        {#if s.state === 'live' && router.path !== liveHref(uuid)}
          <a class="btn" href={liveHref(uuid)} onclick={router.link} data-testid="watch-live">Watch live</a>
        {/if}
        <button type="button" onclick={copyLink} title="Copy a link to the current command">Copy link</button>
        <ExportMenu bind:this={exportMenu} {uuid} command={currentCmd ?? null} {selection} onnotice={flash} />
        <button type="button" onclick={forgetCurrent} disabled={!currentCmd} title="Remove the current command from search and playback"
          >Forget…</button
        >
        <button
          type="button"
          aria-pressed={prefs.keystrokeOverlay}
          onclick={() => prefs.setKeystrokeOverlay(!prefs.keystrokeOverlay)}
          title="Show keystrokes over the terminal">Keys overlay</button
        >
        {#if onclose}<button type="button" onclick={onclose} aria-label="Close player">✕</button>{/if}
      </div>
    </header>

    {#key `${uuid}:${reload}`}
      <Player
        bind:this={player}
        {uuid}
        {seek}
        theme={playerTheme(prefs.resolved)}
        keystrokeOverlay={prefs.keystrokeOverlay}
        bind:currentTime
        bind:duration
        bind:playing
        oninput={onKey}
      />
    {/key}

    <Rail
      duration={total}
      {commands}
      bands={marks.bands}
      nested={marks.nested}
      hits={hitTimes}
      {currentTime}
      onseek={seekTo}
      bind:selection
      annotations={annotationSpans}
    />
    <CommandStrip {commands} {current} {focusSeq} onseek={seekTo} />

    <div class="foot">
      {#if currentCmd}
        <div class="cmdinfo" data-testid="current-command">
          <code>$ {oneLine(currentCmd.typed)}</code>
          <span class={`exit ${currentCmd.exit === null ? 'unknown' : currentCmd.exit === 0 ? 'ok' : 'fail'}`}>{currentCmd.exit ?? '?'}</span>
          {#if currentCmd.cwd}<span class="muted mono">{currentCmd.cwd}</span>{/if}
          {#if currentCmd.approx}<span class="badge approx">approx</span>{/if}
        </div>
      {/if}
      {#if hitTimes.length}
        <span class="muted" data-testid="hit-count"
          >{hitTimes.length} hit{hitTimes.length === 1 ? '' : 's'} here{hitsTruncated ? ' (more hits than shown)' : ''} ·
          <kbd>n</kbd>/<kbd>N</kbd></span
        >
      {/if}
      {#if keys.length}
        <div class="keys" aria-label="Recent keystrokes" data-testid="keystrokes">
          {#each keys as k, i (i)}<kbd>{k}</kbd>{/each}
        </div>
      {/if}
    </div>
    {#if notice}<p class="notice" role="status">{notice}</p>{/if}
    <AnnotationsPanel
      bind:this={annotations}
      bind:spans={annotationSpans}
      session={uuid}
      {commands}
      current={currentCmd ?? null}
      {currentTime}
      {selection}
      onseek={seekTo}
    />
  {/if}
</section>

<style>
  .session {
    display: grid;
    /* One column as wide as the panel: the player scales to it instead of widening it. */
    grid-template-columns: minmax(0, 1fr);
    gap: 0.5rem;
    align-content: start;
    min-width: 0;
    padding: 0.6rem;
  }
  .head {
    /* Above the player's overlays, so the export menu stays clickable. */
    position: relative;
    z-index: 20;
    display: flex;
    flex-wrap: wrap;
    gap: 0.4rem 1rem;
    justify-content: space-between;
  }
  h2 {
    display: flex;
    gap: 0.4rem;
    align-items: center;
    margin: 0;
    font-size: 1.05rem;
  }
  .meta {
    display: flex;
    flex-wrap: wrap;
    gap: 0.1rem 0.8rem;
    font-size: 0.88em;
  }
  .links {
    display: flex;
    flex-wrap: wrap;
    gap: 0.2rem 0.8rem;
    font-size: 0.88em;
  }
  .actions {
    display: flex;
    flex-wrap: wrap;
    gap: 0.3rem;
    align-items: flex-start;
  }
  .foot {
    display: flex;
    flex-wrap: wrap;
    gap: 0.4rem 1rem;
    align-items: center;
    font-size: 0.9em;
  }
  .cmdinfo {
    display: flex;
    gap: 0.5rem;
    align-items: center;
    min-width: 0;
  }
  .cmdinfo code {
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .keys {
    display: flex;
    gap: 0.15rem;
    overflow: hidden;
    max-width: 100%;
  }
  .notice {
    margin: 0;
    color: var(--fg-muted);
  }
</style>
