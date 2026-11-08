//! `cinelog serve`, `compress` and `meta` (work package M2-D; plan §6.4, §6.6, §10.4).
//!
//! Owns: the serve.lock singleton and `--ensure`, discovery (registry + 60 s dir walk +
//! [`cinelog_index::classify()`]), the scheduler (live sessions first, `min(4, cores/2)`
//! backfill threads feeding the single writer), live incremental indexing (2 s commits),
//! crash detection, the daily compressor, the nightly meta backup and
//! `packaging/systemd/cinelog.service`.
//!
//! Frozen entry points (called from `main.rs` and `search::index`): [`run`], [`compress`],
//! [`meta`] and [`open_engine`]. The bodies are the pre-integration stand-ins of the
//! contracts commit.

use std::process::ExitCode;

use cinelog_index::{IndexEngine, IndexError};

use crate::cli::{CompressArgs, MetaArgs, ServeArgs};
use crate::ctx::Ctx;

pub fn run(args: &ServeArgs) -> ExitCode {
    let _ = args;
    unavailable("serve")
}

pub fn compress(args: &CompressArgs) -> ExitCode {
    let _ = args;
    unavailable("compress")
}

pub fn meta(args: &MetaArgs) -> ExitCode {
    let _ = args;
    unavailable("meta")
}

/// The one-shot discovery and backfill engine of `ctx`'s scope (takes the writer lock).
pub fn open_engine(ctx: &Ctx) -> Result<Box<dyn IndexEngine>, IndexError> {
    let _ = ctx;
    Err(IndexError::Unavailable("the index engine"))
}

fn unavailable(what: &str) -> ExitCode {
    eprintln!("cinelog: `{what}` is not available in this build yet");
    ExitCode::from(2)
}
