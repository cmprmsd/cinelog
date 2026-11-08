//! Discovery (§6.4): the directory walk over the logs root and the registry scan.
//!
//! Every session is keyed by the plain path of its first part (`<stem>.cast`), so the
//! plain and the compressed file, and all rotation parts, map to the same key.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use cinelog_cast::{PartName, part_file_name};
use cinelog_core::Scope;
use cinelog_core::registry::{ProcessProbe, RecState, Registry};
use cinelog_index::model::FileStamp;
use cinelog_index::{CastKind, classify};

/// Session key of any part (plain or compressed) of a cast.
pub fn session_key(path: &Path) -> PathBuf {
    path.file_name()
        .and_then(|n| n.to_str())
        .and_then(PartName::parse)
        .map(|n| path.with_file_name(part_file_name(&n.stem, 1, false)))
        .unwrap_or_else(|| path.to_path_buf())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FoundKind {
    Cinelog { uuid: String },
    Legacy,
}

/// One session found on disk.
#[derive(Debug, Clone, PartialEq)]
pub struct Found {
    pub key: PathBuf,
    /// The file to open the pipeline with: the lowest existing part (plain preferred).
    pub path: PathBuf,
    pub kind: FoundKind,
    /// Existing part files in order (plain preferred over `.zst`).
    pub parts: Vec<PathBuf>,
    /// Stamp of [`Self::path`].
    pub stamp: FileStamp,
    /// Directory depth below the logs root (0 = directly in it).
    pub depth: usize,
}

impl Found {
    pub fn is_legacy(&self) -> bool {
        self.kind == FoundKind::Legacy
    }

    /// Stamp of part `part` (1-based) if that part is still a plain file.
    pub fn plain_part_stamp(&self, part: u32) -> Option<FileStamp> {
        let p = self.parts.get(part.checked_sub(1)? as usize)?;
        (!is_zst(p)).then(|| FileStamp::of(p).ok()).flatten()
    }
}

/// Everything one walk produced.
#[derive(Debug, Default)]
pub struct WalkResult {
    pub casts: Vec<Found>,
    /// `(plain, zst)` pairs of the same part: a compression that was interrupted between
    /// rename and unlink (or before the DB update). Reconciled at startup.
    pub duplicates: Vec<(PathBuf, PathBuf)>,
    /// Leftover `*.cast.zst.tmp` files of an interrupted compression.
    pub tmp_files: Vec<PathBuf>,
}

pub fn is_zst(p: &Path) -> bool {
    p.extension().is_some_and(|e| e == "zst")
}

#[derive(Default)]
struct Group {
    depth: usize,
    /// part → (plain, zst)
    parts: BTreeMap<u32, (Option<PathBuf>, Option<PathBuf>)>,
}

/// Classifies files once per (path, inode); casts do not change kind while they grow.
#[derive(Debug, Default)]
pub struct Walker {
    cache: HashMap<(PathBuf, u64), CastKind>,
}

impl Walker {
    pub fn new() -> Self {
        Self::default()
    }

    fn classify_cached(&mut self, path: &Path, ino: u64) -> Option<CastKind> {
        let key = (path.to_path_buf(), ino);
        if let Some(k) = self.cache.get(&key) {
            return Some(k.clone());
        }
        match classify(path) {
            Ok(k) => {
                self.cache.insert(key, k.clone());
                Some(k)
            }
            Err(e) => {
                tracing::debug!("cannot classify {}: {e}", path.display());
                None
            }
        }
    }

    /// Walks `root` (hidden entries and symlinks are skipped).
    pub fn walk(&mut self, root: &Path) -> WalkResult {
        let mut files = Vec::new();
        let mut tmp_files = Vec::new();
        let it = walkdir::WalkDir::new(root)
            .follow_links(false)
            .into_iter()
            .filter_entry(|e| e.depth() == 0 || !e.file_name().to_string_lossy().starts_with('.'));
        for entry in it {
            let entry = match entry {
                Ok(e) => e,
                Err(e) => {
                    tracing::debug!("walk {}: {e}", root.display());
                    continue;
                }
            };
            if !entry.file_type().is_file() {
                continue;
            }
            let name = entry.file_name().to_string_lossy();
            if name.ends_with(".cast.zst.tmp") {
                tmp_files.push(entry.path().to_path_buf());
            } else if PartName::parse(&name).is_some() {
                files.push((entry.path().to_path_buf(), entry.depth().saturating_sub(1)));
            }
        }
        let mut out = self.group(files);
        out.tmp_files = tmp_files;
        out
    }

    /// Discovery over an explicit list of casts (`cinelog index CAST…`); directories are
    /// walked.
    pub fn collect(&mut self, paths: &[PathBuf]) -> WalkResult {
        let mut out = WalkResult::default();
        let mut files = Vec::new();
        for p in paths {
            if p.is_dir() {
                let r = self.walk(p);
                out.casts.extend(r.casts);
                out.duplicates.extend(r.duplicates);
                out.tmp_files.extend(r.tmp_files);
            } else {
                files.push((p.clone(), 0));
            }
        }
        let r = self.group(files);
        out.casts.extend(r.casts);
        out.duplicates.extend(r.duplicates);
        out
    }

    fn group(&mut self, files: Vec<(PathBuf, usize)>) -> WalkResult {
        let mut groups: BTreeMap<PathBuf, Group> = BTreeMap::new();
        for (path, depth) in files {
            let Some(name) = path.file_name().and_then(|n| n.to_str()).and_then(PartName::parse) else {
                continue;
            };
            let g = groups.entry(session_key(&path)).or_default();
            g.depth = depth;
            let slot = g.parts.entry(name.part).or_default();
            if name.compressed {
                slot.1 = Some(path);
            } else {
                slot.0 = Some(path);
            }
        }
        let mut out = WalkResult::default();
        for (key, g) in groups {
            let mut parts = Vec::new();
            for (plain, zst) in g.parts.values() {
                if let (Some(p), Some(z)) = (plain, zst) {
                    out.duplicates.push((p.clone(), z.clone()));
                }
                if let Some(p) = plain.as_ref().or(zst.as_ref()) {
                    parts.push(p.clone());
                }
            }
            let Some(first) = parts.first().cloned() else { continue };
            let Ok(md) = std::fs::metadata(&first) else { continue };
            use std::os::unix::fs::MetadataExt;
            match self.classify_cached(&first, md.ino()) {
                Some(CastKind::Cinelog { uuid, .. }) => out.casts.push(Found {
                    key,
                    path: first,
                    kind: FoundKind::Cinelog { uuid },
                    parts,
                    stamp: FileStamp::from_metadata(&md),
                    depth: g.depth,
                }),
                // Legacy casts have no rotation parts: a `_partN` name is just a name.
                Some(CastKind::Legacy) => {
                    for p in parts {
                        if let Some(found) = self.legacy_file(&p, g.depth) {
                            out.casts.push(found);
                        }
                    }
                }
                Some(CastKind::NotACast) | None => {}
            }
        }
        out
    }

    fn legacy_file(&mut self, path: &Path, depth: usize) -> Option<Found> {
        use std::os::unix::fs::MetadataExt;
        let md = std::fs::metadata(path).ok()?;
        matches!(self.classify_cached(path, md.ino())?, CastKind::Legacy).then(|| Found {
            key: path.to_path_buf(),
            path: path.to_path_buf(),
            kind: FoundKind::Legacy,
            parts: vec![path.to_path_buf()],
            stamp: FileStamp::from_metadata(&md),
            depth,
        })
    }
}

/// Liveness of a recorder from its registry entry.
#[derive(Debug, Clone, PartialEq)]
pub struct LiveEntry {
    pub uuid: String,
    pub key: PathBuf,
    /// Current part (changes on rotation).
    pub cast: PathBuf,
    /// pid + starttime + boot_id describe a running process.
    pub alive: bool,
    pub state: RecState,
}

/// Reads every registry entry of `scope` in `dirs`. A uuid listed twice (a stale entry and
/// a live one) keeps the live one.
pub fn scan_registry(dirs: &[PathBuf], scope: Scope, probe: &dyn ProcessProbe) -> Vec<LiveEntry> {
    let mut by_uuid: BTreeMap<String, LiveEntry> = BTreeMap::new();
    for dir in dirs {
        let Ok(entries) = Registry::new(dir).entries() else {
            continue;
        };
        for (_, entry) in entries {
            let Ok(e) = entry else { continue };
            if e.scope != scope || e.uuid.is_empty() {
                continue;
            }
            let live = LiveEntry {
                key: session_key(&e.cast),
                alive: e.is_alive(probe),
                state: e.state,
                cast: e.cast,
                uuid: e.uuid,
            };
            match by_uuid.get(&live.uuid) {
                Some(have) if have.alive && !live.alive => {}
                _ => {
                    by_uuid.insert(live.uuid.clone(), live);
                }
            }
        }
    }
    by_uuid.into_values().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::serve::testkit::*;
    use cinelog_core::registry::{RegistryEntry, TtyKey};

    #[test]
    fn keys_normalise_parts_and_compression() {
        let k = Path::new("/l/2026-11/h_x_abcd.cast");
        assert_eq!(session_key(k), k);
        assert_eq!(session_key(Path::new("/l/2026-11/h_x_abcd.cast.zst")), k);
        assert_eq!(session_key(Path::new("/l/2026-11/h_x_abcd_part3.cast")), k);
        assert_eq!(session_key(Path::new("/l/other.txt")), Path::new("/l/other.txt"));
    }

    #[test]
    fn walk_groups_parts_and_finds_leftovers() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path();
        let month = root.join("2026-11");
        let a = write_cast(&month, "h_a_00000001.cast", UUID_A, 1, &[(0.5, "o", "hi\r\n")], None);
        let a2 = write_cast(&month, "h_a_00000001_part2.cast", UUID_A, 2, &[], None);
        let b = write_cast(&month, "h_b_00000002.cast", UUID_B, 1, &[], Some(0));
        let bz = compress_copy(&b);
        let legacy = write_legacy(root, "old_2024.cast", &[(1.0, "o", "$ ls\r\n")]);
        std::fs::write(root.join("junk.cast"), b"no cast").unwrap();
        std::fs::write(root.join("old_2024.cast.commands.log"), b"[1, \"m\", \"ls\"]\n").unwrap();
        let hidden = root.join(".git");
        write_cast(&hidden, "h_c.cast", UUID_C, 1, &[], None);
        let tmp = month.join("h_x.cast.zst.tmp");
        std::fs::write(&tmp, b"partial").unwrap();

        let mut w = Walker::new();
        let r = w.walk(root);
        let mut casts = r.casts.clone();
        casts.sort_by(|x, y| x.key.cmp(&y.key));
        assert_eq!(casts.len(), 3, "{casts:#?}");
        let fa = casts.iter().find(|c| c.key == a).unwrap();
        assert_eq!(fa.kind, FoundKind::Cinelog { uuid: UUID_A.into() });
        assert_eq!(fa.parts, [a.clone(), a2]);
        assert_eq!(fa.depth, 1);
        let fb = casts.iter().find(|c| c.key == b).unwrap();
        assert_eq!(fb.path, b, "plain preferred over the zst");
        assert_eq!(r.duplicates, [(b.clone(), bz.clone())]);
        let fl = casts.iter().find(|c| c.key == legacy).unwrap();
        assert!(fl.is_legacy());
        assert_eq!(fl.depth, 0);
        assert_eq!(r.tmp_files, [tmp]);

        std::fs::remove_file(&b).unwrap();
        let r = w.walk(root);
        let fb = r.casts.iter().find(|c| c.key == b).unwrap();
        assert_eq!(fb.path, bz, "the zst alone keeps the plain key");
        assert_eq!(fb.plain_part_stamp(1), None);
        assert!(r.duplicates.is_empty());

        let r = w.collect(&[legacy.clone(), month.clone()]);
        assert_eq!(r.casts.len(), 3);
    }

    #[test]
    fn registry_scan_prefers_live_entries_and_filters_scope() {
        let d = tempfile::tempdir().unwrap();
        let r1 = Registry::new(d.path().join("a"));
        let r2 = Registry::new(d.path().join("b"));
        r1.ensure_dir().unwrap();
        r2.ensure_dir().unwrap();
        let entry = |pid: u32, uuid: &str, scope: Scope| RegistryEntry {
            format: 1,
            pid,
            starttime: 7,
            boot_id: "boot".into(),
            uuid: uuid.into(),
            nonce: cinelog_core::Nonce::from_bytes([1; 16]),
            scope,
            cast: format!("/l/{uuid}_part2.cast").into(),
            pts: "/dev/pts/3".into(),
            host_tty: String::new(),
            shell_pid: 0,
            parent_uuid: None,
            state: RecState::Degraded,
            state_reason: None,
            part: 2,
            version: String::new(),
            opts: String::new(),
        };
        r1.write(&TtyKey::Pts(1), &entry(42, "u1", Scope::Personal)).unwrap();
        r2.write(&TtyKey::Pts(2), &entry(43, "u1", Scope::Personal)).unwrap();
        r2.write(&TtyKey::Pts(3), &entry(42, "u2", Scope::Team)).unwrap();
        let probe = FakeProbe::alive(&[(43, 7)]);
        let live = scan_registry(&[r1.dir().into(), r2.dir().into()], Scope::Personal, &probe);
        assert_eq!(live.len(), 1);
        assert!(live[0].alive);
        assert_eq!(live[0].key, Path::new("/l/u1.cast"));
        assert_eq!(live[0].state, RecState::Degraded);
        let probe = FakeProbe::alive(&[]);
        let live = scan_registry(&[r1.dir().into()], Scope::Personal, &probe);
        assert!(!live[0].alive);
    }
}
