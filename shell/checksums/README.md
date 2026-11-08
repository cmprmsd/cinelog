# Release checksums

`v<VERSION>.sha256` is the `SHA256SUMS` file of release `v<VERSION>`, in `sha256sum` format
(`<64 hex>  cinelog-<target>`, one line per target). The release workflow commits it to `main`
once the assets are published. These files are never edited by hand and never rewritten, and
a version whose file is on `main` is never rebuilt: a re-release needs a new version and tag.

`shell/fetch.zsh` (zim `--on-pull`) verifies the downloaded binary against the file for the
version in `VERSION`. That makes this repository the trust anchor: the checksum has the same
trust as the plugin code you already run. If the file does not exist yet, fetch.zsh falls back
to the release's `SHA256SUMS` and warns.

Only a module that tracks `main` gets the repo trust anchor. The checksum commit lands on
`main` after the tag, so it is never part of the tagged tree:

- Tracking `main` (the default `zmodule cmprmsd/cinelog`): a pull between the tag and the
  checksum commit uses the release's `SHA256SUMS`; the next pull re-verifies the installed
  binary against the committed file.
- Pinned to a tag or commit (`zmodule cmprmsd/cinelog --tag vX.Y.Z`): every fetch verifies
  against the release's `SHA256SUMS` only, which is as trustworthy as the GitHub release
  itself. fetch.zsh says so when the checkout has a detached HEAD.

As root, fetch.zsh only trusts root-owned, non-group/world-writable files (module dir and
parents, `bin/`, `VERSION`, these checksum files, an existing `bin/cinelog`), ignores
`PATH`, `TMPDIR` and `CINELOG_FETCH_BASE_URL` from the environment and never builds with cargo.
