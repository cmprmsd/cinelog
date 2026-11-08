// Annotation logic of the session view (plan §8.4; docs/API.md §9.4): tag rules mirrored
// from the server, colours, plain-text bodies with links, rail spans and applying bus
// events. The server stays authoritative; these rules only give early feedback.

import type { Annotation, AnnotationEvent, Command, Tag } from '../api/types';
import { fmtClock, oneLine } from '../format';
import type { AnnotationSpan } from '../spans';

/** Longest body in UTF-8 bytes (`MAX_ANNOTATION_BODY`). */
export const MAX_BODY_BYTES = 16 * 1024;
export const MAX_TAGS = 16;
export const MAX_TAG_CHARS = 40;

const TAG_CHAR = /^[\p{Alphabetic}\p{N}\-_.:/+#@]$/u;

/** A tag as the server stores it (`normalize_tag`), or `null` when the server would refuse it. */
export function normalizeTag(raw: string): string | null {
  const n = raw.trim().toLowerCase();
  const chars = [...n];
  if (chars.length < 1 || chars.length > MAX_TAG_CHARS) return null;
  return chars.every((c) => TAG_CHAR.test(c)) ? n : null;
}

/** Sorted, de-duplicated tag names. */
export function sortTags(tags: readonly string[]): string[] {
  return [...new Set(tags)].sort((a, b) => (a < b ? -1 : a > b ? 1 : 0));
}

export const bodyBytes = (body: string) => new TextEncoder().encode(body.trimEnd()).length;

/** The tag's colour, or one derived from its name (stable across clients). */
export function tagColor(name: string, color: string | null | undefined): string {
  if (color) return color;
  // FNV-1a over the UTF-16 units: cheap and evenly spread over the hue circle.
  let h = 0x811c9dc5;
  for (let i = 0; i < name.length; i += 1) {
    h ^= name.charCodeAt(i);
    h = Math.imul(h, 0x01000193);
  }
  return `hsl(${(h >>> 0) % 360} 60% 48%)`;
}

/** A piece of an annotation body: text, or a link (`href` is an http(s) URL). */
export interface BodyPart {
  text: string;
  href: string | null;
}

const URL_RE = /https?:\/\/[^\s<>"'`]+/gi;
const TRAILING = /[.,;:!?'"»”’]+$/;

/** Cuts trailing punctuation and closing brackets that the URL did not open. */
function trimUrl(url: string): string {
  let u = url.replace(TRAILING, '');
  for (const [open, close] of [
    ['(', ')'],
    ['[', ']'],
    ['{', '}'],
  ] as const) {
    while (u.endsWith(close) && u.split(close).length > u.split(open).length) {
      u = u.slice(0, -1).replace(TRAILING, '');
    }
  }
  return u;
}

function safeHref(url: string): string | null {
  try {
    const u = new URL(url);
    return u.protocol === 'http:' || u.protocol === 'https:' ? u.href : null;
  } catch {
    return null;
  }
}

/**
 * The body as text with `http(s)://` URLs as links; nothing else is interpreted. Rendered
 * as text nodes and `<a>` elements only (never `{@html}`).
 */
export function linkify(body: string): BodyPart[] {
  const parts: BodyPart[] = [];
  const push = (text: string, href: string | null) => {
    if (!text) return;
    const last = parts.at(-1);
    if (last && last.href === null && href === null) last.text += text;
    else parts.push({ text, href });
  };
  let at = 0;
  for (const m of body.matchAll(URL_RE)) {
    const url = trimUrl(m[0]);
    const href = safeHref(url);
    push(body.slice(at, m.index), null);
    push(url, href);
    at = m.index + url.length;
  }
  push(body.slice(at), null);
  return parts;
}

/** Oldest first, uuid breaking ties (the API's order). */
export function compareAnnotations(a: Annotation, b: Annotation): number {
  return a.created_at - b.created_at || (a.uuid < b.uuid ? -1 : a.uuid > b.uuid ? 1 : 0);
}

/**
 * uuids of annotations known to be deleted. A soft delete is final (the server refuses to
 * change a deleted annotation), so any version of one that arrives later is stale: an event
 * published out of order, or a load that started before the delete.
 */
export type Tombstones = Set<string>;

/**
 * `list` with `a` applied: inserted in order, replacing an older version of it, or removed
 * when deleted (and then remembered in `gone`). A version older than the one held, or of an
 * annotation in `gone`, changes nothing.
 */
export function upsert(list: readonly Annotation[], a: Annotation, gone?: Tombstones): Annotation[] {
  if (gone?.has(a.uuid)) return [...list];
  const i = list.findIndex((x) => x.uuid === a.uuid);
  if (i >= 0 && list[i]!.updated_at > a.updated_at) return [...list];
  const rest = i >= 0 ? list.filter((_, j) => j !== i) : [...list];
  if (a.deleted_at !== null) {
    gone?.add(a.uuid);
    return rest;
  }
  rest.push(a);
  return rest.sort(compareAnnotations);
}

/** Applies a bus `annotation` event to the annotations of `session`. */
export function applyEvent(
  list: readonly Annotation[],
  ev: AnnotationEvent,
  session: string,
  gone?: Tombstones,
): Annotation[] {
  if (ev.annotation.session !== session) return [...list];
  return upsert(list, ev.annotation, gone);
}

/** `list` without annotation `uuid`, which was deleted here; remembered in `gone`. */
export function dropDeleted(list: readonly Annotation[], uuid: string, gone: Tombstones): Annotation[] {
  gone.add(uuid);
  return list.filter((x) => x.uuid !== uuid);
}

/** A loaded list without the annotations deleted meanwhile. */
export function withoutDeleted(list: readonly Annotation[], gone: Tombstones): Annotation[] {
  return list.filter((x) => !gone.has(x.uuid));
}

/** Session seconds an annotation covers; `null` when its command is not (or no longer) known. */
export function rangeOfAnnotation(a: Annotation, commands: readonly Command[]): { from: number; to: number } | null {
  if (a.cmd_seq !== null) {
    const c = commands.find((x) => x.seq === a.cmd_seq);
    return c ? { from: c.t_start, to: c.t_end ?? c.t_start } : null;
  }
  if (a.t_from !== null && a.t_to !== null) return { from: a.t_from, to: a.t_to };
  return null;
}

/** First line of the body, at most `max` characters, made safe for one line. */
export function firstLine(body: string, max = 80): string {
  const line = oneLine(body.split(/\r?\n/, 1)[0] ?? '');
  return line.length > max ? `${line.slice(0, max - 1)}…` : line;
}

/** What the rail draws: one band per live annotation whose target is known. */
export function spansOf(
  annotations: readonly Annotation[],
  commands: readonly Command[],
  colors: ReadonlyMap<string, string | null>,
): AnnotationSpan[] {
  const out: AnnotationSpan[] = [];
  for (const a of annotations) {
    if (a.deleted_at !== null) continue;
    const r = rangeOfAnnotation(a, commands);
    if (!r) continue;
    const first = a.tags[0];
    const label = [firstLine(a.body), a.tags.map((t) => `#${t}`).join(' ')].filter(Boolean).join(' · ');
    out.push({
      uuid: a.uuid,
      from: r.from,
      to: r.to,
      color: first ? tagColor(first, colors.get(first)) : 'var(--accent)',
      label: `${label} (${a.author})`,
    });
  }
  return out;
}

/** A short label of what an annotation is attached to. */
export function targetLabel(a: Pick<Annotation, 'cmd_seq' | 't_from' | 't_to'>, commands: readonly Command[]): string {
  if (a.cmd_seq !== null) {
    const c = commands.find((x) => x.seq === a.cmd_seq);
    const typed = c ? oneLine(c.typed) : `command ${a.cmd_seq}`;
    return `$ ${typed.length > 60 ? `${typed.slice(0, 59)}…` : typed}`;
  }
  return `${fmtClock(a.t_from ?? 0)}–${fmtClock(a.t_to ?? 0)}`;
}

/** True when the playhead is on what the annotation is attached to. */
export function covers(a: Annotation, commands: readonly Command[], t: number, currentSeq: number | null): boolean {
  if (a.cmd_seq !== null) return a.cmd_seq === currentSeq;
  const r = rangeOfAnnotation(a, commands);
  return r !== null && t >= r.from && t <= r.to;
}

/**
 * Tag suggestions for what is typed: prefix matches before substring matches, then by use
 * count and name, without the tags already chosen. An empty input suggests the most used.
 */
export function suggestTags(all: readonly Tag[], input: string, chosen: readonly string[], max = 8): Tag[] {
  const q = input.trim().toLowerCase();
  const taken = new Set(chosen);
  const rank = (t: Tag) => (q === '' || t.name.startsWith(q) ? 0 : t.name.includes(q) ? 1 : 2);
  return all
    .filter((t) => !taken.has(t.name) && rank(t) < 2)
    .sort((a, b) => rank(a) - rank(b) || b.count - a.count || (a.name < b.name ? -1 : 1))
    .slice(0, max);
}

/** The tag list with `tag` added or replaced, by name. */
export function upsertTag(list: readonly Tag[], tag: Tag): Tag[] {
  return [...list.filter((t) => t.name !== tag.name), tag].sort((a, b) => (a.name < b.name ? -1 : 1));
}
