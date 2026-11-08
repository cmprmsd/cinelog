//! meta.db outside the index (§6.3): the nightly `meta-YYYYMMDD.jsonl.zst` backups,
//! `cinelog meta export|import`, and the one-time import of the port `install` migrated
//! from the old settings file.

use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use cinelog_index::model::{DbPaths, META_BACKUPS_KEPT, settings_keys};
use cinelog_index::{IndexError, MetaStore};

use super::engine::now_ts;

/// zstd level of exports: they are small and written while the server runs.
const EXPORT_LEVEL: i32 = 3;

fn is_zst_name(p: &Path) -> bool {
    p.extension().is_some_and(|e| e == "zst")
}

/// Writes `path` atomically (0600): `<dir>/.<name>.<pid>.tmp`, fsync, rename.
fn write_atomic(path: &Path, body: impl FnOnce(&mut dyn Write) -> Result<u64, IndexError>) -> Result<u64, IndexError> {
    let dir = path
        .parent()
        .filter(|d| !d.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let tmp = dir.join(format!(".{name}.{}.tmp", std::process::id()));
    let result = (|| {
        let file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)?;
        let mut out = io::BufWriter::new(file);
        let rows = if is_zst_name(path) {
            let mut enc = zstd::stream::write::Encoder::new(&mut out, EXPORT_LEVEL)?;
            let rows = body(&mut enc)?;
            enc.finish()?;
            rows
        } else {
            body(&mut out)?
        };
        let file = out.into_inner().map_err(io::IntoInnerError::into_error)?;
        file.sync_all()?;
        fs::rename(&tmp, path)?;
        File::open(dir)?.sync_all()?;
        Ok(rows)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

/// `cinelog meta export FILE` (`-` = stdout; a `.zst` name compresses). Returns the rows.
pub fn export(meta: &dyn MetaStore, file: &Path) -> Result<u64, IndexError> {
    if file == Path::new("-") {
        let mut out = io::stdout().lock();
        let rows = meta.export_jsonl(&mut out)?;
        out.flush()?;
        return Ok(rows);
    }
    write_atomic(file, |w| meta.export_jsonl(w))
}

/// `cinelog meta import FILE` (`-` = stdin; zstd is detected by its magic).
pub fn import(meta: &mut dyn MetaStore, file: &Path) -> Result<u64, IndexError> {
    let input: Box<dyn Read> = if file == Path::new("-") {
        Box::new(io::stdin().lock())
    } else {
        Box::new(File::open(file)?)
    };
    let mut input = BufReader::new(input);
    let zst = cinelog_cast::is_zstd_magic(input.fill_buf()?);
    if zst {
        let mut dec = BufReader::new(zstd::stream::read::Decoder::with_buffer(input)?);
        meta.import_jsonl(&mut dec)
    } else {
        meta.import_jsonl(&mut input)
    }
}

/// Backups in `dir`, oldest first.
pub fn backups(dir: &Path) -> Vec<PathBuf> {
    let Ok(rd) = fs::read_dir(dir) else { return Vec::new() };
    let mut v: Vec<PathBuf> = rd
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| {
            p.file_name().and_then(|n| n.to_str()).is_some_and(|n| {
                n.strip_prefix("meta-")
                    .and_then(|r| r.strip_suffix(".jsonl.zst"))
                    .is_some_and(|d| d.len() == 8 && d.bytes().all(|b| b.is_ascii_digit()))
            })
        })
        .collect();
    v.sort();
    v
}

/// Removes all but the newest `keep` backups. Returns what was removed.
pub fn prune(dir: &Path, keep: usize) -> Vec<PathBuf> {
    let all = backups(dir);
    let n = all.len().saturating_sub(keep);
    all.into_iter().take(n).filter(|p| fs::remove_file(p).is_ok()).collect()
}

/// The backup of `today`, written if it does not exist yet (so one per day, at the first
/// check after midnight or after the server starts). Returns the new file.
pub fn nightly(
    meta: &mut dyn MetaStore,
    db: &DbPaths,
    today: jiff::civil::Date,
) -> Result<Option<PathBuf>, IndexError> {
    let path = db.meta_backup(today);
    if path.exists() {
        return Ok(None);
    }
    write_atomic(&path, |w| meta.export_jsonl(w))?;
    meta.set_setting(settings_keys::LAST_BACKUP, Some(&now_ts().to_string()))?;
    prune(&db.dir, META_BACKUPS_KEPT);
    Ok(Some(path))
}

/// Moves `<db_dir>/migrated-port` (written by `install` from the old settings file) into
/// `meta.settings` unless a port is already persisted. The file is left in place and
/// ignored from then on.
pub fn import_migrated_port(meta: &mut dyn MetaStore, file: &Path) -> Result<Option<u16>, IndexError> {
    let Ok(text) = fs::read_to_string(file) else {
        return Ok(None);
    };
    if meta.setting(settings_keys::PORT)?.is_some() {
        return Ok(None);
    }
    let Some(port) = text.trim().parse::<u16>().ok().filter(|p| *p != 0) else {
        tracing::warn!("{}: not a port number, ignored", file.display());
        return Ok(None);
    };
    meta.set_setting(settings_keys::PORT, Some(&port.to_string()))?;
    Ok(Some(port))
}

#[cfg(test)]
mod tests {
    use super::*;
    use cinelog_index::fake::MemIndex;
    use cinelog_index::model::ForgetTarget;

    #[test]
    fn nightly_backup_once_per_day_and_pruned() {
        let d = tempfile::tempdir().unwrap();
        let db = DbPaths::in_dir(d.path());
        let mut mem = MemIndex::new();
        mem.forget(
            &ForgetTarget::Session {
                uuid: "01934f5e-7b2a-7c3d-8e4f-0123456789ab".into(),
            },
            "me",
        )
        .unwrap();
        // Floats the default serde_json parser moves by one ulp.
        mem.forget(
            &ForgetTarget::Range {
                uuid: "01934f5e-7b2a-7c3d-8e4f-0123456789cd".into(),
                t_from: 0.1 + 0.2,
                t_to: 2.0f64.sqrt() * 1e6,
            },
            "me",
        )
        .unwrap();
        for day in 1..=20 {
            fs::write(db.meta_backup(jiff::civil::date(2026, 10, day)), b"old").unwrap();
        }
        fs::write(d.path().join("meta-latest.jsonl.zst"), b"not a backup").unwrap();
        let today = jiff::civil::date(2026, 11, 8);
        let p = nightly(&mut mem, &db, today).unwrap().unwrap();
        assert_eq!(p, db.meta_backup(today));
        assert!(nightly(&mut mem, &db, today).unwrap().is_none(), "once per day");
        let kept = backups(d.path());
        assert_eq!(kept.len(), META_BACKUPS_KEPT);
        assert_eq!(kept.last(), Some(&p));
        assert!(d.path().join("meta-latest.jsonl.zst").exists());
        assert!(mem.setting(settings_keys::LAST_BACKUP).unwrap().is_some());
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(fs::metadata(&p).unwrap().permissions().mode() & 0o777, 0o600);

        // The backup restores into an empty store.
        let mut restored = MemIndex::new();
        let rows = import(&mut restored, &p).unwrap();
        assert!(rows >= 1);
        // Exact, floats included (serde_json `float_roundtrip`): `t_from` is part of a key.
        assert_eq!(restored.forgotten().unwrap(), mem.forgotten().unwrap());
        assert!(
            fs::read_dir(d.path())
                .unwrap()
                .all(|e| !e.unwrap().file_name().to_string_lossy().ends_with(".tmp"))
        );
    }

    #[test]
    fn export_and_import_plain_files() {
        let d = tempfile::tempdir().unwrap();
        let mut mem = MemIndex::new();
        mem.set_setting("k", Some("v")).unwrap();
        let f = d.path().join("meta.jsonl");
        let rows = export(&mem, &f).unwrap();
        assert!(rows >= 1);
        assert!(fs::read_to_string(&f).unwrap().contains("\"k\""));
        let mut other = MemIndex::new();
        assert_eq!(import(&mut other, &f).unwrap(), rows);
        assert_eq!(other.setting("k").unwrap().as_deref(), Some("v"));
        assert!(import(&mut other, &d.path().join("missing")).is_err());
    }

    #[test]
    fn migrated_port_is_imported_once() {
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("migrated-port");
        let mut mem = MemIndex::new();
        assert_eq!(import_migrated_port(&mut mem, &f).unwrap(), None, "no file");
        fs::write(&f, "8123\n").unwrap();
        assert_eq!(import_migrated_port(&mut mem, &f).unwrap(), Some(8123));
        assert_eq!(mem.setting(settings_keys::PORT).unwrap().as_deref(), Some("8123"));
        fs::write(&f, "9000\n").unwrap();
        assert_eq!(import_migrated_port(&mut mem, &f).unwrap(), None, "persisted port wins");
        assert_eq!(mem.setting(settings_keys::PORT).unwrap().as_deref(), Some("8123"));
        let mut fresh = MemIndex::new();
        fs::write(&f, "junk").unwrap();
        assert_eq!(import_migrated_port(&mut fresh, &f).unwrap(), None);
        assert_eq!(fresh.setting(settings_keys::PORT).unwrap(), None);
    }
}
