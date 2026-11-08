//! cinelog indexer (milestone M2; plan §6 and §11).
//!
//! Contracts (frozen for M2, owned by M2-0): [`model`] (data types), [`api`] (traits),
//! [`schema`] with `schema.sql`, `meta_schema.sql` and `migrations/`, [`mod@classify`],
//! [`error`] and the in-memory fakes in `fake` (`features = ["fake"]`).
//!
//! | Module | Work package | Implements |
//! |---|---|---|
//! | [`render`] | M2-A render and segmenter | [`api::Transcript`], [`api::PipelineFactory`] for cinelog casts |
//! | [`db`] | M2-B DB writer and query | [`api::IndexWriter`], [`api::Searcher`], [`api::MetaStore`] |
//! | [`legacy`] | M2-C legacy importer | [`api::PipelineFactory`] for legacy casts |
//! | `cinelog::serve` | M2-D serve daemon | [`api::IndexEngine`], discovery, scheduler, compressor |
//!
//! Data flow: discovery ([`classify()`]) → `PipelineFactory::open` → `SessionPipeline::poll`
//! → [`model::IndexBatch`] → `IndexWriter::apply` (one transaction) → index.db, read by
//! `Searcher` connections.

pub mod api;
pub mod classify;
pub mod db;
pub mod error;
#[cfg(any(test, feature = "fake"))]
pub mod fake;
pub mod legacy;
pub mod model;
pub mod render;
pub mod schema;

pub use api::{IndexEngine, IndexWriter, MetaStore, PipelineFactory, Searcher, SessionPipeline, Transcript};
pub use classify::{CastKind, classify};
pub use error::{IndexError, Result};
pub use model::DbPaths;
