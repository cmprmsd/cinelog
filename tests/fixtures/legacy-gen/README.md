# Synthetic legacy casts

`legacy_gen.rs` generates casts and `*.cast.commands.log` files the way the old cinelog
plugin (asciinema 3 plus zsh `preexec`/`precmd` hooks, up to v0.9) produced them. It is
std-only and deterministic, so tests include it by path instead of reading checked-in files:

```rust
#[path = "../../../../tests/fixtures/legacy-gen/legacy_gen.rs"] // from crates/cinelog-index/src/legacy/
mod legacy_gen;

let dir = tempfile::tempdir()?;
for (name, legacy) in legacy_gen::corpus() {
    let cast = legacy.write(dir.path(), name)?; // also writes <name>.commands.log
    // legacy.expect: one entry per command (typed, logged text, banner, t, output)
}
let big = legacy_gen::long(42, 300); // 300 random commands
```

Scenarios of `corpus()` (each also splits output into events at random points, banners
included):

| File | Covers |
|---|---|
| `plain.cast` | `user@host:~$` prompt, alias expansion in the log and title, empty Enter, an unlogged `…/hist` command, quotes, `%` in a command (title prompt expansion), repeats |
| `p10k.cast` | two-line p10k-like prompt with right prompt and transient redraw, a multi-line `for` loop (`for>` continuation, `;` in the log), a full-screen command |
| `p10k-plain.cast` | the same prompt without transient redraw |
| `wrapped.cast` | a 40-column terminal and a command longer than a row |
| `backslash.cast` | `\c` (truncated log entry merged with the next line, unterminated title) and `\n` (log entry split over two lines) |
| `no-times.cast` | a `SHOW_START_END_TIME=0` period: logged commands without banners |
| `dst.cast` | the banners' wall clock steps back one hour (end of CEST) |
| `resize.cast` | a resize between commands |
| `nolog.cast` | no commands.log |
| `dumb.cast` | a `TERM=dumb` artefact without commands (imported hidden) |

Expected importer results (checked by `cinelog-index` `legacy::tests` and I14): every
banner becomes a command at the banner's time; logged ones carry the log text with
confidence ≥ 0.8, unlogged ones the text reconstructed from the screen; logged commands
without a banner are not commands (they only count in the import row's `log_lines`);
banner lines never appear in output chunks.
