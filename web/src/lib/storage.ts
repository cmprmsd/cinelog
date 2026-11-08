// Per-browser UI preferences. Storage can be missing or throw (private windows, blocked
// site data), so every access is guarded and the UI works without it.

export function readPref(key: string): string | null {
  try {
    return globalThis.localStorage?.getItem(key) ?? null;
  } catch {
    return null;
  }
}

export function writePref(key: string, value: string | null): void {
  try {
    if (value === null) globalThis.localStorage?.removeItem(key);
    else globalThis.localStorage?.setItem(key, value);
  } catch {
    // Preferences are a convenience; losing them is fine.
  }
}
