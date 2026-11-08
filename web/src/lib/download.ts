// Downloads through fetch + a blob URL, so the HttpOnly session cookie applies (API.md §7).

import { ApiFailure } from './api/client';

/** The file name from `Content-Disposition`, reduced to a safe basename. */
export function filenameFromDisposition(header: string | null, fallback: string): string {
  if (!header) return fallback;
  const star = /filename\*\s*=\s*(?:UTF-8'')?([^;]+)/i.exec(header);
  const plain = /filename\s*=\s*"([^"]*)"|filename\s*=\s*([^;]+)/i.exec(header);
  let name: string | undefined;
  if (star?.[1]) {
    try {
      name = decodeURIComponent(star[1].trim().replace(/^"|"$/g, ''));
    } catch {
      name = undefined;
    }
  }
  name ??= (plain?.[1] ?? plain?.[2])?.trim();
  // eslint-disable-next-line no-control-regex -- stripping control characters is the point
  const base = name?.split(/[\\/]/).pop()?.replace(/[\u0000-\u001f]/g, '');
  return base && base !== '.' && base !== '..' ? base : fallback;
}

export async function download(url: string, fallback: string): Promise<string> {
  const res = await fetch(url, { credentials: 'same-origin' });
  if (!res.ok) {
    let message = `${res.status} ${res.statusText}`;
    try {
      const body = (await res.json()) as { message?: string };
      if (body.message) message = body.message;
    } catch {
      // Keep the status line.
    }
    throw new ApiFailure(res.status, 'internal', message);
  }
  const name = filenameFromDisposition(res.headers.get('content-disposition'), fallback);
  const blob = await res.blob();
  const href = URL.createObjectURL(blob);
  try {
    const a = document.createElement('a');
    a.href = href;
    a.download = name;
    a.rel = 'noopener';
    document.body.append(a);
    a.click();
    a.remove();
  } finally {
    setTimeout(() => URL.revokeObjectURL(href), 10_000);
  }
  return name;
}
