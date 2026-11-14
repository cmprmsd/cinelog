//! Opening and migrating index.db and meta.db (plan §6.1–§6.3).
//!
//! `schema.sql` / `meta_schema.sql` are the full current schemas, applied to an empty
//! database. Upgrades from older versions are the files in `migrations/<db>/`, listed in
//! [`INDEX_MIGRATIONS`] / [`META_MIGRATIONS`] (see `migrations/README.md`).

use std::fs::{DirBuilder, OpenOptions};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::Path;
use std::time::Duration;

use rusqlite::{Connection, OpenFlags};

use crate::error::{IndexError, Result};

pub const INDEX_SCHEMA_VERSION: u32 = 2;
pub const META_SCHEMA_VERSION: u32 = 1;

pub const INDEX_SCHEMA: &str = include_str!("schema.sql");
pub const META_SCHEMA: &str = include_str!("meta_schema.sql");

pub const BUSY_TIMEOUT: Duration = Duration::from_millis(5000);

/// One upgrade step from version `to - 1` to `to`.
#[derive(Debug, Clone, Copy)]
pub struct Migration {
    pub to: u32,
    pub sql: &'static str,
}

/// index.db upgrades.
pub const INDEX_MIGRATIONS: &[Migration] = &[Migration {
    to: 2,
    sql: include_str!("migrations/index/0002_tmux_pane_index.sql"),
}];
/// meta.db upgrades (none yet).
pub const META_MIGRATIONS: &[Migration] = &[];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DbKind {
    Index,
    Meta,
}

impl DbKind {
    /// `PRAGMA application_id`, so index.db and meta.db cannot be mixed up.
    pub const fn application_id(self) -> i32 {
        match self {
            Self::Index => 0x434c_4958, // "CLIX"
            Self::Meta => 0x434c_4d54,  // "CLMT"
        }
    }

    pub const fn version(self) -> u32 {
        match self {
            Self::Index => INDEX_SCHEMA_VERSION,
            Self::Meta => META_SCHEMA_VERSION,
        }
    }

    pub const fn name(self) -> &'static str {
        match self {
            Self::Index => "index",
            Self::Meta => "meta",
        }
    }

    pub const fn schema(self) -> &'static str {
        match self {
            Self::Index => INDEX_SCHEMA,
            Self::Meta => META_SCHEMA,
        }
    }

    pub const fn migrations(self) -> &'static [Migration] {
        match self {
            Self::Index => INDEX_MIGRATIONS,
            Self::Meta => META_MIGRATIONS,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    /// Creates the file (0600, parent 0700) and migrates.
    ReadWrite,
    /// Never writes; fails unless the schema is current or newer (plan §10.3: a binary that
    /// finds a newer `user_version` reads it and says so, see [`newer_note`]).
    ReadOnly,
}

/// Opens a database with the §6.1 pragmas (`WAL`, `synchronous=NORMAL`, `foreign_keys=ON`,
/// `busy_timeout=5000`), migrating it in `ReadWrite` mode.
pub fn open(path: &Path, kind: DbKind, access: Access) -> Result<Connection> {
    let conn = match access {
        Access::ReadWrite => {
            if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
                DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
            }
            OpenOptions::new().create(true).append(true).mode(0o600).open(path)?;
            Connection::open_with_flags(
                path,
                OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX | OpenFlags::SQLITE_OPEN_URI,
            )?
        }
        Access::ReadOnly => Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX | OpenFlags::SQLITE_OPEN_URI,
        )?,
    };
    configure(&conn, access)?;
    match access {
        Access::ReadWrite => {
            migrate(&conn, kind, path)?;
        }
        Access::ReadOnly => {
            check_application_id(&conn, kind, path)?;
            let found = user_version(&conn)?;
            if found > kind.version() {
                // A database without our application_id is not one we can vouch for.
                let id: i32 = conn.pragma_query_value(None, "application_id", |r| r.get(0))?;
                if id != kind.application_id() {
                    return Err(IndexError::NewerSchema {
                        path: path.to_owned(),
                        found,
                        supported: kind.version(),
                    });
                }
                warn_newer_once(path, kind, found);
                return Ok(conn);
            }
            if found != kind.version() {
                return Err(IndexError::SchemaMismatch {
                    path: path.to_owned(),
                    found,
                    expected: kind.version(),
                });
            }
        }
    }
    Ok(conn)
}

