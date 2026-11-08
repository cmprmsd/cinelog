//! Read-only inspection of `index.db` for the M2 scenarios.
//!
//! [`Dump`] is a canonical image of the derived tables: surrogate ids are replaced by natural
//! keys (session uuid, command seq), volatile columns are left out and rows are sorted, so
//! two databases built in a different order (a resumed backfill and a one-shot run, I12)
//! compare equal exactly when their content is equal.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use rusqlite::types::ValueRef;
use rusqlite::{Connection, OpenFlags};
use sha2::{Digest, Sha256};

use crate::util::hex;

/// Columns that never take part in a comparison: surrogate keys (replaced by natural keys)
/// and bookkeeping times.
const ALWAYS_SKIPPED: &[&str] = &["id", "session_id", "command_id", "updated_at"];

/// Columns (and, as `table.*`, tables) that describe where and how a cast is stored rather
/// than what it contains; I13 leaves them out to compare an index before and after
/// compression.
pub const STORAGE_COLUMNS: &[&str] = &[
    "sessions.cast_path",
    "sessions.compressed",
    "cast_parts.path",
    "cast_parts.compressed",
    "index_state.file_size",
    "index_state.file_mtime",
    "index_state.file_ino",
    "zst_frames.*",
];

/// The compared tables: name and the query that yields their rows with natural keys first.
const TABLES: &[(&str, &str)] = &[
    ("sessions", "SELECT t.* FROM sessions t"),
    (
        "cast_parts",
        "SELECT s.uuid AS session_uuid, t.* FROM cast_parts t JOIN sessions s ON s.id = t.session_id",
    ),
    (
        "index_state",
        "SELECT s.uuid AS session_uuid, t.* FROM index_state t JOIN sessions s ON s.id = t.session_id",
    ),
    (
        "commands",
        "SELECT s.uuid AS session_uuid, t.* FROM commands t JOIN sessions s ON s.id = t.session_id",
    ),
    (
        "outputs",
        "SELECT s.uuid AS session_uuid, c.seq AS command_seq, t.* FROM outputs t \
         JOIN sessions s ON s.id = t.session_id LEFT JOIN commands c ON c.id = t.command_id",
    ),
    (
        "markers",
        "SELECT s.uuid AS session_uuid, t.* FROM markers t JOIN sessions s ON s.id = t.session_id",
    ),
    (
        "segments",
        "SELECT s.uuid AS session_uuid, t.* FROM segments t JOIN sessions s ON s.id = t.session_id",
    ),
    (
        "zst_frames",
        "SELECT s.uuid AS session_uuid, t.* FROM zst_frames t JOIN sessions s ON s.id = t.session_id",
    ),
    (
        "legacy_import",
        "SELECT s.uuid AS session_uuid, t.* FROM legacy_import t LEFT JOIN sessions s ON s.id = t.session_id",
    ),
];

/// One SQL value as canonical text (reals in their exact shortest form, blobs by hash).
fn value_text(v: ValueRef<'_>) -> String {
    match v {
        ValueRef::Null => "NULL".into(),
        ValueRef::Integer(i) => i.to_string(),
        ValueRef::Real(r) => format!("{r:?}"),
        ValueRef::Text(t) => serde_json::to_string(&String::from_utf8_lossy(t)).expect("string"),
        ValueRef::Blob(b) => format!("blob:{}:{}", b.len(), hex(&Sha256::digest(b))),
    }
}

/// A read-only connection to an index (or a copy of one).
#[derive(Debug)]
pub struct IndexDb {
    conn: Connection,
    pub path: PathBuf,
}

