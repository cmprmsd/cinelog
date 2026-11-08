import { describe, expect, it } from 'vitest';
import { parseRoute, safeNext, sessionHref } from './route';

const U = '0b9e4c1a-7f3d-4e2b-9a8c-1d2e3f4a5b6c';

describe('routes', () => {
  it('parses the client routes', () => {
    expect(parseRoute('/')).toEqual({ name: 'search' });
    expect(parseRoute('')).toEqual({ name: 'search' });
    expect(parseRoute('/login/')).toEqual({ name: 'login' });
    expect(parseRoute(`/s/${U.toUpperCase()}`)).toEqual({ name: 'session', uuid: U });
    expect(parseRoute('/s/not-a-uuid')).toEqual({ name: 'notfound' });
    expect(parseRoute('/s/%E0%A4%A')).toEqual({ name: 'notfound' });
    expect(parseRoute('/timeline/')).toEqual({ name: 'timeline' });
    expect(parseRoute(`/live/${U.toUpperCase()}`)).toEqual({ name: 'live', uuid: U });
    expect(parseRoute('/live/x')).toEqual({ name: 'notfound' });
    expect(parseRoute('/timeline/x')).toEqual({ name: 'notfound' });
  });

  it('only returns to same-origin paths after login', () => {
    expect(safeNext('/s/x?cmd=1')).toBe('/s/x?cmd=1');
    expect(safeNext('//evil.example')).toBe('/');
    expect(safeNext('/\\evil.example')).toBe('/');
    expect(safeNext('https://evil.example')).toBe('/');
    expect(safeNext('/login?next=/x')).toBe('/');
    expect(safeNext(null)).toBe('/');
  });

  it('builds session links', () => {
    expect(sessionHref(U)).toBe(`/s/${U}`);
    expect(sessionHref(U, { cmd: 3 })).toBe(`/s/${U}?cmd=3`);
    expect(sessionHref(U, { t: 1.23456 })).toBe(`/s/${U}?t=1.235`);
    expect(sessionHref(U, { t: 2 }, new URLSearchParams('q=needle&cmd=9'))).toBe(`/s/${U}?q=needle&t=2`);
  });
});