/// The one-line notice for a database of `kind` at schema `found` when that is newer than
/// this binary supports: it is read, never written, and nothing indexes into it.
pub fn newer_note(kind: DbKind, found: u32) -> Option<String> {
    (found > kind.version()).then(|| {
        format!(
            "{}.db has schema version {found}, newer than cinelog {} supports ({}): read-only, not indexing; \
             upgrade cinelog",
            kind.name(),
            env!("CARGO_PKG_VERSION"),
            kind.version(),
        )
    })
}

/// Logs [`newer_note`] once per database file and process (readers open per request).
fn warn_newer_once(path: &Path, kind: DbKind, found: u32) {
    use std::collections::HashSet;
    use std::path::PathBuf;
    use std::sync::{Mutex, OnceLock};
    static SEEN: OnceLock<Mutex<HashSet<PathBuf>>> = OnceLock::new();
    let first = SEEN
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(path.to_owned());
    if first && let Some(note) = newer_note(kind, found) {
        tracing::warn!("{}: {note}", path.display());
    }
}

/// A migrated in-memory database (tests and fakes).
pub fn open_in_memory(kind: DbKind) -> Result<Connection> {
    let conn = Connection::open_in_memory()?;
    configure(&conn, Access::ReadWrite)?;
    migrate(&conn, kind, Path::new(":memory:"))?;
    Ok(conn)
}

/// Per-connection pragmas.
pub fn configure(conn: &Connection, access: Access) -> Result<()> {
    conn.busy_timeout(BUSY_TIMEOUT)?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    if access == Access::ReadWrite {
        // In-memory databases answer "memory"; that is fine.
        let _mode: String = conn.pragma_update_and_check(None, "journal_mode", "WAL", |r| r.get(0))?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
    }
    Ok(())
}

pub fn user_version(conn: &Connection) -> Result<u32> {
    Ok(conn.pragma_query_value(None, "user_version", |r| r.get::<_, u32>(0))?)
}

/// Brings `conn` to `kind.version()`: applies the full schema to an empty database, or the
/// upgrade steps after its current version. Returns the version before migrating.
pub fn migrate(conn: &Connection, kind: DbKind, path: &Path) -> Result<u32> {
    conn.execute_batch("BEGIN IMMEDIATE")?;
    let res = migrate_locked(conn, kind, path);
    match res {
        Ok(_) => conn.execute_batch("COMMIT")?,
        Err(_) => {
            let _ = conn.execute_batch("ROLLBACK");
        }
    }
    res
}

fn check_application_id(conn: &Connection, kind: DbKind, path: &Path) -> Result<()> {
    let id: i32 = conn.pragma_query_value(None, "application_id", |r| r.get(0))?;
    if id != 0 && id != kind.application_id() {
        return Err(IndexError::Schema(format!(
            "{} is not a cinelog {} database",
            path.display(),
            kind.name()
        )));
    }
    Ok(())
}

