//! The compressor (§6.6): finished, non-legacy casts older than
//! `retention.compress_after_days` become `.cast.zst` (independent frames cut at newlines),
//! and the startup reconciliation of interrupted runs.
//!
//! Order per part: write `<cast>.zst.tmp`, fsync, rename to `<cast>.zst`, record the frames
//! in the index (one transaction), unlink the plain cast. An interruption anywhere leaves
//! either a tmp file (removed) or both files (the zst is kept only if it decodes to exactly
//! the plain cast). Only cinelog casts are ever compressed, so only their leftovers are
//! reconciled: files next to legacy casts are the user's.

use std::collections::HashSet;
use std::fs::{self, File};
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use cinelog_cast::{CompressOptions, DEFAULT_FRAME_BYTES, PartName, compress_file, scan_frames};
use cinelog_index::model::{CompressCandidate, CompressionUpdate, SessionState, ZstFrameRow};
use cinelog_index::{CastKind, IndexError, Searcher, classify};

use super::discover::{FoundKind, Walker, is_zst};
use super::engine::{SharedWriter, lock, now_ts, one_line};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CompressOpts {
    pub after_days: u64,
    pub level: i32,
}

impl CompressOpts {
    pub fn from_config(r: &cinelog_core::config::RetentionConfig) -> Self {
        Self {
            after_days: r.compress_after_days,
            level: r.zstd_level,
        }
    }

