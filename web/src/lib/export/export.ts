// What the export menu offers (docs/API.md §7, §9.5): three sources (the current command, the
// rail selection, the whole session) times three formats, as `ExportParams` for
// `api.exportUrl`. The server streams the result; the name comes from `Content-Disposition`.

import type { Command, ExportFormat, ExportParams } from '../api/types';
import { fmtClock, oneLine, shortId } from '../format';
import type { TimeRange } from '../spans';

export type ExportSource = 'command' | 'selection' | 'session';

export interface FormatInfo {
  format: ExportFormat;
  /** What the file is, for the button's title. */
  hint: string;
}

export const EXPORT_FORMATS: readonly FormatInfo[] = [
  { format: 'cast', hint: 'asciicast v3 slice; plays in asciinema-player on its own' },
  { format: 'txt', hint: 'the rendered terminal rows as plain text' },
  { format: 'html', hint: 'a self-contained HTML snippet with colours, escaped and script-free' },
];

export interface ExportChoice {
  source: ExportSource;
  /** Row heading: "Current command", "Selection", "Whole session". */
  title: string;
  /** What exactly is exported (the command line or the time range); untrusted text. */
  detail: string;
  /** `null` when the source is not available (no command, no or an empty selection). */
  query: Omit<ExportParams, 'format'> | null;
  /** The range in session seconds when known (for the fallback file name). */
  range: TimeRange | null;
}

/** Selections and positions travel with millisecond precision. */
const ms = (t: number) => Math.round(t * 1000) / 1000;

/** A selection worth exporting: finite, ordered and at least 1 ms long. */
export function usableSelection(sel: TimeRange | null): TimeRange | null {
  if (!sel || !Number.isFinite(sel.from) || !Number.isFinite(sel.to)) return null;
  const from = ms(Math.max(0, Math.min(sel.from, sel.to)));
  const to = ms(Math.max(sel.from, sel.to));
  return to > from ? { from, to } : null;
}

const span = (r: TimeRange) => `${fmtClock(r.from)}–${fmtClock(r.to)}`;

/** The menu rows, in display order. The selection row is present only while there is one. */
export function exportChoices(command: Command | null, selection: TimeRange | null, duration: number | null): ExportChoice[] {
  const rows: ExportChoice[] = [];
  if (command) {
    const range = command.t_end !== null ? { from: command.t_start, to: command.t_end } : null;
    const line = oneLine(command.typed);
    rows.push({
      source: 'command',
      title: 'Current command',
      detail: `$ ${line.length > 60 ? `${line.slice(0, 59)}…` : line}${range ? ` (${span(range)})` : ' (running)'}`,
      query: { cmd: command.seq },
      range,
    });
  } else {
    rows.push({ source: 'command', title: 'Current command', detail: 'no command at the playhead', query: null, range: null });
  }
  const sel = usableSelection(selection);
  if (selection) {
    rows.push({
      source: 'selection',
      title: 'Selection',
      detail: sel ? span(sel) : 'empty selection',
      query: sel ? { from: sel.from, to: sel.to } : null,
      range: sel,
    });
  }
  const whole = duration !== null && Number.isFinite(duration) && duration > 0 ? { from: 0, to: duration } : null;
  rows.push({ source: 'session', title: 'Whole session', detail: whole ? span(whole) : 'the full recording', query: {}, range: whole });
  return rows;
}

/** The request parameters of one menu entry, or `null` when it is unavailable. */
export function exportParams(choice: ExportChoice, format: ExportFormat): ExportParams | null {
  return choice.query ? { format, ...choice.query } : null;
}

/** The name used when the response carries no usable `Content-Disposition`. */
export function fallbackName(uuid: string, format: ExportFormat, range: TimeRange | null): string {
  const r = range ? `-${Math.floor(range.from)}-${Math.ceil(range.to)}` : '';
  return `cinelog-${shortId(uuid)}${r}.${format}`;
}

/** The accessible name of an entry, e.g. "Current command as .txt". */
export const entryLabel = (choice: ExportChoice, format: ExportFormat) => `${choice.title} as .${format}`;

/** Index of the entry focus moves to for a navigation key, or `null` for other keys. */
export function moveFocus(key: string, index: number, count: number, columns: number): number | null {
  if (count <= 0) return null;
  const clamp = (i: number) => Math.max(0, Math.min(count - 1, i));
  switch (key) {
    case 'ArrowRight':
      return (index + 1) % count;
    case 'ArrowLeft':
      return (index - 1 + count) % count;
    case 'ArrowDown':
      return clamp(index + columns);
    case 'ArrowUp':
      return clamp(index - columns);
    case 'Home':
      return 0;
    case 'End':
      return count - 1;
    default:
      return null;
  }
}
