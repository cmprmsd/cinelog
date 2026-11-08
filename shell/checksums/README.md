# Release checksums

`v<VERSION>.sha256` is the `SHA256SUMS` file of release `v<VERSION>`, in `sha256sum` format
(`<64 hex>  cinelog-<target>`, one line per target). The release workflow commits it to `main`
once the assets are published. These files are never edited by hand and never rewritten.

`shell/fetch.zsh` (zim `--on-pull`) verifies the downloaded binary against the file for the
version in `VERSION`. That makes this repository the trust anchor: the checksum has the same
trust as the plugin code you already run. If the file does not exist yet (you pulled between
the tag and the checksum commit), fetch.zsh falls back to the release's `SHA256SUMS` and warns;
the next pull re-verifies the installed binary against the committed file.
