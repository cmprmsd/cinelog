// Checks that the demo data set is well-formed before it is shown in the documentation:
// valid asciicast v3, the recorder's marker shapes, paired commands, and the session
// relations the screenshots rely on.

import { expect, test } from '@playwright/test';
import { mkdtempSync, readdirSync, readFileSync, rmSync, statSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { buildDemo, castEvents, DEMO, eventLines, legacyCast, writeDemo, type Ev } from './dataset.ts';

const NOW = 1_762_600_000;

function parse(text: string): { header: Record<string, unknown>; events: Ev[] } {
  const [head, ...rest] = text.trimEnd().split('\n');
  let t = 0;
  const events = rest.map((line) => {
    const [dt, code, data] = JSON.parse(line) as [number, string, string];
    expect(dt).toBeGreaterThanOrEqual(0);
    t += dt;
    return [t, code, data] as Ev;
  });
  return { header: JSON.parse(head!), events };
}

type Marker = Record<string, unknown> & { t: number; k: string; seq?: number };

const markers = (ev: Ev[]): Marker[] => ev.filter(([, c]) => c === 'm').map(([t, , d]) => ({ ...(JSON.parse(d) as Record<string, unknown>), t }) as Marker);

test('every cinelog cast opens with a session marker and pairs its commands', () => {
  for (const s of buildDemo(NOW).filter((s) => !s.legacy)) {
    const ev = castEvents(s, NOW - s.start);
    const m = markers(ev);
    expect(m[0]).toMatchObject({ cl: 1, k: 'session', uuid: s.uuid, protocol: 1 });
    const cmds = m.filter((x) => x.k === 'cmd');
    const dones = m.filter((x) => x.k === 'done');
    expect(cmds.map((c) => c.seq)).toEqual(s.commands.map((_, i) => i + 1));
    expect(dones.length).toBe(s.commands.filter((c) => c.dur !== null).length);
    for (const d of dones) {
      const c = cmds.find((x) => x.seq === d.seq)!;
      expect(d.t).toBeGreaterThan(c.t);
    }
    for (let i = 1; i < ev.length; i++) expect(ev[i]![0]).toBeGreaterThanOrEqual(ev[i - 1]![0]);
  }
});

test('no output is recorded while a session is paused', () => {
  for (const s of buildDemo(NOW).filter((s) => !s.legacy)) {
    const ev = castEvents(s, NOW - s.start);
    const pauses = s.markers.filter((m) => m.k === 'pause');
    for (const p of pauses) {
      const resume = s.markers.find((m) => m.k === 'resume' && m.t > p.t)!;
      expect(resume).toBeDefined();
      const inside = ev.filter(([t, code]) => t > p.t && t < resume.t && (code === 'o' || code === 'i'));
      expect(inside).toEqual([]);
    }
  }
});

test('the nested root session points at its parent and starts at the nested marker', () => {
  const all = buildDemo(NOW);
  const deploy = all.find((s) => s.uuid === DEMO.deploy)!;
  const root = all.find((s) => s.uuid === DEMO.root)!;
  const nested = deploy.markers.find((m) => m.k === 'nested')!;
  expect(nested.data.child_uuid).toBe(DEMO.root);
  expect(root.parent).toBe(DEMO.deploy);
  expect(root.start).toBe(deploy.start + nested.t);
  const shot = deploy.commands[DEMO.deploySeq - 1]!;
  expect(shot.typed).toContain(DEMO.query);
  expect(DEMO.deployShotT).toBeGreaterThan(shot.at);
  expect(DEMO.deployShotT).toBeLessThan(shot.at + shot.dur!);
});

test('the legacy cast has banners, no session marker and one log line per command', () => {
  const legacy = buildDemo(NOW).find((s) => s.legacy)!;
  const { cast, log } = legacyCast(legacy);
  const { header, events } = parse(cast);
  expect(header.version).toBe(3);
  expect(events.some(([, c]) => c === 'm')).toBe(false);
  expect(events.filter(([, , d]) => /Begin \[\d{4}-\d\d-\d\d \d\d:\d\d:\d\d\]/.test(d))).toHaveLength(legacy.commands.length);
  expect(events.filter(([, , d]) => /End {3}\[/.test(d))).toHaveLength(legacy.commands.length);
  expect(log.trimEnd().split('\n')).toHaveLength(legacy.commands.length);
});

test('writeDemo lays out private casts by month and leaves the live cast open', () => {
  const dir = mkdtempSync(join(tmpdir(), 'cinelog-docs-test-'));
  try {
    const live = writeDemo(dir, NOW);
    const casts = readdirSync(dir, { recursive: true, encoding: 'utf8' }).filter((f) => f.endsWith('.cast'));
    expect(casts).toHaveLength(buildDemo(NOW).length);
    for (const f of casts) expect(statSync(join(dir, f)).mode & 0o777).toBe(0o600);
    const events = parse(readFileSync(live.path, 'utf8')).events;
    expect(events.some(([, c]) => c === 'x')).toBe(false);
    expect(parse(readFileSync(live.path, 'utf8')).header.timestamp).toBe(Math.floor(live.start));
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test('event lines encode intervals', () => {
  expect(eventLines([[1, 'o', 'a'], [1.5, 'o', 'b']], 0.25)).toBe('[0.750000, "o", "a"]\n[0.500000, "o", "b"]\n');
});