fn migrate_locked(conn: &Connection, kind: DbKind, path: &Path) -> Result<u32> {
    check_application_id(conn, kind, path)?;
    let found = user_version(conn)?;
    let target = kind.version();
    if found > target {
        return Err(IndexError::NewerSchema {
            path: path.to_owned(),
            found,
            supported: target,
        });
    }
    if found == target {
        return Ok(found);
    }
    if found == 0 {
        let objects: i64 = conn.query_row("SELECT count(*) FROM sqlite_schema", [], |r| r.get(0))?;
        if objects != 0 {
            return Err(IndexError::Schema(format!(
                "{} has tables but no schema version; not a cinelog database",
                path.display()
            )));
        }
        conn.execute_batch(kind.schema())?;
        conn.pragma_update(None, "application_id", kind.application_id())?;
    } else {
        for v in found + 1..=target {
            let step = kind
                .migrations()
                .iter()
                .find(|m| m.to == v)
                .ok_or_else(|| IndexError::Schema(format!("no migration to version {v} for {}", path.display())))?;
            conn.execute_batch(step.sql)?;
        }
    }
    conn.pragma_update(None, "user_version", target)?;
    Ok(found)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn schema_objects(conn: &Connection) -> Vec<(String, String)> {
        let mut st = conn
            .prepare("SELECT name, ifnull(sql, '') FROM sqlite_schema ORDER BY name")
            .unwrap();
        st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .map(|r| r.unwrap())
            .collect()
    }

    #[test]
    fn fresh_databases_get_current_version() {
        for kind in [DbKind::Index, DbKind::Meta] {
            let conn = open_in_memory(kind).unwrap();
            assert_eq!(user_version(&conn).unwrap(), kind.version());
            assert_eq!(migrate(&conn, kind, Path::new("x")).unwrap(), kind.version());
        }
    }

    #[test]
    fn migration_steps_are_contiguous() {
        for kind in [DbKind::Index, DbKind::Meta] {
            let tos: Vec<u32> = kind.migrations().iter().map(|m| m.to).collect();
            let expected: Vec<u32> = (2..=kind.version()).collect();
            assert_eq!(tos, expected, "{kind:?}: every version after 1 needs exactly one step");
        }
    }

    /// Migrating from every released version must end in the same schema as a fresh one.
    #[test]
    fn upgrades_match_fresh_schema() {
        for kind in [DbKind::Index, DbKind::Meta] {
            let fresh = open_in_memory(kind).unwrap();
            for from in 1..kind.version() {
                let conn = Connection::open_in_memory().unwrap();
                conn.execute_batch(released_schema(kind, from)).unwrap();
                conn.pragma_update(None, "user_version", from).unwrap();
                migrate(&conn, kind, Path::new("x")).unwrap();
                assert_eq!(schema_objects(&conn), schema_objects(&fresh), "{kind:?} from {from}");
            }
        }
    }

    /// The full schema as released in `version` (kept for the upgrade test).
    fn released_schema(kind: DbKind, version: u32) -> &'static str {
        match (kind, version) {
            (_, v) if v == kind.version() => kind.schema(),
            (DbKind::Index, 1) => include_str!("migrations/index/v1.sql"),
            _ => panic!("add the released schema of {kind:?} v{version} under migrations/ for this test"),
        }
    }

    /// v2 fills `tmux_pane_index` of the sessions already indexed from their last `ctx` marker.
    #[test]
    fn v2_backfills_the_pane_index() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(released_schema(DbKind::Index, 1)).unwrap();
        conn.pragma_update(None, "user_version", 1).unwrap();
        conn.execute_batch(
            r#"INSERT INTO sessions(id, uuid, host, scope, user, tmux_pane, cast_path, started_at, state) VALUES
                 (1, 'a', 'h', 'personal', 'u', '%3', '/a', 0, 'ended'),
                 (2, 'b', 'h', 'personal', 'u', '%4', '/b', 0, 'ended'),
                 (3, 'c', 'h', 'personal', 'u', NULL, '/c', 0, 'ended');
               INSERT INTO markers(session_id, t, ts, kind, data_json) VALUES
                 (1, 1, 1, 'ctx', '{"pane_index":"0","pane_id":"%3"}'),
                 (1, 2, 2, 'ctx', '{"pane_index":"2","pane_id":"%3"}'),
                 (1, 3, 3, 'pause', '{"pane_index":"9"}'),
                 (2, 1, 1, 'ctx', 'not json'),
                 (3, 1, 1, 'ctx', '{"pane_index":"5"}');"#,
        )
        .unwrap();
        migrate(&conn, DbKind::Index, Path::new("x")).unwrap();
        let mut st = conn
            .prepare("SELECT tmux_pane_index FROM sessions ORDER BY id")
            .unwrap();
        let got: Vec<Option<String>> = st.query_map([], |r| r.get(0)).unwrap().map(|r| r.unwrap()).collect();
        assert_eq!(got, [Some("2".to_owned()), None, None]);
    }

    #[test]
    fn refuses_newer_and_foreign_databases() {
        let conn = Connection::open_in_memory().unwrap();
        conn.pragma_update(None, "user_version", 99).unwrap();
        assert!(matches!(
            migrate(&conn, DbKind::Index, Path::new("x")),
            Err(IndexError::NewerSchema { found: 99, .. })
        ));
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE t(x)").unwrap();
        assert!(matches!(
            migrate(&conn, DbKind::Index, Path::new("x")),
            Err(IndexError::Schema(_))
        ));
    }

    #[test]
    fn file_open_modes_and_read_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("host/index.db");
        assert!(matches!(
            open(&path, DbKind::Index, Access::ReadOnly),
            Err(IndexError::Sqlite(_))
        ));
        let rw = open(&path, DbKind::Index, Access::ReadWrite).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let dmode = std::fs::metadata(path.parent().unwrap()).unwrap().permissions().mode() & 0o777;
        assert_eq!(dmode, 0o700);
        let jm: String = rw.pragma_query_value(None, "journal_mode", |r| r.get(0)).unwrap();
        assert_eq!(jm, "wal");
        let ro = open(&path, DbKind::Index, Access::ReadOnly).unwrap();
        assert!(ro.execute("DELETE FROM sessions", []).is_err());
        let fk: i64 = ro.pragma_query_value(None, "foreign_keys", |r| r.get(0)).unwrap();
        assert_eq!(fk, 1);

        let meta = dir.path().join("host/meta.db");
        open(&meta, DbKind::Meta, Access::ReadWrite).unwrap();
        assert!(matches!(
            open(&meta, DbKind::Index, Access::ReadOnly),
            Err(IndexError::Schema(_))
        ));
        assert!(matches!(
            open(&meta, DbKind::Index, Access::ReadWrite),
            Err(IndexError::Schema(_))
        ));
    }

    #[test]
    fn newer_schema_opens_read_only_and_says_so() {
        let dir = tempfile::tempdir().unwrap();
        for kind in [DbKind::Index, DbKind::Meta] {
            let path = dir.path().join(format!("{}.db", kind.name()));
            open(&path, kind, Access::ReadWrite).unwrap();
            {
                let c = Connection::open(&path).unwrap();
                c.pragma_update(None, "user_version", 99).unwrap();
            }
            assert!(matches!(
                open(&path, kind, Access::ReadWrite),
                Err(IndexError::NewerSchema { found: 99, .. })
            ));
            let ro = open(&path, kind, Access::ReadOnly).unwrap();
            assert_eq!(user_version(&ro).unwrap(), 99);
            assert!(ro.execute_batch("CREATE TABLE x(y)").is_err(), "read-only");
            let note = newer_note(kind, 99).unwrap();
            assert!(note.contains("version 99") && note.contains("read-only"), "{note}");
            assert_eq!(newer_note(kind, kind.version()), None);

            // A newer version without our application_id is still refused.
            let foreign = dir.path().join(format!("foreign-{}.db", kind.name()));
            {
                let c = Connection::open(&foreign).unwrap();
                c.execute_batch("CREATE TABLE t(x)").unwrap();
                c.pragma_update(None, "user_version", 99).unwrap();
            }
            assert!(matches!(
                open(&foreign, kind, Access::ReadOnly),
                Err(IndexError::NewerSchema { found: 99, .. })
            ));
        }
    }

    fn insert_session(conn: &Connection, uuid: &str) -> i64 {
        conn.execute(
            "INSERT INTO sessions(uuid, host, scope, user, cast_path, started_at, state)
             VALUES(?1, 'h', 'personal', 'u', ?1 || '.cast', 0, 'live')",
            [uuid],
        )
        .unwrap();
        conn.last_insert_rowid()
    }

    fn count(conn: &Connection, sql: &str, arg: &str) -> i64 {
        conn.query_row(sql, [arg], |r| r.get(0)).unwrap()
    }

    #[test]
    fn fts_triggers_follow_inserts_updates_and_cascades() {
        let conn = open_in_memory(DbKind::Index).unwrap();
        let sid = insert_session(&conn, "s1");
        conn.execute(
            "INSERT INTO commands(session_id, seq, t_start, ts_start, typed, cwd) VALUES(?1, 1, 0, 0, 'grep needle', '/tmp')",
            [sid],
        )
        .unwrap();
        let cid = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO outputs(session_id, command_id, t, ts, line0, kind, text)
             VALUES(?1, ?2, 0, 0, 0, 'primary', 'a haystack with Needle-42 inside')",
            [sid, cid],
        )
        .unwrap();
        let cmd_like = "SELECT count(*) FROM cmd_fts WHERE typed LIKE ?1";
        let out_like = "SELECT count(*) FROM out_fts WHERE text LIKE ?1";
        assert_eq!(count(&conn, cmd_like, "%NEEDLE%"), 1);
        assert_eq!(count(&conn, out_like, "%needle-42%"), 1);
        assert_eq!(
            count(&conn, "SELECT count(*) FROM cmd_fts WHERE cwd LIKE ?1", "%tmp%"),
            1
        );

        // Upsert on (session_id, seq) fires the update trigger only for text columns.
        conn.execute(
            "INSERT INTO commands(session_id, seq, t_start, ts_start, typed) VALUES(?1, 1, 0, 0, 'grep pin')
             ON CONFLICT(session_id, seq) DO UPDATE SET typed = excluded.typed, exit = 0",
            [sid],
        )
        .unwrap();
        assert_eq!(count(&conn, cmd_like, "%needle%"), 0);
        assert_eq!(count(&conn, cmd_like, "%grep pin%"), 1);

        conn.execute("DELETE FROM sessions WHERE id = ?1", [sid]).unwrap();
        assert_eq!(count(&conn, cmd_like, "%grep%"), 0);
        assert_eq!(count(&conn, out_like, "%needle%"), 0);
        conn.execute_batch(
            "INSERT INTO cmd_fts(cmd_fts) VALUES('integrity-check');
             INSERT INTO out_fts(out_fts) VALUES('integrity-check');",
        )
        .unwrap();
    }

    /// The §6.5 LIKE (no ESCAPE) and GLOB forms must be served by the trigram index.
    #[test]
    fn like_and_glob_use_the_trigram_index() {
        let conn = open_in_memory(DbKind::Index).unwrap();
        for (sql, arg) in [
            ("SELECT rowid FROM out_fts WHERE text LIKE ?1", "%needle%"),
            ("SELECT rowid FROM out_fts WHERE text GLOB ?1", "*needle*"),
            ("SELECT rowid FROM cmd_fts WHERE typed LIKE ?1", "%needle%"),
            (
                "SELECT rowid FROM cmd_fts WHERE typed LIKE ?1 OR expanded LIKE ?1 OR cwd LIKE ?1",
                "%needle%",
            ),
        ] {
            let mut st = conn.prepare(&format!("EXPLAIN QUERY PLAN {sql}")).unwrap();
            let plan: Vec<String> = st
                .query_map([arg], |r| r.get::<_, String>(3))
                .unwrap()
                .map(|r| r.unwrap())
                .collect();
            let plan = plan.join(" | ");
            assert!(!plan.is_empty());
            assert!(
                plan.split(" | ")
                    .filter(|p| p.contains("VIRTUAL TABLE"))
                    .all(|p| p.contains("INDEX 0:L") || p.contains("INDEX 0:G")),
                "{sql}: {plan}"
            );
        }
    }

    #[test]
    fn meta_forgotten_constraints() {
        let conn = open_in_memory(DbKind::Meta).unwrap();
        let ins = "INSERT INTO forgotten(session_uuid, kind, cmd_seq, t_from, t_to, by, at) VALUES(?1, ?2, ?3, ?4, ?5, 'me', 0)";
        conn.execute(ins, rusqlite::params!["u", "command", 3, None::<f64>, None::<f64>])
            .unwrap();
        assert!(
            conn.execute(ins, rusqlite::params!["u", "command", 3, None::<f64>, None::<f64>])
                .is_err(),
            "duplicate"
        );
        assert!(
            conn.execute(
                ins,
                rusqlite::params!["u", "command", None::<i64>, None::<f64>, None::<f64>]
            )
            .is_err()
        );
        assert!(
            conn.execute(ins, rusqlite::params!["u", "range", None::<i64>, 5.0, 1.0])
                .is_err()
        );
        conn.execute(ins, rusqlite::params!["u", "range", None::<i64>, 1.0, 5.0])
            .unwrap();
        conn.execute(
            ins,
            rusqlite::params!["u", "session", None::<i64>, None::<f64>, None::<f64>],
        )
        .unwrap();
        assert!(
            conn.execute(
                ins,
                rusqlite::params!["u", "session", None::<i64>, None::<f64>, None::<f64>]
            )
            .is_err()
        );
    }
}
