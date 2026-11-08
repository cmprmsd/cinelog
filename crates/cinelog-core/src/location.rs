//! Location checks shared by `install`, `serve` and `doctor` (§6.1, §5.6).
//!
//! - DB directories must not be synced (Syncthing `.stfolder` ancestor) or on a network
//!   filesystem: SQLite WAL is unsafe there.
//! - Files root executes must be root-owned along the whole path, and not group- or
//!   world-writable.

use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

/// `statfs` magics of filesystems SQLite WAL must not be used on.
const NETWORK_FS: [(i128, &str); 5] = [
    (0x6969, "nfs"),
    (0x517b, "smb"),
    (0xff53_4d42, "cifs"),
    (0xfe53_4d42, "smb2"),
    (0x6573_5546, "fuse"),
];

/// The nearest existing ancestor of `path` (including `path` itself).
pub fn existing_ancestor(path: &Path) -> Option<&Path> {
    path.ancestors().find(|p| p.exists())
}

/// The first directory at or above `path` that contains a Syncthing `.stfolder` marker.
pub fn synced_ancestor(path: &Path) -> Option<PathBuf> {
    path.ancestors()
        .find(|dir| dir.join(".stfolder").exists())
        .map(Path::to_path_buf)
}

/// Name of the network filesystem `path` (or its nearest existing ancestor) lives on.
pub fn network_fs(path: &Path) -> Option<&'static str> {
    let st = nix::sys::statfs::statfs(existing_ancestor(path)?).ok()?;
    let magic = st.filesystem_type().0 as i128;
    NETWORK_FS.iter().find(|(m, _)| *m == magic).map(|(_, name)| *name)
}

/// Why `dir` must not hold `index.db`/`meta.db`, if it must not.
pub fn db_location_problem(dir: &Path) -> Option<String> {
    if let Some(synced) = synced_ancestor(dir) {
        return Some(format!(
            "{} is inside the Syncthing folder {} (SQLite databases must not be synced; set paths.state)",
            dir.display(),
            synced.display()
        ));
    }
    network_fs(dir).map(|fs| {
        format!(
            "{} is on a {fs} filesystem (SQLite WAL is unsafe on network filesystems; set paths.state)",
            dir.display()
        )
    })
}

/// A path component that breaks the ownership rule of [`insecure_component`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InsecurePath {
    pub path: PathBuf,
    pub problem: String,
}

/// Checks `path` and every ancestor down to `stop` (inclusive): each must exist, be owned
/// by `uid`, not be a symlink and not be group- or world-writable (so sticky `/tmp`
/// fails too: other users can create entries there).
///
/// Returns the first offending component, starting from `path`.
pub fn insecure_component(path: &Path, uid: u32, stop: &Path) -> Option<InsecurePath> {
    for p in path.ancestors() {
        let bad = |problem: String| {
            Some(InsecurePath {
                path: p.to_path_buf(),
                problem,
            })
        };
        let meta = match std::fs::symlink_metadata(p) {
            Ok(m) => m,
            Err(e) => return bad(e.to_string()),
        };
        if meta.file_type().is_symlink() {
            return bad("is a symlink".into());
        }
        if meta.uid() != uid {
            return bad(format!("owned by uid {}, expected {uid}", meta.uid()));
        }
        if meta.mode() & 0o022 != 0 {
            return bad(format!("mode {:o} is group- or world-writable", meta.mode() & 0o7777));
        }
        if p == stop {
            break;
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    #[test]
    fn syncthing_folders_are_detected() {
        let dir = tempfile::tempdir().unwrap();
        let synced = dir.path().join("Sync");
        fs::create_dir_all(synced.join(".stfolder")).unwrap();
        let db = synced.join("state/cinelog/host");
        assert_eq!(synced_ancestor(&db), Some(synced.clone()));
        assert!(db_location_problem(&db).unwrap().contains("Syncthing"));
        assert_eq!(synced_ancestor(&dir.path().join("other/x")), None);
        assert_eq!(db_location_problem(&dir.path().join("other/x")), None);
    }

    #[test]
    fn existing_ancestor_walks_up() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(existing_ancestor(&dir.path().join("a/b/c")), Some(dir.path()));
    }

    #[test]
    fn ownership_chain() {
        let dir = tempfile::tempdir().unwrap();
        let uid = nix::unistd::geteuid().as_raw();
        let base = dir.path();
        fs::set_permissions(base, fs::Permissions::from_mode(0o755)).unwrap();
        let file = base.join("sub/bin");
        fs::create_dir(base.join("sub")).unwrap();
        fs::write(&file, b"x").unwrap();
        fs::set_permissions(&file, fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(insecure_component(&file, uid, base), None);

        fs::set_permissions(base.join("sub"), fs::Permissions::from_mode(0o777)).unwrap();
        let bad = insecure_component(&file, uid, base).unwrap();
        assert_eq!(bad.path, base.join("sub"));
        assert!(bad.problem.contains("writable"));

        fs::set_permissions(base.join("sub"), fs::Permissions::from_mode(0o755)).unwrap();
        let bad = insecure_component(&file, uid.wrapping_add(1), base).unwrap();
        assert_eq!(bad.path, file);
        assert!(insecure_component(&base.join("missing"), uid, base).is_some());
    }
}
