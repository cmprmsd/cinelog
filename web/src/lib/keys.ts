// Keyboard-first shortcuts (plan §8.1).

export type Action =
  | 'focus-search'
  | 'escape'
  | 'next-result'
  | 'prev-result'
  | 'open-result'
  | 'open-result-tab'
  | 'next-hit'
  | 'prev-hit'
  | 'prev-chapter'
  | 'next-chapter'
  | 'toggle-play'
  | 'back-5'
  | 'forward-5'
  | 'export'
  | 'timeline'
  | 'annotations'
  | 'toggle-fit'
  | 'toggle-live'
  | 'annotate'
  | 'help';

export interface KeyLike {
  key: string;
  ctrlKey: boolean;
  metaKey: boolean;
  altKey: boolean;
}

const PLAIN: Record<string, Action> = {
  '/': 'focus-search',
  j: 'next-result',
  k: 'prev-result',
  Enter: 'open-result',
  o: 'open-result-tab',
  n: 'next-hit',
  N: 'prev-hit',
  '[': 'prev-chapter',
  ']': 'next-chapter',
  ' ': 'toggle-play',
  ',': 'back-5',
  '.': 'forward-5',
  e: 'export',
  t: 'timeline',
  A: 'annotations',
  w: 'toggle-fit',
  l: 'toggle-live',
  a: 'annotate',
  '?': 'help',
};

/** What has focus: a text field, a button-like control, a link, or anything else. */
export type FocusKind = 'typing' | 'button' | 'link' | 'none';

/**
 * The action for a key press. While typing only `Esc` is ours. A focused button keeps
 * `Enter` and `Space`, a focused link keeps `Enter` (it opens the link).
 */
export function shortcutFor(e: KeyLike, focus: FocusKind): Action | null {
  if (e.key === 'Escape') return 'escape';
  if (focus === 'typing' || e.ctrlKey || e.metaKey || e.altKey) return null;
  if (e.key === 'Enter' && (focus === 'button' || focus === 'link')) return null;
  if (e.key === ' ' && focus === 'button') return null;
  return PLAIN[e.key] ?? null;
}

const NON_TEXT_INPUTS = new Set(['button', 'checkbox', 'radio', 'submit', 'reset', 'range', 'color', 'file']);

export function focusKind(el: EventTarget | null): FocusKind {
  if (!(el instanceof HTMLElement)) return 'none';
  if (el.isContentEditable || el instanceof HTMLTextAreaElement || el instanceof HTMLSelectElement) return 'typing';
  if (el instanceof HTMLInputElement) return NON_TEXT_INPUTS.has(el.type) ? 'button' : 'typing';
  if (el instanceof HTMLButtonElement || el.tagName === 'SUMMARY' || el.getAttribute('role') === 'button') return 'button';
  if (el instanceof HTMLAnchorElement && el.hasAttribute('href')) return 'link';
  return 'none';
}

export const SHORTCUTS: readonly { keys: string[]; what: string }[] = [
  { keys: ['/'], what: 'Focus the search field' },
  { keys: ['Esc'], what: 'Leave the field, close a dialog' },
  { keys: ['j', 'k'], what: 'Next / previous result' },
  { keys: ['Enter'], what: 'Open the result in the player' },
  { keys: ['o'], what: 'Open the result in a new tab' },
  { keys: ['n', 'N'], what: 'Next / previous hit in the open session' },
  { keys: ['[', ']'], what: 'Previous / next command' },
  { keys: ['Space'], what: 'Play / pause' },
  { keys: [',', '.'], what: 'Back / forward 5 s' },
  { keys: ['w'], what: 'Fit the terminal to the screen or to the width' },
  { keys: ['e'], what: 'Export the current command' },
  { keys: ['t'], what: 'Team timeline' },
  { keys: ['A'], what: 'Annotations overview' },
  { keys: ['+', '-'], what: 'Zoom the timeline' },
  { keys: ['l'], what: 'Switch between the recording and the live view' },
  { keys: ['a'], what: 'Annotate the current command or selection' },
  { keys: ['?'], what: 'This sheet' },
];
