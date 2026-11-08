//! Legacy importer (work package M2-C; plan §11).
//!
//! Provides, with exactly this signature, [`pipelines`]: the [`PipelineFactory`] for casts
//! that [`crate::classify()`] reports as `Legacy`. Each pipeline pairs the cast with
//! `<cast>.commands.log`, reconstructs command boundaries from the `Begin`/`End` banners
//! (reading the command line through [`crate::api::Transcript::lines_above_cursor`]),
//! aligns them with the log by Needleman–Wunsch, emits `approx = 1` commands with their
//! confidence, and returns the [`crate::model::LegacyImportRow`] in its final batch. Files
//! are opened with [`crate::classify::open_noatime`]; nothing is ever written next to them.
//!
//! The body below is the pre-integration stand-in of the contracts commit.

use crate::api::PipelineFactory;
use crate::error::{IndexError, Result};
use crate::model::LegacyOptions;

pub fn pipelines(opts: LegacyOptions) -> Result<Box<dyn PipelineFactory>> {
    let _ = opts;
    Err(IndexError::Unavailable("legacy::pipelines"))
}
