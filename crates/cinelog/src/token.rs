//! `cinelog token new|list|revoke|team-login` and the personal token of `install` (work
//! package M3-D; plan §7.2, §7.7; docs/API.md "Tokens").
//!
//! Provides, with exactly these signatures:
//! - [`run`]: the `token` subcommand over [`cinelog_web::tokens::TokenFile`] at
//!   `ctx.paths.auth_file` (0600; TEAM `/etc/cinelog/tokens.toml` needs EUID 0).
//!   `new NAME` prints the token once; PERSONAL `new` replaces the one token (named after
//!   the user). `revoke NAME` removes it and calls `cinelog_web::auth::revoke_principal` on
//!   meta.db. `list` prints names and creation times only. `team-login` stores
//!   `{url, token, fingerprint}` in `$XDG_CONFIG_HOME/cinelog/team-token` (0600) for
//!   `search --team`, pinning the fingerprint (TOFU unless `--fingerprint` is given);
//! - [`ensure_personal`]: creates the PERSONAL token when `auth.toml` has none and returns it
//!   (`install` prints it once); `None` when one exists.
//!
//! The bodies below are the pre-integration stand-ins of the contracts commit.

use std::process::ExitCode;

use cinelog_web::tokens::NewToken;

use crate::cli::TokenArgs;
use crate::ctx::Ctx;

pub fn run(args: &TokenArgs) -> ExitCode {
    let _ = args;
    eprintln!("cinelog: `token` is not available in this build yet");
    ExitCode::from(2)
}

pub fn ensure_personal(ctx: &Ctx) -> anyhow::Result<Option<NewToken>> {
    let _ = ctx;
    Ok(None)
}
