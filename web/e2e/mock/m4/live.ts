// Mock of `GET /api/sessions/{uuid}/live` (work package M4-D). Stand-in until M4-D lands:
// handles nothing, so the mock answers 404.

import type { MockHandler } from './context.ts';

export const handle: MockHandler = async () => false;
