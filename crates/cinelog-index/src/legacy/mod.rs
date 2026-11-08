//! Legacy importer (work package M2-C; plan §11).
//!
//! [`pipelines`] is the [`PipelineFactory`] for casts that [`crate::classify()`] reports as
//! `Legacy`. Each pipeline pairs the cast with `<cast>.commands.log`, reconstructs command
//! boundaries from the old plugin's `Begin`/`End` banners (reading the command line through
//! [`Transcript::lines_above_cursor`] and the window title set right after `Begin`), aligns
//! them with the log by Needleman–Wunsch, emits `approx = 1` commands with their confidence,
//! and returns the [`crate::model::LegacyImportRow`] in its final batch.
//!
//! Read-only by construction: files are opened with [`crate::classify::open_noatime`] and
//! nothing is ever written next to them.
//!
//! Legacy specifics of the emitted rows:
//! - `typed` is the log text of the matched entry, else the text reconstructed from the
//!   screen (`confidence` NULL); logged commands without a banner (`SHOW_START_END_TIME=0`)
//!   are not commands (§11 step 7), they only count in `log_lines - aligned`;
//! - `expanded`, `exit` and `cwd` are NULL; banner lines are removed from chunk text;
//! - `counters.bad_markers` counts banners rejected as replayed text (implausible wall
//!   clock) plus banners failing the clock cross-check (drift above ±2 s beyond whole-hour
//!   steps).

mod align;
mod banner;
mod log;
mod pipeline;
mod sim;
mod source;

pub use source::log_path as commands_log_path;

#[cfg(test)]
mod tests;

use std::path::Path;

use crate::api::{PipelineFactory, SessionPipeline, Transcript};
use crate::error::Result;
use crate::model::{Checkpoint, LegacyOptions, RenderOptions, TranscriptState};

/// How pipelines create and resume their [`Transcript`].
#[derive(Debug, Clone, Copy)]
pub struct TranscriptSource {
    pub new: fn(u16, u16, RenderOptions) -> Result<Box<dyn Transcript>>,
    pub restore: fn(&TranscriptState, RenderOptions) -> Result<Box<dyn Transcript>>,
}

impl TranscriptSource {
    /// The avt renderer of [`crate::render`].
    pub const RENDER: Self = Self {
        new: crate::render::transcript,
        restore: crate::render::restore_transcript,
    };
}

pub fn pipelines(opts: LegacyOptions) -> Result<Box<dyn PipelineFactory>> {
    // Fail once here rather than per file when the renderer is unavailable.
    (TranscriptSource::RENDER.new)(80, 24, opts.render)?;
    Ok(pipelines_with(opts, TranscriptSource::RENDER))
}

/// [`pipelines`] with another transcript implementation (tests, benchmarks).
pub fn pipelines_with(opts: LegacyOptions, transcript: TranscriptSource) -> Box<dyn PipelineFactory> {
    Box::new(Factory { opts, transcript })
}

struct Factory {
    opts: LegacyOptions,
    transcript: TranscriptSource,
}

impl PipelineFactory for Factory {
    fn open(&self, cast: &Path, checkpoint: Option<Checkpoint>) -> Result<Box<dyn SessionPipeline>> {
        Ok(Box::new(pipeline::LegacyPipeline::open(
            &self.opts,
            self.transcript,
            cast,
            checkpoint,
        )?))
    }
}
