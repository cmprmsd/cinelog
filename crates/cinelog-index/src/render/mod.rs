//! Render and segmenter (work package M2-A; plan §6.4).
//!
//! - [`transcript`] / [`restore_transcript`]: the avt 0.20 [`Transcript`] (an observer that
//!   captures rows scrolled off the primary screen, soft-wrap joining, alt-screen
//!   snapshots, chunking, resumable through `Terminal::dump()`);
//! - [`cast_pipelines`]: the [`PipelineFactory`] for cinelog casts (tolerant reader over
//!   all rotation parts, clock mapping, the `cmd`/`done` segmenter stack, markers,
//!   playback segments, checkpoints).

mod chunker;
mod exact;
#[cfg(test)]
mod golden;
mod pipeline;
mod segmenter;
mod transcript;

use crate::api::{PipelineFactory, Transcript};
use crate::error::Result;
use crate::model::{RenderOptions, TranscriptState};

/// A fresh transcript for a `cols`×`rows` terminal.
pub fn transcript(cols: u16, rows: u16, opts: RenderOptions) -> Result<Box<dyn Transcript>> {
    Ok(Box::new(transcript::AvtTranscript::new(cols, rows, opts)?))
}

/// A transcript resumed from [`Transcript::snapshot`].
pub fn restore_transcript(state: &TranscriptState, opts: RenderOptions) -> Result<Box<dyn Transcript>> {
    Ok(Box::new(transcript::AvtTranscript::restore(state, opts)?))
}

/// Pipelines for cinelog casts (those that [`crate::classify()`] reports as `Cinelog`).
pub fn cast_pipelines(opts: RenderOptions) -> Result<Box<dyn PipelineFactory>> {
    Ok(Box::new(pipeline::CastPipelines { opts }))
}
