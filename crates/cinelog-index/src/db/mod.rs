//! DB writer and queries (work package M2-B; plan §6.2–§6.5).
//!
//! Provides, with exactly these signatures:
//! - [`open_writer`]: the single [`IndexWriter`]; takes the flock on
//!   `DbPaths::writer_lock` (non-blocking, [`IndexError::Locked`] when held) and keeps it
//!   for its lifetime; opens with [`crate::schema::open`] (`ReadWrite`);
//! - [`open_reader`]: a read-only [`Searcher`] (FTS5 trigram, `LIKE` without `ESCAPE`, the
//!   Rust post-filter for `%`/`_`, `GLOB` for case-sensitive search, the 500 ms newest-first
//!   scan for terms under 3 characters, recency order with the `(ts, id)` cursor, the 5000
//!   candidate cap, snippets with the U+E000/U+E001 sentinels, `meta.forgotten` excluded);
//! - [`open_meta`]: the [`MetaStore`] over meta.db.
//!
//! Redaction is off in both scopes (§15.7): `index.redact` stays an optional, empty hook.
//!
//! The bodies below are the pre-integration stand-ins of the contracts commit.

use crate::api::{IndexWriter, MetaStore, Searcher};
use crate::error::{IndexError, Result};
use crate::model::DbPaths;
use cinelog_core::config::IndexConfig;

pub fn open_writer(paths: &DbPaths, cfg: &IndexConfig) -> Result<Box<dyn IndexWriter>> {
    let _ = (paths, cfg);
    Err(IndexError::Unavailable("db::open_writer"))
}

pub fn open_reader(paths: &DbPaths) -> Result<Box<dyn Searcher>> {
    let _ = paths;
    Err(IndexError::Unavailable("db::open_reader"))
}

pub fn open_meta(paths: &DbPaths) -> Result<Box<dyn MetaStore>> {
    let _ = paths;
    Err(IndexError::Unavailable("db::open_meta"))
}
