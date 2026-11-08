// The demo server must never fall back to the machine's real host name on its own: legacy
// casts take it from the machine, and it would end up in committed screenshots.

import { expect, test } from '@playwright/test';
import { withHostname } from './server.ts';

test('a missing unshare fails the run unless the real host name is accepted', () => {
  expect(() => withHostname('build01', {}, '/nonexistent/unshare')).toThrow(/CINELOG_DOCS_REAL_HOSTNAME=1/);
  expect(withHostname('build01', { CINELOG_DOCS_REAL_HOSTNAME: '1' }, '/nonexistent/unshare')).toEqual([]);
});

test('an unshare that cannot set the host name fails the run', () => {
  // `false` exits 1, like unshare where user namespaces are disabled.
  expect(() => withHostname('build01', {}, 'false')).toThrow(/demo host name build01/);
  expect(() => withHostname('build01', { CINELOG_DOCS_REAL_HOSTNAME: '0' }, 'false')).toThrow();
});
