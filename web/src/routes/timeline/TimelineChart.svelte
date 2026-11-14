<script lang="ts">
  // The team timeline chart (plan §8.3): lanes of rows with command blocks coloured by exit,
  // "not recorded here" marks, annotation spans, density bars when aggregated, collapsed
  // idle breaks. Pointer: hover shows the command, a click opens it, a drag zooms (brush).
  // Keyboard (chart focused): arrows move between commands, Enter opens.
  import type { Scope, Timeline, TimelineBlock } from '../../lib/api/types';
  import { fmtDuration, fmtTs, oneLine } from '../../lib/format';
  import {
    annotationColor,
    activityRuns,
    annotationGeoms,
    attachedGroupKey,
    blockAt,
    blockInfo,
    blockRuns,
    blockXs,
    densityBars,
    firstVisible,
    focusRows,
    GROUP_H,
    groupId,
    hitAnnotation,
    hitBlock,
    hitMark,
    layout,
    LANE_H,
    locateBlock,
    markGeoms,
    markLink,
    maxDensity,
    nearestBlock,
    ROW_H,
    rowAt,
    sessionMap,
    sessionSpans,
    type BlockAt,
    type BlockRef,
    type DensityBar,
    type GroupGeom,
    type MarkGeom,
    type AnnotationGeom,
  } from './layout';
  import { TimeScale, ticks } from './scale';

  interface Props {
    timeline: Timeline;
    utc: boolean;
    /** Collapse idle spans to breaks. */
    collapse: boolean;
    now: number;
    /** The opened command. */
    selected: { session: string; seq: number } | null;
    /** `performance.now()` when `timeline` arrived (render time is reported from it). */
    receivedAt: number;
    onopen: (b: TimelineBlock) => void;
    onopensession: (uuid: string) => void;
    onzoom: (from: number, to: number) => void;
    /** This server's scope: a nested child of the other scope opens on its own server. */
    scope?: Scope | null;
  }
  let { timeline, utc, collapse, now, selected, receivedAt, onopen, onopensession, onzoom, scope = null }: Props = $props();

  const AXIS_H = 30;
  const BRUSH_MIN_PX = 5;

  let width = $state(800);
  let plot: HTMLDivElement | undefined = $state();
  let renderMs = $state<number | null>(null);

  /** Collapsed row groups ([`GroupGeom.id`]); kept across refreshes of `timeline`. */
  let collapsed = $state<ReadonlySet<string>>(new Set());
  /** The group a click on a tmux pause pointed to (highlighted briefly). */
  let flash = $state<string | null>(null);
  let flashTimer: ReturnType<typeof setTimeout> | undefined;

  const lay = $derived(layout(timeline, collapsed));
  const sessions = $derived(sessionMap(timeline));
  const scale = $derived(new TimeScale(timeline.from, timeline.to, width, timeline.idle, collapse));
  const axis = $derived(ticks(scale, utc));
  const xs = $derived(lay.rows.map((r) => blockXs(r.row, scale, sessions, now)));
  const runs = $derived(lay.rows.map((r, i) => blockRuns(r.row, xs[i]!, scale.width)));
  const marks = $derived(lay.rows.map((r) => markGeoms(r.row, scale, sessions, now)));
  const annos = $derived(annotationGeoms(timeline.annotations, lay, scale));
  const peak = $derived(maxDensity(timeline));
  const bars = $derived(
    timeline.aggregated && timeline.bin_secs
      ? lay.rows.map((r) => densityBars(r.row.density, timeline.bin_secs!, scale, peak))
      : lay.rows.map(() => [] as DensityBar[]),
  );
  /** The row each person was typing in, per activity bin. */
  const focus = $derived(focusRows(timeline));
  // The strip shares the bottom of a row with the density bars, so it is left out when aggregated.
  const activity = $derived(
    lay.rows.map((r) => (timeline.aggregated ? [] : activityRuns(r.row, focus.get(r.row.user), timeline.activity_bin_secs, scale))),
  );
  const nowX = $derived(now >= timeline.from && now <= timeline.to ? scale.x(now) : null);
  const blockCount = $derived(lay.rows.reduce((n, r) => n + r.row.blocks.length, 0));

  /** Where the opened command is drawn. */
  const selectedAt = $derived(locateBlock(lay, selected));

  // ---------------------------------------------------------------- render time

  $effect(() => {
    void runs;
    void bars;
    renderMs = Math.round(performance.now() - receivedAt);
  });

  // ---------------------------------------------------------------- hover
  //
  // `timeline` is replaced on refreshes (follow mode, bus events), so nothing here keeps
  // indices into a response: the hover is derived from the pointer position against the
  // current layout, and the keyboard cursor is the command's identity.

  type Hover =
    | ({ kind: 'block' } & BlockAt)
    | { kind: 'mark'; row: number; mark: MarkGeom }
    | { kind: 'annotation'; anno: AnnotationGeom }
    | { kind: 'density'; row: number; bar: DensityBar };

  /** The pointer over the plot (plot coordinates). */
  let pointer = $state<{ x: number; y: number } | null>(null);
  /** The tooltip follows the keyboard cursor instead of the pointer. */
  let keyTip = $state(false);
  /** Keyboard cursor. */
  let cursor = $state<BlockRef | null>(null);
  let brush = $state<{ x0: number; x1: number } | null>(null);
  let down: { x: number; y: number; id: number } | null = null;

  const cursorAt = $derived(locateBlock(lay, cursor));
  const hover = $derived.by((): Hover | null => {
    if (keyTip) return cursorAt ? { kind: 'block', ...cursorAt } : null;
    return pointer && !brush ? hitAt(pointer.x, pointer.y) : null;
  });
  const tip = $derived.by(() => {
    if (keyTip && cursorAt) {
      const x0 = xs[cursorAt.row]!.x0[cursorAt.i]!;
      return { x: Math.max(0, Math.min(scale.width, x0)), y: lay.rows[cursorAt.row]!.y + ROW_H };
    }
    return pointer ?? { x: 0, y: 0 };
  });

  function toggleGroup(g: GroupGeom): void {
    const others = [...collapsed].filter((id) => id !== g.id);
    collapsed = new Set(g.collapsed ? others : [...others, g.id]);
  }

  /** The group with the panes of the tmux session a paused shell attached to, in its lane. */
  function attachedGroup(row: number, m: MarkGeom): GroupGeom | null {
    const r = lay.rows[row];
    const key = r ? attachedGroupKey(r.row.host, m.mark) : null;
    if (!r || !key) return null;
    const id = groupId(lay.lanes[r.lane]!.key, key);
    return lay.groups.find((g) => g.id === id) ?? lay.groups.find((g) => g.group.key === key) ?? null;
  }

  function showGroup(g: GroupGeom): void {
    if (g.collapsed) toggleGroup(g);
    flash = g.id;
    clearTimeout(flashTimer);
    flashTimer = setTimeout(() => (flash = null), 1600);
    const el = plot?.closest('.tl-scroll');
    if (el instanceof HTMLElement) {
      const top = g.y + AXIS_H;
      if (top < el.scrollTop + AXIS_H || top + GROUP_H > el.scrollTop + el.clientHeight) el.scrollTop = Math.max(0, top - AXIS_H - LANE_H);
    }
  }

  $effect(() => () => clearTimeout(flashTimer));

  function refAt(at: BlockAt): BlockRef | null {
    const b = blockAt(lay, at);
    return b ? { session: b.session, seq: b.seq } : null;
  }

  function local(e: PointerEvent): { x: number; y: number } {
    const box = plot!.getBoundingClientRect();
    return { x: e.clientX - box.left, y: e.clientY - box.top };
  }

  function hitAt(x: number, y: number): Hover | null {
    const row = rowAt(lay, y);
    if (row < 0) return null;
    const m = hitMark(marks[row]!, x);
    if (m?.point) return { kind: 'mark', row, mark: m };
    if (timeline.aggregated) {
      const bar = bars[row]!.find((b) => x >= b.x && x <= b.x + b.w + 1);
      if (bar) return { kind: 'density', row, bar };
    } else {
      const i = hitBlock(lay.rows[row]!.row, xs[row]!, x);
      if (i >= 0) return { kind: 'block', row, i };
    }
    const a = hitAnnotation(annos, row, x);
    if (a) return { kind: 'annotation', anno: a };
    return m ? { kind: 'mark', row, mark: m } : null;
  }

  function onpointermove(e: PointerEvent): void {
    const p = local(e);
    keyTip = false;
    pointer = p;
    if (down && down.id === e.pointerId && Math.abs(p.x - down.x) >= BRUSH_MIN_PX) {
      brush = { x0: Math.max(0, Math.min(down.x, p.x)), x1: Math.min(scale.width, Math.max(down.x, p.x)) };
    }
  }

  function onpointerdown(e: PointerEvent): void {
    if (e.button !== 0) return;
    const p = local(e);
    down = { ...p, id: e.pointerId };
    plot?.setPointerCapture?.(e.pointerId);
  }

  function onpointerup(e: PointerEvent): void {
    if (!down || down.id !== e.pointerId) return;
    const p = local(e);
    down = null;
    plot?.releasePointerCapture?.(e.pointerId);
    if (brush) {
      const b = brush;
      brush = null;
      if (b.x1 - b.x0 >= BRUSH_MIN_PX) onzoom(scale.ts(b.x0), scale.ts(b.x1));
      return;
    }
    activate(hitAt(p.x, p.y));
  }

  function activate(h: Hover | null): void {
    if (!h) return;
    if (h.kind === 'block') {
      const b = blockAt(lay, h);
      if (!b) return;
      cursor = { session: b.session, seq: b.seq };
      onopen(b);
    } else if (h.kind === 'mark' && h.mark.mark.kind === 'nested') {
      const link = markLink(h.mark.mark, scope, location.hostname);
      // Served by the other scope's server (a TEAM `sudo -i`): a new tab, its own sign-in.
      if (link.external) window.open(link.external, '_blank', 'noopener,noreferrer');
      else if (link.child) onopensession(link.child);
    } else if (h.kind === 'mark' && h.mark.mark.tmux) {
      const g = attachedGroup(h.row, h.mark);
      if (g) showGroup(g);
    } else if (h.kind === 'density' && timeline.bin_secs) {
      const mid = h.bar.bin.ts + timeline.bin_secs / 2;
      const span = Math.max(600, timeline.bin_secs * 12);
      onzoom(mid - span / 2, mid + span / 2);
    }
  }

  function onpointerleave(): void {
    if (down) return;
    pointer = null;
    keyTip = false;
  }

  // ---------------------------------------------------------------- keyboard

  function rowsWithBlocks(): number[] {
    return lay.rows.filter((r) => r.row.blocks.length > 0).map((r) => r.index);
  }

  const NAV_KEYS = new Set(['ArrowRight', 'ArrowLeft', 'ArrowUp', 'ArrowDown', 'Home', 'End']);

  function moveCursor(key: string): boolean {
    if (!NAV_KEYS.has(key)) return false;
    const withBlocks = rowsWithBlocks();
    if (withBlocks.length === 0) return false;
    const to = (at: BlockAt): true => {
      cursor = refAt(at);
      return true;
    };
    // No cursor yet, or its command left the response: start at the first visible one.
    if (!cursorAt) {
      const row = withBlocks[0]!;
      return to({ row, i: Math.max(0, firstVisible(xs[row]!, scale.width)) });
    }
    const { row, i } = cursorAt;
    const n = lay.rows[row]!.row.blocks.length;
    const at = withBlocks.indexOf(row);
    const x = (xs[row]!.x0[i]! + xs[row]!.x1[i]!) / 2;
    switch (key) {
      case 'ArrowRight':
        return to({ row, i: Math.min(n - 1, i + 1) });
      case 'ArrowLeft':
        return to({ row, i: Math.max(0, i - 1) });
      case 'Home':
        return to({ row, i: 0 });
      case 'End':
        return to({ row, i: n - 1 });
      case 'ArrowDown':
      case 'ArrowUp': {
        const next = withBlocks[at + (key === 'ArrowDown' ? 1 : -1)];
        if (next === undefined) return true;
        return to({ row: next, i: Math.max(0, nearestBlock(xs[next]!, x)) });
      }
    }
    return false;
  }

  function onkeydown(e: KeyboardEvent): void {
    if (e.ctrlKey || e.metaKey || e.altKey) return;
    if (e.key === 'Enter') {
      if (cursorAt) activate({ kind: 'block', ...cursorAt });
      e.preventDefault();
      return;
    }
    if (moveCursor(e.key)) {
      e.preventDefault();
      keyTip = true;
      if (cursorAt) scrollToRow(cursorAt.row);
    }
  }

  function scrollToRow(row: number): void {
    const el = plot?.closest('.tl-scroll');
    const r = lay.rows[row];
    if (!(el instanceof HTMLElement) || !r) return;
    const top = r.y + AXIS_H;
    if (top < el.scrollTop + AXIS_H) el.scrollTop = top - AXIS_H - LANE_H;
    else if (top + ROW_H > el.scrollTop + el.clientHeight) el.scrollTop = top + ROW_H - el.clientHeight;
  }

  const cursorBlock = $derived(blockAt(lay, cursorAt));
  const announce = $derived(
    cursorBlock && cursorAt
      ? `${oneLine(cursorBlock.typed)}, ${fmtTs(cursorBlock.ts_start, utc)}, ${
          cursorBlock.exit === null ? 'exit unknown' : `exit ${cursorBlock.exit}`
        }, ${lay.rows[cursorAt.row]!.row.label}`
      : '',
  );

  const tipBlock = $derived.by(() => {
    if (hover?.kind !== 'block') return null;
    const b = blockAt(lay, hover);
    return b ? { b, row: lay.rows[hover.row]!.row } : null;
  });

  function spanText(from: number, to: number | null): string {
    return to === null ? `${fmtTs(from, utc)} – (open)` : `${fmtTs(from, utc)} – ${fmtTs(to, utc)} (${fmtDuration(to - from)})`;
  }

  const highlight = (at: BlockAt | null) => {
    if (!at) return null;
    const r = lay.rows[at.row];
    const geo = xs[at.row];
    if (!r || !geo || at.i >= geo.x0.length) return null;
    const x0 = Math.max(-2, geo.x0[at.i]!);
    const x1 = Math.min(scale.width + 2, geo.x1[at.i]!);
    return { x: x0 - 2, w: Math.max(4, x1 - x0 + 4), y: r.y + 1, h: ROW_H - 2 };
  };
  const cursorBox = $derived(highlight(cursorAt));
  const selectedBox = $derived(highlight(selectedAt));
