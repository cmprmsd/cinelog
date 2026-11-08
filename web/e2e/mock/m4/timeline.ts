// Mock of `GET /api/timeline` (work package M4-B). Stand-in until M4-B lands: handles
// nothing, so the mock answers 404.

import type { MockHandler } from './context.ts';

export const handle: MockHandler = async () => false;
