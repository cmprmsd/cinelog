// Mock of `/api/annotations`, `/api/annotations/{uuid}` and `/api/tags` (work package
// M4-C). Stand-in until M4-C lands: handles nothing, so the mock answers 404.

import type { MockHandler } from './context.ts';

export const handle: MockHandler = async () => false;
