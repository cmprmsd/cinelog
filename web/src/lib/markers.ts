// Recorder markers as bands and points on the session rail: pauses, gaps, forgotten
// ranges and nested child sessions (plan §8.2, §15.2: every pause leaves a visible marker).

import type { MaskedSpan, Marker } from './api/types';

export interface Band {
  kind: 'pause' | 'gap' | 'forgotten';
  from: number;
  to: number;
  label: string;
}

export interface NestedPoint {
  t: number;
  child: string | null;
  label: string;
}

const str = (v: unknown): string | null => (typeof v === 'string' && v ? v : null);
const num = (v: unknown): number | null => (typeof v === 'number' && Number.isFinite(v) ? v : null);

function pauseLabel(data: Record<string, unknown>): string {
  const reason = str(data.reason);
  const detail = str(data.detail);
  if (!reason) return 'paused';
  return detail ? `paused (${reason}: ${detail})` : `paused (${reason})`;
}

export function markerBands(
  markers: readonly Marker[],
  masked: readonly MaskedSpan[],
  duration: number,
): { bands: Band[]; nested: NestedPoint[] } {
  const bands: Band[] = [];
  const nested: NestedPoint[] = [];
  let open: { t: number; label: string } | null = null;
  for (const m of markers) {
    switch (m.kind) {
      case 'pause':
        if (!open) open = { t: m.t, label: pauseLabel(m.data) };
        break;
      case 'resume':
        if (open) {
          bands.push({ kind: 'pause', from: open.t, to: Math.max(open.t, m.t), label: open.label });
          open = null;
        }
        break;
      case 'gap': {
        const from = num(m.data.from_t) ?? m.t;
        const to = num(m.data.to_t) ?? from;
        const bytes = num(m.data.dropped_bytes);
        bands.push({ kind: 'gap', from, to: Math.max(from, to), label: bytes ? `gap (${bytes} bytes dropped)` : 'gap' });
        break;
      }
      case 'nested': {
        const user = str(m.data.user);
        const sudo = str(m.data.sudo_user);
        const mode = str(m.data.mode);
        let label = user ? `nested: ${user}` : 'nested session';
        if (sudo && sudo !== user) label += ` (sudo by ${sudo})`;
        if (mode === 'pause') label += ', outer paused';
        nested.push({ t: m.t, child: str(m.data.child_uuid), label });
        break;
      }
    }
  }
  if (open) bands.push({ kind: 'pause', from: open.t, to: Math.max(open.t, duration), label: open.label });

  // The server reports what playback masks; a forgotten command has left the command list.
  for (const f of masked) {
    bands.push({ kind: 'forgotten', from: f.t_from, to: Math.max(f.t_from, f.t_to ?? duration), label: `forgotten by ${f.by}` });
  }
  bands.sort((a, b) => a.from - b.from);
  return { bands, nested };
}
