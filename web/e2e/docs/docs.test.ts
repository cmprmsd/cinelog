// Keeps the documentation consistent: every relative link and image in README.md and docs/
// points at an existing file (and, for Markdown targets, an existing heading), and every
// screenshot in docs/screenshots/ is shown in the README in both themes; documented zmodule
// lines use only options zimfw accepts.

import { expect, test } from '@playwright/test';
import { existsSync, readdirSync, readFileSync } from 'node:fs';
import { dirname, join, relative } from 'node:path';
import { fileURLToPath } from 'node:url';

const ROOT = fileURLToPath(new URL('../../../', import.meta.url));
const DOCS = join(ROOT, 'docs');
const SHOTS = join(DOCS, 'screenshots');

const pages = [join(ROOT, 'README.md'), ...readdirSync(DOCS).filter((f) => f.endsWith('.md')).map((f) => join(DOCS, f))];

/** GitHub's heading anchors: lowercase, punctuation dropped, spaces to hyphens. */
function slug(heading: string): string {
  return heading
    .trim()
    .toLowerCase()
    .replace(/[^\p{L}\p{N} _-]/gu, '')
    .replace(/ /g, '-');
}

function anchors(file: string): Set<string> {
  const text = readFileSync(file, 'utf8').replace(/```[\s\S]*?```/g, '');
  return new Set([...text.matchAll(/^#{1,6} (.+)$/gm)].map((m) => slug(m[1]!)));
}

function targets(file: string): string[] {
  const text = readFileSync(file, 'utf8').replace(/```[\s\S]*?```/g, '').replace(/`[^`\n]*`/g, '');
  const md = [...text.matchAll(/\]\(([^)\s]+)\)/g)].map((m) => m[1]!);
  const html = [...text.matchAll(/(?:src|srcset|href)="([^"]+)"/g)].map((m) => m[1]!);
  return [...md, ...html].filter((t) => !/^(?:[a-z]+:|\/\/)/i.test(t));
}

test('slugs follow GitHub', () => {
  expect(slug('Team (root) recording')).toBe('team-root-recording');
  expect(slug('From a 0.10–0.13 pre-release')).toBe('from-a-010013-pre-release');
  expect(slug('`cinelog doctor`')).toBe('cinelog-doctor');
  expect(slug('record.input_suppress')).toBe('recordinput_suppress');
});

for (const page of pages) {
  test(`links in ${relative(ROOT, page)} resolve`, () => {
    const broken: string[] = [];
    for (const target of targets(page)) {
      const [path, anchor] = target.split('#') as [string, string | undefined];
      const file = path ? join(dirname(page), decodeURIComponent(path)) : page;
      if (!existsSync(file)) broken.push(`${target}: no such file`);
      else if (anchor && file.endsWith('.md') && !anchors(file).has(anchor)) broken.push(`${target}: no such heading`);
    }
    expect(broken).toEqual([]);
  });
}

test('every screenshot is in the README, in both themes', () => {
  const readme = readFileSync(join(ROOT, 'README.md'), 'utf8');
  const shots = readdirSync(SHOTS).filter((f) => f.endsWith('.png'));
  expect(shots.length).toBeGreaterThan(0);
  const views = new Set(shots.map((f) => f.replace(/-(?:light|dark)\.png$/, '')));
  for (const view of views) {
    for (const theme of ['light', 'dark']) {
      expect(shots, `${view}-${theme}.png`).toContain(`${view}-${theme}.png`);
      expect(readme, `README shows ${view}-${theme}.png`).toContain(`docs/screenshots/${view}-${theme}.png`);
    }
  }
});

test('the old screenshots are not referenced any more', () => {
  for (const page of pages) {
    expect(readFileSync(page, 'utf8'), relative(ROOT, page)).not.toMatch(/command-search\.png|input-output-search\.png|file-structure\.png|motd\.png|demo-webplayer\.gif/);
  }
});

// The options `zmodule` accepts in zimfw 1.20.2 (the release shell/tests/zim_test.zsh pins);
// any other option makes zimfw fail the whole ~/.zimrc ("Unknown zmodule option").
const ZMODULE_OPTIONS = new Set(
  '-b --branch -t --tag -u --use --no-submodules -z --frozen --on-pull --if --if-command --if-ostype -f --fpath -a --autoload -s --source -c --cmd -d --disabled'.split(' '),
);

test('every documented zmodule line uses options zimfw knows', () => {
  const lines = pages.flatMap((page) =>
    readFileSync(page, 'utf8')
      .split('\n')
      .map((l) => l.trim().replace(/^`|`$/g, ''))
      .filter((l) => l.startsWith('zmodule '))
      .map((l) => ({ page: relative(ROOT, page), line: l })),
  );
  expect(lines.length).toBeGreaterThan(0);
  const unknown = lines.flatMap(({ page, line }) =>
    line
      .split(/\s+/)
      .filter((w) => w.startsWith('-') && !ZMODULE_OPTIONS.has(w))
      .map((w) => `${page}: ${w} in ${line}`),
  );
  expect(unknown).toEqual([]);
});
