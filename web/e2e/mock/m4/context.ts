// What the contract mock (`../server.ts`) hands the M4 mock handlers (docs/API.md §9.7).
// Each handler module belongs to one work package: `live.ts` M4-D, `timeline.ts` M4-B,
// `annotations.ts` M4-C.

import type { IncomingMessage, ServerResponse } from 'node:http';
import type { BusEvent, ErrorCode } from '../../../src/lib/api/types.ts';
import type { FixSession } from '../fixtures.ts';

export interface MockContext {
  /** Every fixture session, forgotten ones included. */
  readonly fixtures: readonly FixSession[];
  /** Fixture sessions that are not forgotten. */
  visible(): FixSession[];
  /** The session, or a 404 `not_found` failure. */
  find(uuid: string): FixSession;
  /** Throws an API error response (`ApiError` body). */
  fail(status: number, code: ErrorCode, message: string): never;
  /** The JSON body of a mutating request (`undefined` when empty; 400 on bad JSON). */
  readJson(req: IncomingMessage): Promise<unknown>;
  json(res: ServerResponse, status: number, body: unknown): void;
  /** The forget mask of a session: the `by` of a forgotten command seq, else null. */
  maskFor(f: FixSession): (seq: number) => string | null;
  /** Sends a UI bus event to every open `/api/events` stream. */
  publish(event: BusEvent): void;
  /** ts the mock started; fixture times are relative to it. */
  readonly startedAt: number;
}

export interface MockRequest {
  req: IncomingMessage;
  res: ServerResponse;
  url: URL;
  method: string;
  /** The signed-in principal (the mock has one, `alice`, unless a handler says otherwise). */
  principal: string;
}

/**
 * Handles a request it owns and returns true; returns false to pass it on (the mock answers
 * 404 when nobody handles it). Runs after the session check, before the M3 routes reject
 * methods other than GET.
 */
export type MockHandler = (ctx: MockContext, r: MockRequest) => Promise<boolean>;