    /// `ended_at` bound of the candidates at `now`.
    pub fn ended_before(&self, now: f64) -> f64 {
        now - self.after_days as f64 * 86_400.0
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CompressReport {
    pub candidates: u64,
    pub compressed: u64,
    pub bytes_in: u64,
    pub bytes_out: u64,
    pub skipped: u64,
    pub errors: Vec<String>,
    /// The run stopped early (disk full or shutdown).
    pub aborted: bool,
}

/// `<cast>.zst` next to a plain part.
pub fn zst_path(plain: &Path) -> PathBuf {
    let mut s = plain.as_os_str().to_owned();
    s.push(".zst");
    PathBuf::from(s)
}

fn is_disk_full(e: &cinelog_cast::CastError) -> bool {
    matches!(e, cinelog_cast::CastError::Io(io) if io.raw_os_error() == Some(libc::ENOSPC))
}

fn sync_dir(p: &Path) {
    if let Some(dir) = p.parent().filter(|d| !d.as_os_str().is_empty())
        && let Ok(f) = File::open(dir)
    {
        let _ = f.sync_all();
    }
}

/// The candidates of this run (sessions in `skip` are being indexed right now).
pub fn candidates(
    w: &SharedWriter,
    opts: &CompressOpts,
    skip: &HashSet<String>,
) -> Result<Vec<CompressCandidate>, IndexError> {
    let mut c = lock(w).compress_candidates(opts.ended_before(now_ts()))?;
    c.retain(|c| !skip.contains(&c.uuid) && !is_zst(&c.path));
    sort(&mut c);
    Ok(c)
}

fn sort(c: &mut [CompressCandidate]) {
    c.sort_by(|a, b| a.ended_at.total_cmp(&b.ended_at).then(a.part.cmp(&b.part)));
}

/// The candidates as a reader sees them, for `cinelog compress --dry-run` while the server
/// holds the writer: the cinelog casts under `logs_root` whose session is finished (`ended`
/// or `crashed`), not legacy and old enough, one per plain part.
pub fn preview(
    searcher: &dyn Searcher,
    logs_root: &Path,
    opts: &CompressOpts,
) -> Result<Vec<CompressCandidate>, IndexError> {
    let bound = opts.ended_before(now_ts());
    let mut out = Vec::new();
    for found in Walker::new().walk(logs_root).casts {
        let FoundKind::Cinelog { uuid } = &found.kind else {
            continue;
        };
        let Some(s) = searcher.session(uuid)? else { continue };
        let finished = matches!(s.state, SessionState::Ended | SessionState::Crashed);
        let Some(ended_at) = s.ended_at.filter(|t| finished && !s.legacy && *t < bound) else {
            continue;
        };
        for path in found.parts.iter().filter(|p| !is_zst(p)) {
            let part = path
                .file_name()
                .and_then(|n| n.to_str())
                .and_then(PartName::parse)
                .map_or(1, |n| n.part);
            out.push(CompressCandidate {
                uuid: uuid.clone(),
                part,
                path: path.clone(),
                ended_at,
            });
        }
    }
    sort(&mut out);
    Ok(out)
}

enum One {
    Done { bytes_in: u64, bytes_out: u64 },
    Skipped,
}

fn compress_one(w: &SharedWriter, c: &CompressCandidate, level: i32) -> Result<One, cinelog_cast::CastError> {
    let src = &c.path;
    let Ok(before) = fs::metadata(src) else {
        return Ok(One::Skipped); // gone, or already compressed by an earlier run
    };
    let dst = zst_path(src);
    let _ = fs::remove_file(&dst); // a stale zst that reconciliation did not trust
    let out = compress_file(
        src,
        &dst,
        CompressOptions {
            level,
            frame_bytes: DEFAULT_FRAME_BYTES,
        },
    )?;
    let changed = fs::metadata(src).map(|m| m.len() != before.len()).unwrap_or(true);
    if changed || out.uncompressed_bytes != before.len() {
        let _ = fs::remove_file(&dst);
        return Err(io::Error::other("the cast changed while it was compressed").into());
    }
    let update = CompressionUpdate {
        uuid: c.uuid.clone(),
        part: c.part,
        from: src.clone(),
        to: dst.clone(),
        uncompressed_bytes: out.uncompressed_bytes,
        compressed_bytes: out.compressed_bytes,
        frames: ZstFrameRow::from_frames(&out.frames),
    };
    if let Err(e) = lock(w).record_compression(&update) {
        let _ = fs::remove_file(&dst);
        return Err(io::Error::other(format!("index: {e}")).into());
    }
    fs::remove_file(src)?;
    sync_dir(src);
    Ok(One::Done {
        bytes_in: out.uncompressed_bytes,
        bytes_out: out.compressed_bytes,
    })
}

/// Compresses `list` in order; stops on a full disk or when `stop` is set.
pub fn compress(w: &SharedWriter, list: &[CompressCandidate], level: i32, stop: &AtomicBool) -> CompressReport {
    let mut r = CompressReport {
        candidates: list.len() as u64,
        ..CompressReport::default()
    };
    for c in list {
        if stop.load(Ordering::Relaxed) {
            r.aborted = true;
            break;
        }
        match compress_one(w, c, level) {
            Ok(One::Done { bytes_in, bytes_out }) => {
                r.compressed += 1;
                r.bytes_in += bytes_in;
                r.bytes_out += bytes_out;
            }
            Ok(One::Skipped) => r.skipped += 1,
            Err(e) => {
                let full = is_disk_full(&e);
                r.errors.push(one_line(format!("{}: {e}", c.path.display())));
                if full {
                    r.aborted = true;
                    break;
                }
            }
        }
    }
    r
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Reconciled {
    pub tmp_removed: u64,
    /// The zst was complete: the plain cast was removed.
    pub kept_zst: u64,
    /// The zst was incomplete or broken: it was removed.
    pub kept_plain: u64,
    /// Files next to something that is not a cinelog cast (legacy casts, other files):
    /// not ours, left alone.
    pub foreign: u64,
    pub errors: Vec<String>,
}

fn is_cinelog(plain: &Path) -> bool {
    matches!(classify(plain), Ok(CastKind::Cinelog { .. }))
}

/// Whether `zst` decodes to exactly the bytes of `plain`. Any error (a truncated or broken
/// frame) means it does not.
fn same_content(plain: &Path, zst: &Path) -> bool {
    let compare = || -> io::Result<bool> {
        let mut a = io::BufReader::new(File::open(plain)?);
        let mut b = zstd::stream::read::Decoder::new(File::open(zst)?)?;
        let (mut x, mut y) = (vec![0u8; 64 << 10], vec![0u8; 64 << 10]);
        loop {
            let n = read_full(&mut a, &mut x)?;
            let m = read_full(&mut b, &mut y)?;
            if n != m || x[..n] != y[..m] {
                return Ok(false);
            }
            if n == 0 {
                return Ok(true);
            }
        }
    };
    compare().unwrap_or(false)
}

fn read_full(r: &mut impl Read, buf: &mut [u8]) -> io::Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        match r.read(&mut buf[n..]) {
            Ok(0) => break,
            Ok(k) => n += k,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(n)
}

/// Cleans up after interrupted compressions (§6.6 step 4). `duplicates` and `tmp_files`
/// come from a walk; `busy` keys (live or being indexed) are left alone, and so is anything
/// that does not belong to a cinelog cast.
pub fn reconcile(
    w: &SharedWriter,
    duplicates: &[(PathBuf, PathBuf)],
    tmp_files: &[PathBuf],
    busy: &HashSet<PathBuf>,
) -> Reconciled {
    let mut r = Reconciled::default();
    for tmp in tmp_files {
        // `<cast>.zst.tmp` exists only before the rename, so the plain cast is still there.
        let plain = tmp.to_string_lossy();
        let plain = Path::new(plain.strip_suffix(".zst.tmp").unwrap_or(&plain));
        if !is_cinelog(plain) {
            r.foreign += 1;
            continue;
        }
        match fs::remove_file(tmp) {
            Ok(()) => r.tmp_removed += 1,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => r.errors.push(one_line(format!("{}: {e}", tmp.display()))),
        }
    }
    for (plain, zst) in duplicates {
        if busy.contains(&super::discover::session_key(plain)) {
            continue;
        }
        if let Err(e) = reconcile_pair(w, plain, zst, &mut r) {
            r.errors.push(one_line(format!("{}: {e}", plain.display())));
        }
    }
    r
}

/// A pair of a cinelog cast: the zst replaces the plain part if it holds exactly its bytes,
/// else the zst goes.
fn reconcile_pair(w: &SharedWriter, plain: &Path, zst: &Path, r: &mut Reconciled) -> Result<(), String> {
    let Ok(CastKind::Cinelog { uuid, .. }) = classify(plain) else {
        r.foreign += 1;
        return Ok(());
    };
    let size = fs::metadata(plain).map_err(|e| e.to_string())?.len();
    if !same_content(plain, zst) {
        fs::remove_file(zst).map_err(|e| e.to_string())?;
        r.kept_plain += 1;
        return Ok(());
    }
    let part = plain
        .file_name()
        .and_then(|n| n.to_str())
        .and_then(PartName::parse)
        .map_or(1, |n| n.part);
    let frames = scan_frames(&mut File::open(zst).map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
    let update = CompressionUpdate {
        uuid,
        part,
        from: plain.to_path_buf(),
        to: zst.to_path_buf(),
        uncompressed_bytes: size,
        compressed_bytes: fs::metadata(zst).map_err(|e| e.to_string())?.len(),
        frames: ZstFrameRow::from_frames(&frames),
    };
    // Unknown to the index: it will index the zst.
    match lock(w).record_compression(&update) {
        Ok(()) | Err(IndexError::NotFound(_)) => {}
        Err(e) => return Err(format!("index: {e}")),
    }
    fs::remove_file(plain).map_err(|e| e.to_string())?;
    sync_dir(plain);
    r.kept_zst += 1;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::serve::engine::shared;
    use crate::serve::testkit::*;
    use cinelog_index::IndexWriter;
    use cinelog_index::fake::MemIndex;
    use cinelog_index::model::{CastPart, IndexBatch, SessionState};

    /// Indexes a finished single-part session that ended `age_days` ago.
    fn indexed(mem: &mut MemIndex, path: &Path, uuid: &str, age_days: f64, legacy: bool) {
        let mut meta = session_meta(uuid);
        meta.cast_path = path.to_path_buf();
        meta.state = SessionState::Ended;
        meta.ended_at = Some(now_ts() - age_days * 86_400.0);
        meta.legacy = legacy;
        let mut b = IndexBatch::new(meta);
        b.parts = vec![CastPart {
            part: 1,
            path: path.to_path_buf(),
            compressed: false,
            bytes: None,
            t_offset: 0.0,
            frames: None,
        }];
        mem.apply(&b).unwrap();
    }

    fn opts() -> CompressOpts {
        CompressOpts {
            after_days: 14,
            level: 3,
        }
    }

    #[test]
    fn compresses_old_finished_casts_only() {
        let d = tempfile::tempdir().unwrap();
        let ev = [(0.5, "o", "hello\r\n")];
        let old = write_cast(d.path(), "h_a.cast", UUID_A, 1, &ev, Some(0));
        let young = write_cast(d.path(), "h_b.cast", UUID_B, 1, &ev, Some(0));
        let busy = write_cast(d.path(), "h_c.cast", UUID_C, 1, &ev, Some(0));
        let legacy = write_legacy(d.path(), "old.cast", &[(1.0, "o", "x\r\n")]);
        let original = fs::read(&old).unwrap();
        let mut mem = MemIndex::new();
        indexed(&mut mem, &old, UUID_A, 20.0, false);
        indexed(&mut mem, &young, UUID_B, 2.0, false);
        indexed(&mut mem, &busy, UUID_C, 20.0, false);
        indexed(&mut mem, &legacy, UUID_D, 400.0, true);
        let w = shared(Box::new(mem.clone()));
        let skip: HashSet<String> = [UUID_C.to_owned()].into();
        let list = candidates(&w, &opts(), &skip).unwrap();
        assert_eq!(list.len(), 1, "{list:?}");
        assert_eq!(list[0].uuid, UUID_A);
        let r = compress(&w, &list, 3, &AtomicBool::new(false));
        assert_eq!((r.compressed, r.errors.len()), (1, 0), "{r:?}");
        assert!(r.bytes_out > 0 && r.bytes_in == original.len() as u64);
        assert!(!old.exists());
        let z = zst_path(&old);
        let mut back = Vec::new();
        std::io::Read::read_to_end(
            &mut zstd::stream::read::Decoder::new(File::open(&z).unwrap()).unwrap(),
            &mut back,
        )
        .unwrap();
        assert_eq!(back, original);
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(fs::metadata(&z).unwrap().permissions().mode() & 0o777, 0o600);
        let s = mem.sessions().into_iter().find(|s| s.uuid == UUID_A).unwrap();
        assert!(s.compressed);
        assert_eq!(s.cast_path, z);
        assert!(legacy.exists() && young.exists());
        // Nothing left to do on the next run.
        assert!(candidates(&w, &opts(), &skip).unwrap().is_empty());
    }

    #[test]
    fn a_vanished_candidate_is_skipped_and_errors_are_reported() {
        let d = tempfile::tempdir().unwrap();
        let mem = MemIndex::new();
        let w = shared(Box::new(mem));
        let gone = CompressCandidate {
            uuid: UUID_A.into(),
            part: 1,
            path: d.path().join("gone.cast"),
            ended_at: 0.0,
        };
        let present = write_cast(d.path(), "h_b.cast", UUID_B, 1, &[], Some(0));
        let unknown = CompressCandidate {
            uuid: UUID_B.into(),
            part: 1,
            path: present.clone(),
            ended_at: 0.0,
        };
        let r = compress(&w, &[gone, unknown], 3, &AtomicBool::new(false));
        assert_eq!((r.skipped, r.compressed), (1, 0));
        assert_eq!(r.errors.len(), 1, "unknown part: {r:?}");
        assert!(present.exists(), "plain kept when the index update fails");
        assert!(!zst_path(&present).exists());
        let r = compress(&w, &[], 3, &AtomicBool::new(true));
        assert!(!r.aborted);
    }

    #[test]
    fn reconcile_keeps_complete_zst_and_drops_broken_ones() {
        let d = tempfile::tempdir().unwrap();
        let ev = [(0.5, "o", "data\r\n")];
        let a = write_cast(d.path(), "h_a.cast", UUID_A, 1, &ev, Some(0));
        let az = compress_copy(&a);
        let b = write_cast(d.path(), "h_b.cast", UUID_B, 1, &ev, Some(0));
        let bz = zst_path(&b);
        fs::write(&bz, b"\x28\xb5\x2f\xfdtruncated").unwrap();
        let c = write_cast(d.path(), "h_c.cast", UUID_C, 1, &ev, Some(0));
        let cz = compress_copy(&c);
        let x = write_cast(d.path(), "h_x.cast", UUID_D, 1, &ev, Some(0));
        let tmp = d.path().join("h_x.cast.zst.tmp");
        fs::write(&tmp, b"partial").unwrap();
        let mut mem = MemIndex::new();
        indexed(&mut mem, &a, UUID_A, 20.0, false);
        let w = shared(Box::new(mem.clone()));
        let busy: HashSet<PathBuf> = [c.clone()].into();
        let pairs = [
            (a.clone(), az.clone()),
            (b.clone(), bz.clone()),
            (c.clone(), cz.clone()),
        ];
        let r = reconcile(&w, &pairs, std::slice::from_ref(&tmp), &busy);
        assert_eq!(
            r,
            Reconciled {
                tmp_removed: 1,
                kept_zst: 1,
                kept_plain: 1,
                foreign: 0,
                errors: vec![]
            }
        );
        assert!(!a.exists() && az.exists());
        assert!(b.exists() && !bz.exists());
        assert!(c.exists() && cz.exists(), "busy sessions are left alone");
        assert!(!tmp.exists() && x.exists());
        let s = mem.sessions().into_iter().find(|s| s.uuid == UUID_A).unwrap();
        assert!(s.compressed);
    }

    #[test]
    fn reconcile_leaves_legacy_and_foreign_files_alone() {
        let d = tempfile::tempdir().unwrap();
        let legacy = write_legacy(d.path(), "old.cast", &[(1.0, "o", "$ ls\r\n")]);
        let legacy_z = compress_copy(&legacy);
        let legacy_tmp = d.path().join("old.cast.zst.tmp");
        fs::write(&legacy_tmp, b"the user's").unwrap();
        let orphan_tmp = d.path().join("gone.cast.zst.tmp");
        fs::write(&orphan_tmp, b"?").unwrap();
        // A cinelog cast whose zst has the same length but other bytes: not a copy of it.
        let a = write_cast(d.path(), "h_a.cast", UUID_A, 1, &[(0.5, "o", "data\r\n")], Some(0));
        let az = compress_copy(&a);
        let original = fs::read(&a).unwrap();
        let text = String::from_utf8(original.clone()).unwrap().replace("data", "date");
        fs::write(&a, &text).unwrap();
        let w = shared(Box::new(MemIndex::new()));
        let pairs = [(legacy.clone(), legacy_z.clone()), (a.clone(), az.clone())];
        let r = reconcile(&w, &pairs, &[legacy_tmp.clone(), orphan_tmp.clone()], &HashSet::new());
        assert_eq!(
            (r.foreign, r.kept_plain, r.kept_zst, r.tmp_removed),
            (3, 1, 0, 0),
            "{r:?}"
        );
        assert!(
            legacy.exists() && legacy_z.exists(),
            "legacy originals are never removed"
        );
        assert!(legacy_tmp.exists() && orphan_tmp.exists());
        assert_eq!(fs::read_to_string(&a).unwrap(), text, "the plain cast is kept");
        assert!(!az.exists());
    }

    #[test]
    fn preview_lists_what_the_writer_would() {
        let d = tempfile::tempdir().unwrap();
        let ev = [(0.5, "o", "hello\r\n")];
        let month = d.path().join("2026-10");
        let old = write_cast(&month, "h_a.cast", UUID_A, 1, &ev, Some(0));
        let old2 = write_cast(&month, "h_a_part2.cast", UUID_A, 2, &ev, Some(0));
        let young = write_cast(&month, "h_b.cast", UUID_B, 1, &ev, Some(0));
        let done = write_cast(&month, "h_c.cast", UUID_C, 1, &ev, Some(0));
        compress_copy(&done);
        fs::remove_file(&done).unwrap();
        let mut mem = MemIndex::new();
        indexed(&mut mem, &old, UUID_A, 20.0, false);
        indexed(&mut mem, &young, UUID_B, 2.0, false);
        indexed(&mut mem, &done, UUID_C, 30.0, false);
        let list = preview(&mem, d.path(), &opts()).unwrap();
        let got: Vec<_> = list.iter().map(|c| (c.uuid.as_str(), c.part, c.path.clone())).collect();
        assert_eq!(got, [(UUID_A, 1, old), (UUID_A, 2, old2)]);
    }
}
