//! DB writer and queries (work package M2-B; plan §6.2–§6.5, §7.6).
//!
//! - [`open_writer`]: the single [`IndexWriter`]. Takes the flock on `DbPaths::writer_lock`
//!   (non-blocking, [`IndexError::Locked`] when held) and keeps it for its lifetime.
//! - [`open_reader`]: a read-only [`Searcher`] implementing the §6.5 query rules (see
//!   `query`): FTS5 trigram `LIKE` without `ESCAPE` (per column for `cmd_fts`), the Rust
//!   post-filter, `GLOB` for case-sensitive search, the 500 ms newest-first scan for terms the
//!   index cannot serve, recency order with the `(ts, kind, id)` cursor, the candidate cap,
//!   snippets with the U+E000/U+E001 sentinels and `meta.forgotten` excluded at query time.
//! - [`open_meta`]: the [`MetaStore`] over meta.db.
//!
//! Redaction is off in both scopes (§15.7): nothing is redacted unless the user lists
//! regexes in `index.redact` themselves; no default set exists.

mod forget;
mod meta;
mod query;
mod text;
mod writer;

#[cfg(test)]
mod tests;

use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use rusqlite::types::{FromSql, FromSqlError, FromSqlResult, ToSqlOutput, Value, ValueRef};
use rusqlite::{Connection, Row, ToSql};

use crate::api::{IndexWriter, MetaStore, Searcher};
use crate::error::{IndexError, Result};
use crate::model::{DbPaths, FileStamp, Forgotten, LegacyImportRow};
use crate::schema::{self, Access, DbKind};
use cinelog_core::config::IndexConfig;

/// `sessions.attribution` of a forgotten session's tombstone: a row stripped of its content
/// and of who ran it, kept only so discovery sees the cast as indexed (`known_casts`).
const TOMBSTONE: &str = "forgotten";

pub fn open_writer(paths: &DbPaths, cfg: &IndexConfig) -> Result<Box<dyn IndexWriter>> {
    Ok(Box::new(writer::SqliteWriter::open(paths, cfg)?))
}

pub fn open_reader(paths: &DbPaths) -> Result<Box<dyn Searcher>> {
    Ok(Box::new(query::SqliteSearcher::open(paths)?))
}

pub fn open_meta(paths: &DbPaths) -> Result<Box<dyn MetaStore>> {
    Ok(Box::new(meta::SqliteMeta::open(&paths.meta)?))
}

/// The forgotten list of an existing meta.db, read through a read-only connection. A missing
/// or never initialised meta.db has nothing forgotten.
fn read_meta_forgotten(path: &Path) -> Result<Option<(Connection, Vec<Forgotten>)>> {
    if !path.exists() {
        return Ok(None);
    }
    match schema::open(path, DbKind::Meta, Access::ReadOnly) {
        Ok(conn) => {
            let list = meta::read_forgotten(&conn)?;
            Ok(Some((conn, list)))
        }
        Err(IndexError::SchemaMismatch { found: 0, .. }) => Ok(None),
        Err(e) => Err(e),
    }
}

/// The current epoch time in whole microseconds (the schema's ts precision). Such a value
/// survives a JSON round trip exactly (meta export and import): serde_json parses it as an
/// integer divided by 10^6, which is correctly rounded, while a full-precision `f64` can come
/// back one ULP off without serde_json's `float_roundtrip`.
fn now() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_micros() as f64 / 1e6)
        .unwrap_or(0.0)
}

/// Paths are stored as TEXT when they are UTF-8 and as BLOB (raw bytes) otherwise, so
/// non-UTF-8 file names keep their identity.
fn path_value(p: &Path) -> Value {
    match p.to_str() {
        Some(s) => Value::Text(s.to_owned()),
        None => Value::Blob(p.as_os_str().as_bytes().to_vec()),
    }
}

fn path_at(row: &Row<'_>, idx: usize) -> rusqlite::Result<PathBuf> {
    Ok(match row.get_ref(idx)? {
        ValueRef::Text(b) | ValueRef::Blob(b) => PathBuf::from(OsStr::from_bytes(b)),
        ValueRef::Null => PathBuf::new(),
        other => {
            return Err(rusqlite::Error::InvalidColumnType(
                idx,
                "path".into(),
                other.data_type(),
            ));
        }
    })
}

