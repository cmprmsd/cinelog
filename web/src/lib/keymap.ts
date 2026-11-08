// Routes shortcut actions to whichever mounted view handles them; the most recently
// registered handler for an action wins (a session view over the result list, a dialog
// over both).

import type { Action } from './keys';

export type Handlers = Partial<Record<Action, () => boolean | void>>;

const stack: Handlers[] = [];

/** Registers handlers until the returned function is called (use as an `$effect` cleanup). */
export function useKeys(handlers: Handlers): () => void {
  stack.push(handlers);
  return () => {
    const i = stack.lastIndexOf(handlers);
    if (i >= 0) stack.splice(i, 1);
  };
}

/** Runs the action; a handler returning `false` passes it on. True when handled. */
export function dispatch(action: Action): boolean {
  for (let i = stack.length - 1; i >= 0; i -= 1) {
    const fn = stack[i]![action];
    if (fn && fn() !== false) return true;
  }
  return false;
}
