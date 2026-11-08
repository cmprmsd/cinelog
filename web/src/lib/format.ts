// Display formatting. Wall times are epoch seconds (UTC); cast times are session seconds.

const pad = (n: number, w = 2) => String(n).padStart(w, '0');

/** `YYYY-MM-DD HH:MM:SS` in local time, or in UTC. */
export function fmtTs(ts: number, utc: boolean): string {
  const d = new Date(ts * 1000);
  if (Number.isNaN(d.getTime())) return '?';
  const [y, mo, da, h, mi, s] = utc
    ? [d.getUTCFullYear(), d.getUTCMonth(), d.getUTCDate(), d.getUTCHours(), d.getUTCMinutes(), d.getUTCSeconds()]
    : [d.getFullYear(), d.getMonth(), d.getDate(), d.getHours(), d.getMinutes(), d.getSeconds()];
  return `${y}-${pad(mo + 1)}-${pad(da)} ${pad(h)}:${pad(mi)}:${pad(s)}`;
}

/** The zone the UI shows times in. */
export function tzLabel(utc: boolean): string {
  if (utc) return 'UTC';
  try {
    return Intl.DateTimeFormat().resolvedOptions().timeZone || 'local';
  } catch {
    return 'local';
  }
}

/** A human duration: `850ms`, `12.3s`, `3m 05s`, `1h 02m`, `2d 4h`. */
export function fmtDuration(s: number | null | undefined): string {
  if (s === null || s === undefined || !Number.isFinite(s) || s < 0) return '–';
  if (s < 1) return `${Math.round(s * 1000)}ms`;
  if (s < 60) return `${s.toFixed(1)}s`;
  const total = Math.floor(s);
  const d = Math.floor(total / 86400);
  const h = Math.floor((total % 86400) / 3600);
  const m = Math.floor((total % 3600) / 60);
  const sec = total % 60;
  if (d > 0) return `${d}d ${h}h`;
  if (h > 0) return `${h}h ${pad(m)}m`;
  return `${m}m ${pad(sec)}s`;
}

/** Player-style clock for a cast position: `m:ss` or `h:mm:ss`. */
export function fmtClock(t: number): string {
  const total = Math.max(0, Math.floor(t));
  const h = Math.floor(total / 3600);
  const m = Math.floor((total % 3600) / 60);
  const s = total % 60;
  return h > 0 ? `${h}:${pad(m)}:${pad(s)}` : `${m}:${pad(s)}`;
}

export function fmtBytes(n: number | null | undefined): string {
  if (n === null || n === undefined || !Number.isFinite(n)) return '–';
  const units = ['B', 'KiB', 'MiB', 'GiB', 'TiB'];
  let v = n;
  let i = 0;
  while (v >= 1024 && i < units.length - 1) {
    v /= 1024;
    i += 1;
  }
  return i === 0 ? `${v} B` : `${v.toFixed(v < 10 ? 1 : 0)} ${units[i]}`;
}

/** Shortens a uuid for labels. */
export const shortId = (uuid: string) => uuid.slice(0, 8);

/**
 * Makes terminal text safe to show on one line: newlines become `⏎`, tabs a space, other
 * C0/C1 controls their visible Control Pictures form. Rendering stays text-only.
 */
export function oneLine(text: string): string {
  // eslint-disable-next-line no-control-regex -- stripping control characters is the point
  return text.replace(/\r?\n|\r/g, ' ⏎ ').replace(/[\t]/g, ' ').replace(/[\u0000-\u001f\u007f-\u009f]/g, (c) => {
    const code = c.charCodeAt(0);
    if (code < 0x20) return String.fromCharCode(0x2400 + code);
    if (code === 0x7f) return '␡';
    return '�';
  });
}

/** One keystroke from the player's `input` event, as a short visible label. */
export function keyLabel(data: string): string {
  switch (data) {
    case '\r':
    case '\n':
      return '⏎';
    case '\t':
      return '⇥';
    case '\x7f':
    case '\b':
      return '⌫';
    case '\x1b':
      return 'Esc';
    case ' ':
      return '␣';
    case '\x1b[A':
      return '↑';
    case '\x1b[B':
      return '↓';
    case '\x1b[C':
      return '→';
    case '\x1b[D':
      return '←';
  }
  if (data.length === 1) {
    const code = data.charCodeAt(0);
    if (code < 0x20) return `^${String.fromCharCode(code + 0x40)}`;
  }
  if (data.startsWith('\x1b')) return `Esc${oneLine(data.slice(1))}`;
  return oneLine(data);
}
