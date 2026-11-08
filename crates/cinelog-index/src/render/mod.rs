//! Render and segmenter (work package M2-A; plan §6.4).
//!
//! Provides, with exactly these signatures:
//! - [`transcript`] / [`restore_transcript`]: the avt 0.20 [`Transcript`] (observer that
//!   captures scrolled-off rows, soft-wrap joining, alt-screen snapshots, chunking);
//! - [`cast_pipelines`]: the [`PipelineFactory`] for cinelog casts (tolerant reader over all
//!   rotation parts, clock mapping, the `cmd`/`done` segmenter stack, markers, playback
//!   segments, checkpoints).
//!
//! The bodies below are the pre-integration stand-ins of the contracts commit.

use crate::api::{PipelineFactory, Transcript};
use crate::error::{IndexError, Result};
use crate::model::{RenderOptions, TranscriptState};

/// A fresh transcript for a `cols`×`rows` terminal.
pub fn transcript(cols: u16, rows: u16, opts: RenderOptions) -> Result<Box<dyn Transcript>> {
    let _ = (cols, rows, opts);
    Err(IndexError::Unavailable("render::transcript"))
}

/// A transcript resumed from [`Transcript::snapshot`].
pub fn restore_transcript(state: &TranscriptState, opts: RenderOptions) -> Result<Box<dyn Transcript>> {
    let _ = (state, opts);
    Err(IndexError::Unavailable("render::restore_transcript"))
}

/// Pipelines for cinelog casts (those that [`crate::classify()`] reports as `Cinelog`).
pub fn cast_pipelines(opts: RenderOptions) -> Result<Box<dyn PipelineFactory>> {
    let _ = opts;
    Err(IndexError::Unavailable("render::cast_pipelines"))
}
