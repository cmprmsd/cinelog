use std::path::PathBuf;

/// Error type of every `cinelog-index` API.
#[derive(Debug, thiserror::Error)]
pub enum IndexError {
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("{0}")]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Cast(#[from] cinelog_cast::CastError),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    /// The database was written by a newer cinelog.
    #[error("{path}: schema version {found} is newer than this cinelog supports ({supported})")]
    NewerSchema { path: PathBuf, found: u32, supported: u32 },
    /// A read-only open found a database that still needs migrating (or does not exist).
    #[error("{path}: schema version {found}, expected {expected}; run `cinelog index` or `cinelog serve`")]
    SchemaMismatch { path: PathBuf, found: u32, expected: u32 },
    #[error("schema: {0}")]
    Schema(String),
    /// The single-writer lock (`index.db.writer.lock`) is held by another process.
    #[error("index writer lock {} is held by another process", .0.display())]
    Locked(PathBuf),
    #[error("not found: {0}")]
    NotFound(String),
    #[error("invalid: {0}")]
    Invalid(String),
    /// A component whose work package has not landed in this build.
    #[error("{0} is not available in this build yet")]
    Unavailable(&'static str),
}

pub type Result<T, E = IndexError> = std::result::Result<T, E>;
