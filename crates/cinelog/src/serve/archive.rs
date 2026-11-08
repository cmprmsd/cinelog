//! `cinelog archive-legacy --to DIR` (plan §6.6, §15.10): zstd copies of the legacy casts the
//! index knows, written to `DIR/<path below the logs root>.zst` in the compressor's frame
//! format. The originals are only read (with `O_NOATIME`), never written, renamed or
//! removed; deleting them after checking the copies is left to the user.

use std::fs::{self, DirBuilder, File};
use std::os::unix::fs::DirBuilderExt;
use std::path::{Component, Path, PathBuf};

use cinelog_cast::{CastError, CompressOptions, compress_from, scan_frames, verify_zst};
use cinelog_index::classify::open_noatime;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArchiveOpts {
    pub to: PathBuf,
    pub level: i32,
    pub dry_run: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ArchiveReport {
    /// Legacy casts considered (those still on disk).
    pub casts: u64,
    /// Written now (with `dry_run`: would be written).
    pub archived: u64,
    /// Already archived at their current size.
    pub current: u64,
    pub failed: u64,
    pub bytes_in: u64,
    pub bytes_out: u64,
    pub errors: Vec<String>,
    /// Stopped early because the target disk is full.
    pub aborted: bool,
}

/// `path` made absolute and lexically normalised, with its longest existing prefix resolved
/// through symlinks (the rest may not exist yet).
fn resolve(path: &Path) -> std::io::Result<PathBuf> {
    let abs = std::path::absolute(path)?;
    let mut base = abs.as_path();
    let mut rest = Vec::new();
    loop {
        match base.canonicalize() {
            Ok(mut p) => {
                for c in rest.iter().rev() {
                    match c {
                        Component::ParentDir => {
                            p.pop();
                        }
                        Component::Normal(n) => p.push(n),
                        _ => {}
                    }
                }
                return Ok(p);
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let Some(parent) = base.parent() else { return Err(e) };
                rest.push(
                    base.components()
                        .next_back()
                        .expect("a path with a parent has a last component"),
                );
                base = parent;
            }
            Err(e) => return Err(e),
        }
    }
}

/// The resolved archive directory; refused inside the logs root, where the copies would sit
/// next to the originals and be walked as casts.
pub fn check_target(logs_root: &Path, to: &Path) -> Result<PathBuf, String> {
    let target = resolve(to).map_err(|e| format!("{}: {e}", to.display()))?;
    let logs = resolve(logs_root).map_err(|e| format!("{}: {e}", logs_root.display()))?;
    if target.starts_with(&logs) {
        return Err(format!(
            "{} is inside the logs directory {}; archive to another directory",
            to.display(),
            logs_root.display()
        ));
    }
    Ok(target)
}

/// `<to>/<cast relative to logs_root>.zst`; `None` for a cast outside `logs_root`.
pub fn archive_path(logs_root: &Path, to: &Path, cast: &Path) -> Option<PathBuf> {
    let rel = cast.strip_prefix(logs_root).ok()?;
    if rel.as_os_str().is_empty() || rel.components().any(|c| !matches!(c, Component::Normal(_))) {
        return None;
    }
    let mut name = to.join(rel).into_os_string();
    name.push(".zst");
    Some(PathBuf::from(name))
}

/// Whether `dst` is a complete archive of `size` bytes (frame headers only, no decoding).
fn is_current(dst: &Path, size: u64) -> bool {
    let Ok(mut f) = File::open(dst) else { return false };
    scan_frames(&mut f).is_ok_and(|frames| frames.iter().map(|f| f.u_len).sum::<u64>() == size)
}

fn is_disk_full(e: &CastError) -> bool {
    matches!(e, CastError::Io(io) if io.raw_os_error() == Some(libc::ENOSPC))
}

/// Writes one archive; returns `(uncompressed, compressed)` bytes.
fn archive_one(src: &Path, dst: &Path, level: i32) -> Result<(u64, u64), CastError> {
    if let Some(dir) = dst.parent() {
        DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
    }
    let input = open_noatime(src)?;
    let c = compress_from(
        input,
        dst,
        CompressOptions {
            level,
            ..CompressOptions::default()
        },
    )?;
    let decoded = verify_zst(dst)?;
    if decoded != c.uncompressed_bytes {
        let _ = fs::remove_file(dst);
        return Err(CastError::Io(std::io::Error::other(format!(
            "the copy decodes to {decoded} bytes, expected {}",
            c.uncompressed_bytes
        ))));
    }
    Ok((c.uncompressed_bytes, c.compressed_bytes))
}

/// Archives `casts` (legacy casts under `logs_root`) into `opts.to`, which the caller has
/// checked with [`check_target`]. A cast that grew since its last archive is archived again.
pub fn archive(casts: &[PathBuf], logs_root: &Path, opts: &ArchiveOpts) -> ArchiveReport {
    let mut r = ArchiveReport::default();
    for src in casts {
        let size = match fs::symlink_metadata(src) {
            Ok(m) if m.is_file() => m.len(),
            // Deleted (or replaced by something else) since it was imported.
            _ => continue,
        };
        r.casts += 1;
        let Some(dst) = archive_path(logs_root, &opts.to, src) else {
            r.failed += 1;
            r.errors
                .push(format!("{}: not below {}", src.display(), logs_root.display()));
            continue;
        };
        if is_current(&dst, size) {
            r.current += 1;
            continue;
        }
        if opts.dry_run {
            r.archived += 1;
            r.bytes_in += size;
            continue;
        }
        match archive_one(src, &dst, opts.level) {
            Ok((bytes_in, bytes_out)) => {
                r.archived += 1;
                r.bytes_in += bytes_in;
                r.bytes_out += bytes_out;
            }
            Err(e) => {
                r.failed += 1;
                r.errors.push(format!("{}: {e}", src.display()));
                if is_disk_full(&e) {
                    r.aborted = true;
                    break;
                }
            }
        }
    }
    r
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    fn cast(dir: &Path, rel: &str, lines: usize) -> PathBuf {
        let p = dir.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        let mut s = String::from("{\"version\": 2, \"width\": 80, \"height\": 24}\n");
        for i in 0..lines {
            s.push_str(&format!("[{i}.5, \"o\", \"line {i} of {rel}\\r\\n\"]\n"));
        }
        fs::write(&p, s).unwrap();
        p
    }

    fn stamp(p: &Path) -> (u64, i64, i64, u64, Vec<u8>) {
        let m = fs::metadata(p).unwrap();
        (m.len(), m.mtime(), m.mtime_nsec(), m.ino(), fs::read(p).unwrap())
    }

    fn opts(to: &Path, dry_run: bool) -> ArchiveOpts {
        ArchiveOpts {
            to: to.to_owned(),
            level: 3,
            dry_run,
        }
    }

    #[test]
    fn targets_inside_the_logs_root_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let logs = dir.path().join("logs");
        fs::create_dir_all(logs.join("sub")).unwrap();
        assert!(check_target(&logs, &logs).is_err());
        assert!(check_target(&logs, &logs.join("sub/new/dir")).is_err());
        assert!(check_target(&logs, &dir.path().join("x/../logs/archive")).is_err());
        std::os::unix::fs::symlink(&logs, dir.path().join("link")).unwrap();
        assert!(check_target(&logs, &dir.path().join("link/archive")).is_err());
        let ok = check_target(&logs, &dir.path().join("archive/new")).unwrap();
        assert!(ok.ends_with("archive/new"));
        assert!(
            check_target(&logs, dir.path()).is_ok(),
            "a parent of the logs root is fine"
        );
    }

    #[test]
    fn archive_paths_keep_the_layout() {
        let to = Path::new("/a");
        let logs = Path::new("/l");
        assert_eq!(
            archive_path(logs, to, Path::new("/l/2024-01/h.cast")),
            Some(PathBuf::from("/a/2024-01/h.cast.zst"))
        );
        assert_eq!(archive_path(logs, to, Path::new("/elsewhere/h.cast")), None);
        assert_eq!(archive_path(logs, to, Path::new("/l/../x.cast")), None);
    }

    #[test]
    fn copies_round_trip_and_originals_stay_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let logs = dir.path().join("logs");
        let a = cast(&logs, "host_2023.cast", 20_000);
        let b = cast(&logs, "2024-02/host_b.cast", 50);
        let gone = logs.join("deleted.cast");
        let before: Vec<_> = [&a, &b].iter().map(|p| stamp(p)).collect();
        let to = dir.path().join("archive");
        let list = vec![a.clone(), b.clone(), gone];

        let dry = archive(&list, &logs, &opts(&to, true));
        assert_eq!((dry.casts, dry.archived, dry.current, dry.failed), (2, 2, 0, 0));
        assert!(!to.exists(), "--dry-run writes nothing");

        let r = archive(&list, &logs, &opts(&to, false));
        assert_eq!(
            (r.casts, r.archived, r.current, r.failed),
            (2, 2, 0, 0),
            "{:?}",
            r.errors
        );
        assert!(r.bytes_out * 4 < r.bytes_in, "{r:?}");
        for src in [&a, &b] {
            let dst = archive_path(&logs, &to, src).unwrap();
            assert_eq!(
                zstd::decode_all(File::open(&dst).unwrap()).unwrap(),
                fs::read(src).unwrap()
            );
            assert_eq!(fs::metadata(&dst).unwrap().permissions().mode() & 0o777, 0o600);
        }
        assert_eq!(
            fs::metadata(to.join("2024-02")).unwrap().permissions().mode() & 0o777,
            0o700
        );
        let after: Vec<_> = [&a, &b].iter().map(|p| stamp(p)).collect();
        assert_eq!(after, before, "originals unchanged");
        let names: Vec<_> = fs::read_dir(&logs).unwrap().flatten().map(|e| e.file_name()).collect();
        assert!(
            names.iter().all(|n| !n.to_string_lossy().ends_with(".zst")),
            "{names:?}"
        );

        // Up to date: nothing is written; a cast that grew is archived again.
        let again = archive(&list, &logs, &opts(&to, false));
        assert_eq!((again.archived, again.current), (0, 2));
        fs::write(
            &b,
            [fs::read(&b).unwrap(), b"[99.0, \"o\", \"more\"]\n".to_vec()].concat(),
        )
        .unwrap();
        let grown = archive(&list, &logs, &opts(&to, false));
        assert_eq!((grown.archived, grown.current), (1, 1));
        let dst = archive_path(&logs, &to, &b).unwrap();
        assert_eq!(
            zstd::decode_all(File::open(dst).unwrap()).unwrap(),
            fs::read(&b).unwrap()
        );
    }

    #[test]
    fn errors_are_counted_and_reported() {
        let dir = tempfile::tempdir().unwrap();
        let logs = dir.path().join("logs");
        let a = cast(&logs, "a.cast", 5);
        let outside = cast(dir.path(), "outside.cast", 5);
        // The target's parent is a file: the archive directory cannot be created.
        let blocker = dir.path().join("blocker");
        fs::write(&blocker, "").unwrap();
        let r = archive(&[a, outside], &logs, &opts(&blocker.join("archive"), false));
        assert_eq!((r.casts, r.archived, r.failed), (2, 0, 2));
        assert_eq!(r.errors.len(), 2);
        assert!(!r.aborted);
    }
}
