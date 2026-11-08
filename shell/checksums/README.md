# Release checksums

`v<VERSION>.sha256` is the `SHA256SUMS` file of release `v<VERSION>`, in `sha256sum` format
(`<64 hex>  cinelog-<target>`, one line per target). The release workflow commits it to `main`
once the assets are published. These files are never edited by hand and never rewritten, and
a version whose file is on `main` is never rebuilt: a re-release needs a new version and tag.

`shell/fetch.zsh` (zim `--on-pull`) verifies the downloaded binary against the file for the
version in `VERSION`. That makes this repository the trust anchor: the checksum has the same
trust as the plugin code you already run. If the file does not exist yet, fetch.zsh falls back
to the release's `SHA256SUMS` and warns, except in a pinned checkout (below).

Only a module that tracks `main` gets the repo trust anchor. The checksum commit lands on
`main` after the tag, so it is never part of the tagged tree:

- Tracking `main` (the default `zmodule cmprmsd/cinelog`): a pull between the tag and the
  checksum commit uses the release's `SHA256SUMS`; the next pull re-verifies the installed
  binary against the committed file.
- Pinned to a tag or commit (`zmodule cmprmsd/cinelog --tag vX.Y.Z`): the file never arrives,
  and the release's `SHA256SUMS` comes from the same server as the binary, so a compromised
  release could supply both. fetch.zsh (detached HEAD) refuses it and builds the pinned
  source with cargo instead (or keeps a binary it already has). To install the release
  binary, copy `v<VERSION>.sha256` from `main` into this directory of the checkout (it is
  then the trust anchor, as on `main`), or run `zsh shell/fetch.zsh --trust-release-sums`
  (`cinelog-fetch --trust-release-sums`) to accept the release's own `SHA256SUMS`.

As root, fetch.zsh only trusts root-owned, non-group/world-writable files (module dir and
parents, `bin/`, `VERSION`, these checksum files, an existing `bin/cinelog`), ignores
`PATH`, `TMPDIR` and `CINELOG_FETCH_BASE_URL` from the environment and never builds with cargo.
