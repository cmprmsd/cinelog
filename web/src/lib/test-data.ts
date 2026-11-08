// Builders for unit tests.

import type { Command, SearchHit } from './api/types';

export function hit(over: Partial<SearchHit> = {}): SearchHit {
  return {
    id: 'c:1',
    kind: 'command',
    session: '11111111-2222-3333-4444-555555555555',
    cmd_seq: 1,
    t: 1,
    ts: 1_700_000_000,
    user: 'alice',
    host: 'lab1',
    pane: '%1',
    tmux_session: 'work',
    exit: 0,
    cwd: '/srv',
    typed: 'echo needle',
    snippet: [
      { text: 'echo ', hit: false },
      { text: 'needle', hit: true },
    ],
    chunk_kind: null,
    approx: false,
    confidence: null,
    legacy: false,
    ...over,
  };
}

export function cmd(seq: number, t_start: number, over: Partial<Command> = {}): Command {
  return {
    seq,
    parent_seq: null,
    depth: 0,
    t_start,
    t_end: t_start + 1,
    ts_start: 1_700_000_000 + t_start,
    ts_end: 1_700_000_001 + t_start,
    exit: 0,
    pipestatus: [0],
    cwd: '/srv',
    user: 'alice',
    typed: `cmd ${seq}`,
    expanded: null,
    typed_trunc: false,
    approx: false,
    confidence: null,
    out_bytes: 0,
    out_truncated: false,
    ...over,
  };
}
