// Shared shapes of the session view's M4 parts (docs/API.md §9.6): the rail selection that
// exports and annotations use, and the annotation spans the rail draws.

/** A span of session seconds, `from ≤ to`. */
export interface TimeRange {
  from: number;
  to: number;
}

/** An annotation drawn on the rail (from `AnnotationsPanel`). */
export interface AnnotationSpan {
  uuid: string;
  from: number;
  to: number;
  /** CSS colour (the first tag's, or derived from its name). */
  color: string;
  /** Hover text: the body's first line and the tags. */
  label: string;
}

/** A selection from two drag positions, ordered and clamped to `[0, duration]`. */
export function rangeOf(a: number, b: number, duration: number): TimeRange {
  const clamp = (t: number) => Math.max(0, Math.min(duration, t));
  const [from, to] = a <= b ? [a, b] : [b, a];
  return { from: clamp(from), to: clamp(to) };
}