impl IndexDb {
    /// Opens `path` read-only; the writer may keep running (WAL).
    pub fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX | OpenFlags::SQLITE_OPEN_URI,
        )
        .with_context(|| format!("opening {} read-only", path.display()))?;
        conn.busy_timeout(Duration::from_secs(5))?;
        Ok(Self {
            conn,
            path: path.to_path_buf(),
        })
    }

    pub fn conn(&self) -> &Connection {
        &self.conn
    }

    /// The single integer `sql` yields (`NULL` → 0).
    pub fn int(&self, sql: &str) -> Result<i64> {
        self.conn
            .query_row(sql, [], |r| r.get::<_, Option<i64>>(0))
            .map(Option::unwrap_or_default)
            .with_context(|| format!("{sql} on {}", self.path.display()))
    }

    /// Rows of `sql` as canonical text, one `Vec` per row.
    pub fn rows(&self, sql: &str, params: impl rusqlite::Params) -> Result<Vec<Vec<String>>> {
        let mut st = self.conn.prepare(sql).with_context(|| sql.to_owned())?;
        let n = st.column_count();
        let rows = st.query_map(params, |r| (0..n).map(|i| r.get_ref(i).map(value_text)).collect())?;
        rows.collect::<rusqlite::Result<_>>().context("reading rows")
    }

    /// Text columns of `sql` (`NULL` → `None`).
    pub fn strings(&self, sql: &str, params: impl rusqlite::Params) -> Result<Vec<Vec<Option<String>>>> {
        let mut st = self.conn.prepare(sql).with_context(|| sql.to_owned())?;
        let n = st.column_count();
        let rows = st.query_map(params, |r| {
            (0..n)
                .map(|i| {
                    Ok(match r.get_ref(i)? {
                        ValueRef::Null => None,
                        ValueRef::Text(t) => Some(String::from_utf8_lossy(t).into_owned()),
                        other => Some(value_text(other)),
                    })
                })
                .collect()
        })?;
        rows.collect::<rusqlite::Result<_>>().context("reading rows")
    }

    /// The canonical image of every compared table, without the columns in `skip`
    /// (`table.column`, or `table.*` for a whole table).
    pub fn dump(&self, skip: &[&str]) -> Result<Dump> {
        let mut tables = BTreeMap::new();
        for (name, sql) in TABLES {
            if skip.contains(&format!("{name}.*").as_str()) {
                continue;
            }
            let mut st = self.conn.prepare(sql).with_context(|| sql.to_string())?;
            let keep: Vec<(usize, String)> = st
                .column_names()
                .enumerate()
                .filter(|(_, c)| !ALWAYS_SKIPPED.contains(c) && !skip.contains(&format!("{name}.{c}").as_str()))
                .map(|(i, c)| (i, (*c).to_owned()))
                .collect();
            let mut rows: Vec<String> = st
                .query_map([], |r| {
                    let mut line = String::new();
                    for (i, c) in &keep {
                        let _ = write!(line, "{c}={} ", value_text(r.get_ref(*i)?));
                    }
                    Ok(line.trim_end().to_owned())
                })?
                .collect::<rusqlite::Result<_>>()?;
            rows.sort();
            tables.insert((*name).to_owned(), rows);
        }
        Ok(Dump { tables })
    }
}

/// Canonical rows per table (see the module docs).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dump {
    pub tables: BTreeMap<String, Vec<String>>,
}

impl Dump {
    /// sha256 over every table's rows.
    pub fn hash(&self) -> String {
        let mut h = Sha256::new();
        for (name, rows) in &self.tables {
            h.update(name.as_bytes());
            h.update([0]);
            for r in rows {
                h.update(r.as_bytes());
                h.update(*b"\n");
            }
        }
        hex(&h.finalize())
    }

    pub fn count(&self, table: &str) -> usize {
        self.tables.get(table).map_or(0, Vec::len)
    }

