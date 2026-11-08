//! `cinelog search --team` (work package M3-D; plan §7.7; docs/API.md "Team search client").
//!
//! Provides, with exactly this signature, [`store`]: a [`Store`] whose reader is a
//! `Searcher` over the team server's HTTPS API (`GET /api/search`, `GET /api/show`,
//! `GET /api/status`), authenticated with the token in
//! `$XDG_CONFIG_HOME/cinelog/team-token` (`cinelog token team-login`) and pinned to its
//! certificate fingerprint; `meta()` and `writer()` fail (read-only). The rest of `search`
//! (fzf, rendering) is unchanged.
//!
//! The body below is the pre-integration stand-in of the contracts commit.

use super::Store;
use crate::ctx::Ctx;

pub fn store(ctx: &Ctx) -> anyhow::Result<Box<dyn Store>> {
    let _ = ctx;
    anyhow::bail!("`search --team` is not available in this build yet")
}