</script>

<div
  class="tl-scroll"
  data-testid="timeline-chart"
  data-render-ms={renderMs}
  data-blocks={blockCount}
  data-aggregated={timeline.aggregated}
  data-from={timeline.from}
  data-to={timeline.to}
>
  <div class="grid">
    <div class="corner"></div>
    <svg class="axis" width={scale.width} height={AXIS_H} aria-hidden="true">
      {#each scale.breaks() as b (b.from)}
        <g class="break">
          <title>idle {fmtDuration(b.to - b.from)} ({fmtTs(b.from, utc)} – {fmtTs(b.to, utc)})</title>
          <rect x={b.x0} y="0" width={b.x1 - b.x0} height={AXIS_H} />
          <path d={`M${b.x0 + 3} ${AXIS_H - 4} l4 -8 M${b.x0 + 7} ${AXIS_H - 4} l4 -8`} />
        </g>
      {/each}
      {#each axis as t (t.ts)}
        <line class:major={t.major} x1={t.x} x2={t.x} y1={AXIS_H - 8} y2={AXIS_H} />
        <text class:major={t.major} x={t.x + 3} y={AXIS_H - 12}>{t.label}</text>
      {/each}
      {#if nowX !== null}<line class="now" x1={nowX} x2={nowX} y1="0" y2={AXIS_H} />{/if}
    </svg>

    <div class="labels">
      {#each lay.lanes as lane (lane.key)}
        <div class="lane-label" style:height={`${LANE_H}px`} title={lane.label} data-testid="tl-lane" data-key={lane.key}>
          {lane.label}
        </div>
        {#each lane.items as item (item.kind === 'group' ? item.group.id : item.row.row.key)}
          {#if item.kind === 'group'}
            {@const g = item.group}
            <button
              type="button"
              class="group-label"
              class:flash={flash === g.id}
              style:height={`${GROUP_H}px`}
              aria-expanded={!g.collapsed}
              title={`${g.group.kind === 'tmux' ? `tmux session ${g.group.label}` : 'shells outside tmux'} on ${g.group.host}: ${g.count} ${g.count === 1 ? 'row' : 'rows'}`}
              data-testid="tl-group"
              data-key={g.group.key}
              onclick={() => toggleGroup(g)}
            >
              <span class="twist" aria-hidden="true">{g.collapsed ? '▸' : '▾'}</span>
              {#if g.group.kind === 'tmux'}<span class="kind">tmux</span>{/if}
              <span class="name">{g.group.label}</span>
              {#if g.live}<span class="live-dot" aria-label="live"></span>{/if}
              {#if g.collapsed}<span class="count">{g.count}</span>{/if}
            </button>
          {:else}
            {@const r = item.row}
            <div
              class="row-label"
              class:grouped={r.row.group !== null && lane.items[0]?.kind === 'group'}
              class:nested={r.row.depth > 0}
              style:height={`${ROW_H}px`}
              style:--depth={r.row.depth}
              title={`${r.row.depth > 0 ? 'nested shell: ' : ''}${r.row.user}@${r.row.host}${r.row.tmux.pane ? ` tmux pane ${r.row.tmux.pane}` : ''}${r.row.tty ? ` ${r.row.tty}` : ''}${r.row.holder_comm ? ` (${r.row.holder_comm})` : ''}`}
              data-testid="tl-row"
              data-key={r.row.key}
              data-depth={r.row.depth}
              data-blocks={r.row.blocks.length}
            >
              {#if r.row.depth > 0}<span class="nest" aria-hidden="true">↳</span>{/if}
              {#if r.live}<span class="live-dot" aria-label="live"></span>{/if}
              <span class="row-name">{r.row.label}</span>
            </div>
          {/if}
        {/each}
      {/each}
    </div>

    <!-- svelte-ignore a11y_no_noninteractive_tabindex, a11y_no_noninteractive_element_interactions -->
    <div
      class="plot"
      bind:this={plot}
      bind:clientWidth={width}
      tabindex="0"
      role="application"
      aria-label="Team timeline. Arrow keys move between commands, Enter opens one, plus and minus zoom, drag to zoom into a range."
      aria-describedby="tl-announce"
      {onpointermove}
      {onpointerdown}
      {onpointerup}
      {onpointerleave}
      onpointercancel={() => {
        down = null;
        brush = null;
      }}
      {onkeydown}
    >
      <svg width={scale.width} height={Math.max(lay.height, 1)} class="plot-svg" aria-hidden="true">
        <defs>
          <pattern id="tl-hatch" width="6" height="6" patternUnits="userSpaceOnUse" patternTransform="rotate(45)">
            <line class="hatch-line" x1="0" y1="0" x2="0" y2="6" />
          </pattern>
          <pattern id="tl-pause" width="6" height="6" patternUnits="userSpaceOnUse" patternTransform="rotate(-45)">
            <line class="pause-line" x1="0" y1="0" x2="0" y2="6" />
          </pattern>
        </defs>

        {#each scale.breaks() as b (b.from)}
          <rect class="break-band" x={b.x0} y="0" width={b.x1 - b.x0} height={lay.height} />
        {/each}
        {#each lay.lanes as lane (lane.key)}
          <rect class="lane-band" x="0" y={lane.y} width={scale.width} height={LANE_H} />
        {/each}
        {#each lay.groups as g (g.id)}
          <rect class="group-band" class:flash={flash === g.id} x="0" y={g.y} width={scale.width} height={GROUP_H} />
        {/each}

        {#each lay.rows as r (r.row.key)}
          <g transform={`translate(0 ${r.y})`} data-testid="tl-row-plot" data-key={r.row.key}>
            {#if r.row.depth > 0}
              <rect class="nested-row" x="0" y="0" width={scale.width} height={ROW_H - 1} data-testid="tl-nested-row" />
              {#each sessionSpans(r.row, scale, now) as s, si (si)}
                <rect class="nested-band" x={s.x} y="0" width={s.w} height={ROW_H - 1} rx="2" data-testid="tl-nested-band" />
              {/each}
            {/if}
            <line class="row-line" x1="0" x2={scale.width} y1={ROW_H - 0.5} y2={ROW_H - 0.5} />
            {#each marks[r.index]! as m, mi (mi)}
              {#if !m.point}
                <rect
                  class={`span ${m.mark.kind}`}
                  class:tmux={m.mark.tmux !== null}
                  x={m.x}
                  y="1"
                  width={m.w}
                  height={ROW_H - 2}
                  data-testid="tl-mark"
                  data-kind={m.mark.kind}
                  data-tmux={m.mark.tmux?.session_id}
                />
              {/if}
            {/each}
            {#each runs[r.index]! as run, ri (ri)}
              <rect class={`run ${run.cls}`} x={run.x} y={run.y} width={Math.max(run.w, 1)} height={run.h} rx="1.5" />
              {#if run.approx}
                <rect class="hatch" x={run.x} y={run.y} width={Math.max(run.w, 1)} height={run.h} />
              {/if}
            {/each}
            {#each activity[r.index]! as a, ai (ai)}
              <rect class={`act ${a.kind}`} x={a.x} y={ROW_H - 4} width={a.w} height="3" data-testid="tl-activity" data-kind={a.kind} />
            {/each}
            {#each bars[r.index]! as bar, bi (bi)}
              <rect class="bar" x={bar.x} y={ROW_H - 3 - bar.h} width={bar.w} height={bar.h} data-testid="tl-density" />
              {#if bar.hFailed > 0}
                <rect class="bar-failed" x={bar.x} y={ROW_H - 3 - bar.h} width={bar.w} height={bar.hFailed} />
              {/if}
            {/each}
            {#each marks[r.index]! as m, mi (mi)}
              {#if m.point && m.mark.kind === 'skip'}
                <path class="point skip" d={`M${m.x - 4} 1 h8 l-4 6 z`} data-testid="tl-mark" data-kind="skip" />
              {:else if m.point}
                <path class="point nested" d={`M${m.x} ${ROW_H / 2 - 5} l5 5 l-5 5 l-5 -5 z`} data-testid="tl-mark" data-kind="nested" />
              {/if}
            {/each}
          </g>
        {/each}

        {#each annos as a (a.annotation.uuid)}
          <rect
            class="anno"
            x={a.x}
            y={lay.rows[a.row]!.y + 1}
            width={a.w}
            height={ROW_H - 2}
            style:fill={annotationColor(a.annotation)}
            style:stroke={annotationColor(a.annotation)}
            data-testid="tl-annotation"
            data-uuid={a.annotation.uuid}
          />
        {/each}

        {#if selectedBox}
          <rect class="selected" x={selectedBox.x} y={selectedBox.y} width={selectedBox.w} height={selectedBox.h} rx="3" data-testid="tl-selected" />
        {/if}
        {#if cursorBox}
          <rect class="cursor" x={cursorBox.x} y={cursorBox.y} width={cursorBox.w} height={cursorBox.h} rx="3" data-testid="tl-cursor" />
        {/if}
        {#if nowX !== null}<line class="now" x1={nowX} x2={nowX} y1="0" y2={lay.height} />{/if}
        {#if brush}
          <rect class="brush" x={brush.x0} y="0" width={brush.x1 - brush.x0} height={lay.height} data-testid="tl-brush" />
        {/if}
      </svg>

      {#if hover}
        <div
          class="tip"
          class:flip={tip.x > scale.width - 320}
          style:left={`${tip.x}px`}
          style:top={`${tip.y + 14}px`}
          role="tooltip"
          data-testid="tl-tip"
        >
          {#if tipBlock}
            {@const info = blockInfo(tipBlock.b, tipBlock.row, sessions, now, utc)}
            <div class="tip-typed">$ {info.typed}</div>
            {#each info.lines as line, li (li)}<div>{line}</div>{/each}
          {:else if hover.kind === 'mark'}
            {@const link = markLink(hover.mark.mark, scope, location.hostname)}
            <div class="tip-title">{hover.mark.label}</div>
            <div>{hover.mark.point ? fmtTs(hover.mark.mark.ts, utc) : spanText(hover.mark.mark.ts, hover.mark.mark.ts_end)}</div>
            {#if link.external}<div class="muted">click to open it on the {hover.mark.mark.child_scope} server (new tab)</div>
            {:else if link.child}<div class="muted">click to open it</div>
            {:else if attachedGroup(hover.row, hover.mark)}<div class="muted">click to show its panes</div>{/if}
          {:else if hover.kind === 'annotation'}
            <div class="tip-title">annotation by {oneLine(hover.anno.annotation.author)}</div>
            {#if hover.anno.annotation.tags.length}<div>{hover.anno.annotation.tags.map((t) => `#${oneLine(t)}`).join(' ')}</div>{/if}
            <div>{spanText(hover.anno.annotation.ts_from, hover.anno.annotation.ts_to)}</div>
          {:else if hover.kind === 'density' && timeline.bin_secs}
            <div class="tip-title">{hover.bar.bin.commands} commands, {hover.bar.bin.failed} failed</div>
            <div>{spanText(hover.bar.bin.ts, hover.bar.bin.ts + timeline.bin_secs)}</div>
            <div class="muted">click to zoom in</div>
          {/if}
        </div>
      {/if}
    </div>
  </div>
  <div id="tl-announce" class="sr-only" aria-live="polite">{announce}</div>
</div>

<style>
  .tl-scroll {
    position: relative;
    overflow: auto;
    flex: 1 1 auto;
    min-height: 0;
    border: 1px solid var(--border);
    border-radius: var(--radius);
    background: var(--bg-elev);
  }
  .grid {
    display: grid;
    grid-template-columns: 13rem minmax(0, 1fr);
  }
  @media (max-width: 600px) {
    .grid {
      grid-template-columns: 7.5rem minmax(0, 1fr);
    }
  }
  .corner,
  .axis {
    position: sticky;
    top: 0;
    z-index: 2;
    background: var(--bg-elev);
    border-bottom: 1px solid var(--border);
  }
  .corner {
    left: 0;
    z-index: 3;
  }
  .axis {
    display: block;
    width: 100%;
  }
  .axis text {
    font-size: 11px;
    fill: var(--fg-muted);
  }
  .axis text.major {
    fill: var(--fg);
    font-weight: 600;
  }
  .axis line {
    stroke: var(--border);
  }
  .axis line.major {
    stroke: var(--fg-muted);
  }
  .break rect {
    fill: var(--bg-sunk);
  }
  .break path {
    stroke: var(--fg-muted);
    fill: none;
    stroke-width: 1.5;
  }
  .labels {
    position: sticky;
    left: 0;
    z-index: 1;
    background: var(--bg-elev);
    border-right: 1px solid var(--border);
  }
  .lane-label,
  .row-label {
    display: flex;
    align-items: center;
    gap: 0.35rem;
    padding: 0 0.5rem;
    overflow: hidden;
    white-space: nowrap;
    text-overflow: ellipsis;
  }
  .lane-label {
    font-weight: 600;
    background: var(--bg-sunk);
  }
  .row-label {
    font-family: var(--mono);
    font-size: 0.85em;
    color: var(--fg-muted);
    padding-left: calc(1.1rem + var(--depth, 0) * 0.9rem);
    border-bottom: 1px solid var(--border);
  }
  /* A nested shell (sudo inside the row above): the whole row is tinted faintly, label
     included, and shaded stronger from the start to the end of each of its sessions. */
  .row-label.nested {
    background: color-mix(in srgb, var(--accent) 6%, var(--bg-elev));
  }
  .nested-row {
    fill: var(--accent);
    fill-opacity: 0.06;
  }
  .nested-band {
    fill: var(--accent);
    fill-opacity: 0.16;
  }
  .row-label.grouped {
    padding-left: calc(1.9rem + var(--depth, 0) * 0.9rem);
  }
  .row-name {
    overflow: hidden;
    text-overflow: ellipsis;
  }
  .nest {
    flex: none;
    opacity: 0.7;
  }
  .group-label {
    display: flex;
    align-items: center;
    gap: 0.35rem;
    width: 100%;
    padding: 0 0.5rem 0 0.8rem;
    border: 0;
    border-radius: 0;
    background: var(--bg-elev);
    color: var(--fg);
    font: inherit;
    font-size: 0.85em;
    text-align: left;
    white-space: nowrap;
    overflow: hidden;
    cursor: pointer;
  }
  .group-label:hover {
    background: var(--bg-hover);
  }
  .group-label .twist {
    flex: none;
    width: 0.8em;
    color: var(--fg-muted);
  }
  .group-label .kind,
  .group-label .count {
    flex: none;
    color: var(--fg-muted);
    font-size: 0.9em;
  }
  .group-label .name {
    font-weight: 600;
    overflow: hidden;
    text-overflow: ellipsis;
  }
  .group-label.flash {
    background: var(--accent-soft);
  }
  .live-dot {
    flex: none;
    width: 0.5rem;
    height: 0.5rem;
    border-radius: 50%;
    background: var(--live);
  }
  .plot {
    position: relative;
    cursor: crosshair;
    touch-action: pan-y;
    user-select: none;
  }
  .plot:focus-visible {
    outline-offset: -2px;
  }
  .plot-svg {
    display: block;
    width: 100%;
  }
  .lane-band {
    fill: var(--bg-sunk);
  }
  .break-band {
    fill: var(--bg-sunk);
    opacity: 0.8;
  }
  .row-line {
    stroke: var(--border);
  }
  .run.ok {
    fill: var(--ok);
  }
  .run.fail {
    fill: var(--fail);
  }
  .run.unknown {
    fill: var(--unknown);
  }
  .hatch {
    fill: url(#tl-hatch);
  }
  .hatch-line {
    stroke: var(--bg-elev);
    stroke-width: 2.5;
  }
  .pause-line {
    stroke: var(--pause);
    stroke-width: 1.5;
  }
  .span.pause {
    fill: url(#tl-pause);
    stroke: var(--pause);
    stroke-opacity: 0.6;
  }
  /* Paused for a tmux client whose panes are recorded: quieter than a real pause. */
  .span.pause.tmux {
    fill: var(--pause);
    fill-opacity: 0.12;
    stroke-dasharray: 3 3;
    cursor: pointer;
  }
  .group-band {
    fill: var(--bg-elev);
  }
  .group-band.flash {
    fill: var(--accent-soft);
  }
  /* Activity strip: where the person typed (their terminal), typing elsewhere, output only. */
  .act.focus {
    fill: var(--accent);
  }
  .act.typed {
    fill: var(--accent);
    opacity: 0.4;
  }
  .act.output {
    fill: var(--fg-muted);
    opacity: 0.35;
  }
  .span.gap {
    fill: var(--gap);
    opacity: 0.35;
  }
  .span.forgotten {
    fill: var(--forgot);
    opacity: 0.45;
  }
  .point.skip {
    fill: var(--pause);
    stroke: var(--bg-elev);
  }
  .point.nested {
    fill: var(--accent);
    stroke: var(--bg-elev);
    cursor: pointer;
  }
  .bar {
    fill: var(--ok);
    opacity: 0.85;
  }
  .bar-failed {
    fill: var(--fail);
  }
  .anno {
    fill-opacity: 0.22;
    stroke-opacity: 0.8;
    stroke-width: 1;
    pointer-events: none;
  }
  .selected {
    fill: none;
    stroke: var(--accent);
    stroke-width: 2;
  }
  .cursor {
    fill: none;
    stroke: var(--focus);
    stroke-width: 2;
    stroke-dasharray: 3 2;
  }
  .now {
    stroke: var(--live);
    stroke-width: 1.5;
  }
  .brush {
    fill: var(--accent);
    fill-opacity: 0.15;
    stroke: var(--accent);
  }
  .tip {
    position: absolute;
    z-index: 4;
    max-width: 30rem;
    padding: 0.4rem 0.55rem;
    background: var(--bg-elev);
    border: 1px solid var(--border);
    border-radius: var(--radius);
    box-shadow: var(--shadow);
    font-size: 0.85em;
    pointer-events: none;
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
  }
  .tip.flip {
    transform: translateX(-100%);
  }
  .tip-typed {
    font-family: var(--mono);
    font-weight: 600;
    overflow: hidden;
    text-overflow: ellipsis;
  }
  .tip-title {
    font-weight: 600;
  }
  .muted {
    color: var(--fg-muted);
  }
  .sr-only {
    position: absolute;
    width: 1px;
    height: 1px;
    overflow: hidden;
    clip-path: inset(50%);
    white-space: nowrap;
  }
</style>