    /// Row counts per table, for messages.
    pub fn summary(&self) -> String {
        self.tables
            .iter()
            .map(|(t, r)| format!("{t}={}", r.len()))
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// `None` when equal; otherwise, per differing table, the rows only one side has
    /// (at most `max` each, shortened).
    pub fn diff(&self, other: &Self, max: usize) -> Option<String> {
        let short = |s: &str| {
            if s.len() > 400 {
                let mut cut = 400;
                while !s.is_char_boundary(cut) {
                    cut -= 1;
                }
                format!("{}…", &s[..cut])
            } else {
                s.to_owned()
            }
        };
        let empty = Vec::new();
        let mut out = String::new();
        let names: std::collections::BTreeSet<&String> = self.tables.keys().chain(other.tables.keys()).collect();
        for name in names {
            let a = self.tables.get(name).unwrap_or(&empty);
            let b = other.tables.get(name).unwrap_or(&empty);
            if a == b {
                continue;
            }
            let only = |x: &[String], y: &[String]| -> Vec<String> {
                let ys: std::collections::HashSet<&String> = y.iter().collect();
                x.iter()
                    .filter(|r| !ys.contains(r))
                    .take(max)
                    .map(|r| short(r))
                    .collect()
            };
            let _ = writeln!(out, "table {name}: {} vs {} rows", a.len(), b.len());
            for r in only(a, b) {
                let _ = writeln!(out, "  - {r}");
            }
            for r in only(b, a) {
                let _ = writeln!(out, "  + {r}");
            }
        }
        (!out.is_empty()).then_some(out)
    }
}

/// Copies `db` (with its `-wal`) into `dir` and runs the FTS5 `integrity-check` with content
/// verification (`rank = 1`) on both full-text tables of the copy. The writer must be idle.
pub fn fts_integrity(db: &Path, dir: &Path) -> Result<()> {
    let copy = dir.join("fts-check.db");
    for suffix in ["", "-wal"] {
        let src = PathBuf::from(format!("{}{suffix}", db.display()));
        let dst = PathBuf::from(format!("{}{suffix}", copy.display()));
        let _ = std::fs::remove_file(&dst);
        if src.exists() {
            std::fs::copy(&src, &dst).with_context(|| format!("copying {}", src.display()))?;
        }
    }
    let conn = Connection::open(&copy)?;
    for t in ["cmd_fts", "out_fts"] {
        conn.execute(&format!("INSERT INTO {t}({t}, rank) VALUES('integrity-check', 1)"), [])
            .with_context(|| format!("FTS integrity-check of {t} in {}", db.display()))?;
    }
    drop(conn);
    for suffix in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{}{suffix}", copy.display()));
    }
    Ok(())
}

