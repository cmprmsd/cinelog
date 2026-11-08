# M2 verification record (v0.11, re-measured with 1.0)

Plan §13 "M2 Accept". The real-corpus numbers were first taken against the 0.11 tree and
re-measured on 2026-11-08 with a release build of the 1.0 tree; the table shows the 1.0 run.
Dev machine: Arch Linux, kernel 6.18, 16 threads, NVMe. The real corpus (`~/logs`, 1650
old-plugin casts, 3.4 GiB) was mounted read-only as the lower layer of an overlayfs inside a
user namespace, with a recursive inotify watcher on it; the databases went to a scratch
HOME. Only counts and timings were recorded, never cast content.

| Item | Result | How |
|---|---|---|
| 1. I11–I14 and I17 green | pass | `cargo test -p cinelog-it` (the whole suite, 107 tests, 0 failed; I17's root half needs `CINELOG_IT_ROOT=1`). |
| 2. Real-corpus import under 15 min | pass | `cinelog index --once`: 1650 of 1650 casts, 0 failed, 9896 commands, 110777 output chunks in 67 s (0.11: 66 s). index.db 1.0 GB under `~/.local/state/cinelog/<host>/`. |
| 2. §11 targets: never modify the casts | pass | inotify on the corpus: 0 events during the import (and during `archive-legacy`, below); the overlay's upper layer stayed empty. |
| 2. Legacy alignment | pass | `import --report`: 1428 imported, 222 hidden (empty or banner-only), 0 failed; 9896 banners, 9590 commands-log lines, 9566 aligned (99.7 % of the log lines). |
| 3. `search -a`, 3–13 character terms, first results p95 < 300 ms | pass | 30 random terms from the index × 5 runs of `cinelog search -a --print`: first line p50 6 ms, p95 24 ms, max 68 ms; complete p95 55 ms. |
| 3. Terms under 3 characters finish in under 1 s | pass | 12 terms × 5: complete p50 40 ms, p95 113 ms, max 117 ms. No-match terms: 5 ms (long), 27 ms (short). |
| 4. A command in a recorded pane is findable within 3 s | pass | I11 (`i11_live_commands_and_output_are_searchable_within_3s`). |
| 5. `install` stops the old Python server; nothing else on the persisted port | pass | `install_stops_the_old_python_viewer` (crates/cinelog/tests/cli.rs); the M5 E2E script checks `doctor`'s `old-server` line. |
| 6. Annotations survive `index --rebuild` | pass | `annotations_survive_an_index_rebuild` (cli.rs) and I23 (rebuild and meta export/import). |
| `archive-legacy` on the real corpus (§6.6, added in 1.0) | pass | 1650 of 1650 casts, 3.66 GB → 137 MB (27×) at level 9 in 20 s; a second run skips all 1650; 0 inotify events on the originals. The copies were deleted afterwards. |

## Fixed after verification (1.0)

- A legacy cast the old plugin still appends to gets a new uuid at every import; its forget
  rows, playback spans and annotations are now carried over to the new uuid
  (`db::tests::a_forget_on_a_legacy_cast_survives_its_growth`, docs/SECURITY.md §3).