fn opt_path_at(row: &Row<'_>, idx: usize) -> rusqlite::Result<Option<PathBuf>> {
    Ok(match row.get_ref(idx)? {
        ValueRef::Null => None,
        _ => Some(path_at(row, idx)?),
    })
}

/// UTF-8 text stored in a BLOB column (`vt_dump`).
fn utf8_at(row: &Row<'_>, idx: usize) -> rusqlite::Result<String> {
    match row.get_ref(idx)? {
        ValueRef::Null => Ok(String::new()),
        ValueRef::Text(b) | ValueRef::Blob(b) => String::from_utf8(b.to_vec())
            .map_err(|e| rusqlite::Error::FromSqlConversionFailure(idx, rusqlite::types::Type::Blob, Box::new(e))),
        other => Err(rusqlite::Error::InvalidColumnType(
            idx,
            "utf8".into(),
            other.data_type(),
        )),
    }
}

/// Parses a TEXT enum column (`state`, `kind`, `scope`, …).
fn parse_at<T: std::str::FromStr>(row: &Row<'_>, idx: usize) -> rusqlite::Result<T>
where
    T::Err: std::error::Error + Send + Sync + 'static,
{
    let s: String = row.get(idx)?;
    s.parse()
        .map_err(|e| rusqlite::Error::FromSqlConversionFailure(idx, rusqlite::types::Type::Text, Box::new(e)))
}

/// `legacy_import` with the session uuid, in the column order of [`legacy_from_row`].
const LEGACY_SELECT: &str = "SELECT l.path, l.size, l.mtime, l.ino, l.sha256, l.commands_log, l.status, s.uuid,
    l.banners, l.log_lines, l.aligned, l.mean_conf, l.error
    FROM legacy_import l LEFT JOIN sessions s ON s.id = l.session_id";

fn legacy_from_row(r: &Row<'_>) -> rusqlite::Result<LegacyImportRow> {
    Ok(LegacyImportRow {
        path: path_at(r, 0)?,
        file: FileStamp {
            size: opt_u64_at(r, 1)?.unwrap_or(0),
            mtime: r.get::<_, Option<f64>>(2)?.unwrap_or(0.0),
            ino: r.get::<_, Option<i64>>(3)?.map_or(0, ino_from_db),
        },
        sha256: r.get::<_, Option<String>>(4)?.unwrap_or_default(),
        commands_log: opt_path_at(r, 5)?,
        status: parse_at(r, 6)?,
        session_uuid: r.get(7)?,
        banners: opt_u64_at(r, 8)?.unwrap_or(0),
        log_lines: opt_u64_at(r, 9)?.unwrap_or(0),
        aligned: opt_u64_at(r, 10)?.unwrap_or(0),
        mean_conf: r.get(11)?,
        error: r.get(12)?,
    })
}

/// Unsigned columns (seq, offsets, counters). SQLite integers are i64 and rusqlite maps `u64`
/// only with its `fallible_uint` feature, so they go through this checked wrapper: negative
/// values and values above `i64::MAX` are conversion errors, never silently wrapped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct U64(u64);

impl ToSql for U64 {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        i64::try_from(self.0)
            .map(ToSqlOutput::from)
            .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))
    }
}

impl FromSql for U64 {
    fn column_result(v: ValueRef<'_>) -> FromSqlResult<Self> {
        let i = i64::column_result(v)?;
        u64::try_from(i).map(U64).map_err(|_| FromSqlError::OutOfRange(i))
    }
}

/// Bind helper for `Option<u64>` parameters.
fn opt_u64(v: Option<u64>) -> Option<U64> {
    v.map(U64)
}

fn u64_at(row: &Row<'_>, idx: usize) -> rusqlite::Result<u64> {
    Ok(row.get::<_, U64>(idx)?.0)
}

fn opt_u64_at(row: &Row<'_>, idx: usize) -> rusqlite::Result<Option<u64>> {
    Ok(row.get::<_, Option<U64>>(idx)?.map(|v| v.0))
}

/// Inode numbers are full u64; SQLite integers are i64. Stored bit-for-bit.
fn ino_to_db(ino: u64) -> i64 {
    ino as i64
}

fn ino_from_db(v: i64) -> u64 {
    v as u64
}