/// Removes `index.db` and its WAL files (a fresh one-shot run starts from nothing).
pub fn remove_index(db: &Path) -> Result<()> {
    for suffix in ["", "-wal", "-shm"] {
        let p = PathBuf::from(format!("{}{suffix}", db.display()));
        match std::fs::remove_file(&p) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                return Err(e).with_context(|| format!("removing {}", p.display()));
            }
            _ => {}
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(dir: &Path, name: &str, order: &[(i64, &str, i64)]) -> PathBuf {
        let p = dir.join(name);
        let c = Connection::open(&p).unwrap();
        c.execute_batch(
            "PRAGMA journal_mode=WAL;
             CREATE TABLE sessions(id INTEGER PRIMARY KEY, uuid TEXT, started_at REAL, cast_path TEXT, compressed INT);
             CREATE TABLE cast_parts(session_id INT, part INT, path TEXT, compressed INT);
             CREATE TABLE index_state(session_id INT, part INT, updated_at REAL, file_size INT, file_mtime REAL, file_ino INT);
             CREATE TABLE commands(id INTEGER PRIMARY KEY, session_id INT, seq INT, typed TEXT, confidence REAL);
             CREATE TABLE outputs(id INTEGER PRIMARY KEY, session_id INT, command_id INT, t REAL, text TEXT);
             CREATE TABLE markers(id INTEGER PRIMARY KEY, session_id INT, t REAL, kind TEXT);
             CREATE TABLE segments(session_id INT, seq INT, vt_dump BLOB);
             CREATE TABLE zst_frames(session_id INT, part INT, seq INT);
             CREATE TABLE legacy_import(path TEXT, session_id INT, updated_at REAL);
             CREATE VIRTUAL TABLE cmd_fts USING fts5(typed, content='commands', content_rowid='id', tokenize='trigram', detail='none');
             CREATE VIRTUAL TABLE out_fts USING fts5(text, content='outputs', content_rowid='id', tokenize='trigram', detail='none');",
        )
        .unwrap();
        for &(sid, uuid, path_n) in order {
            c.execute(
                "INSERT INTO sessions VALUES(?1, ?2, 0.1, ?3, 0)",
                rusqlite::params![sid, uuid, format!("/x/{path_n}.cast")],
            )
            .unwrap();
            c.execute(
                "INSERT INTO commands(session_id, seq, typed, confidence) VALUES(?1, 1, 'ls', 0.25)",
                [sid],
            )
            .unwrap();
            let cid = c.last_insert_rowid();
            c.execute("INSERT INTO cmd_fts(rowid, typed) VALUES(?1, 'ls')", [cid])
                .unwrap();
            c.execute(
                "INSERT INTO outputs(session_id, command_id, t, text) VALUES(?1, ?2, 1.5, 'out')",
                [sid, cid],
            )
            .unwrap();
            let oid = c.last_insert_rowid();
            c.execute("INSERT INTO out_fts(rowid, text) VALUES(?1, 'out')", [oid])
                .unwrap();
            c.execute("INSERT INTO segments VALUES(?1, 0, x'00ff')", [sid]).unwrap();
            c.execute(
                "INSERT INTO index_state VALUES(?1, 1, ?2, 10, 1.0, 5)",
                rusqlite::params![sid, sid as f64],
            )
            .unwrap();
        }
        p
    }

    #[test]
    fn dumps_ignore_ids_and_order_but_not_content() {
        let d = crate::util::scratch_dir().unwrap();
        let a = fixture(d.path(), "a.db", &[(1, "u1", 1), (2, "u2", 2)]);
        let b = fixture(d.path(), "b.db", &[(7, "u2", 2), (3, "u1", 1)]);
        let c = fixture(d.path(), "c.db", &[(1, "u1", 9), (2, "u2", 2)]);
        let (da, db, dc) = (
            IndexDb::open(&a).unwrap().dump(&[]).unwrap(),
            IndexDb::open(&b).unwrap().dump(&[]).unwrap(),
            IndexDb::open(&c).unwrap().dump(&[]).unwrap(),
        );
        assert_eq!(da.hash(), db.hash(), "{:?}", da.diff(&db, 5));
        assert!(da.diff(&db, 5).is_none());
        assert_ne!(da.hash(), dc.hash());
        let diff = da.diff(&dc, 5).unwrap();
        assert!(diff.contains("table sessions") && diff.contains("/x/9.cast"), "{diff}");
        let skip_storage = |p: &Path| IndexDb::open(p).unwrap().dump(STORAGE_COLUMNS).unwrap();
        assert_eq!(skip_storage(&a), skip_storage(&c), "storage columns are skipped");
        assert!(!skip_storage(&a).tables.contains_key("zst_frames"));
        assert_eq!(da.count("outputs"), 2);
        assert!(da.tables["outputs"][0].starts_with("session_uuid=\"u1\" command_seq=1 "));
        assert!(da.tables["segments"][0].contains("vt_dump=blob:2:"));
        assert!(da.summary().contains("commands=2"));
    }

    #[test]
    fn queries_and_fts_check() {
        let d = crate::util::scratch_dir().unwrap();
        let a = fixture(d.path(), "a.db", &[(1, "u1", 1)]);
        let db = IndexDb::open(&a).unwrap();
        assert_eq!(db.int("SELECT count(*) FROM commands").unwrap(), 1);
        assert_eq!(db.int("SELECT max(id) FROM markers").unwrap(), 0);
        assert_eq!(
            db.rows("SELECT typed, confidence, NULL FROM commands WHERE seq = ?1", [1])
                .unwrap(),
            vec![vec!["\"ls\"".to_owned(), "0.25".into(), "NULL".into()]]
        );
        assert_eq!(
            db.strings("SELECT typed, NULL FROM commands", []).unwrap(),
            vec![vec![Some("ls".to_owned()), None]]
        );
        fts_integrity(&a, d.path()).unwrap();
        remove_index(&a).unwrap();
        assert!(!a.exists());
        remove_index(&a).unwrap();
    }
}
