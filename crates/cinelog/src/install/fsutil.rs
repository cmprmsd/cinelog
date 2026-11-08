//! Small, idempotent filesystem steps used by `install`. Each returns what it did, so the
//! command can print an accurate summary.

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Created,
    /// Existed with broader permissions; tightened from the given mode.
    Tightened(u32),
    Unchanged,
    Replaced,
}

/// How [`ensure_dir_with`] treats a symlink at the directory's path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Symlinks {
    /// Refuse it (system paths that root writes into).
    Refuse,
    /// Follow it to a directory owned by this uid and check and tighten that directory
    /// (a `~/logs` on a data disk, a stow-managed `~/.config/cinelog`).
    FollowOwnedBy(u32),
}

/// Creates `dir` (and missing parents) with `mode`. An existing directory keeps its mode
/// unless it grants bits outside `mode`, in which case it is set to `mode`. A symlink at
/// `dir` is refused.
pub fn ensure_dir(dir: &Path, mode: u32) -> Result<Outcome> {
    ensure_dir_with(dir, mode, Symlinks::Refuse)
}

pub fn ensure_dir_with(dir: &Path, mode: u32, symlinks: Symlinks) -> Result<Outcome> {
    let meta = match fs::symlink_metadata(dir) {
        Ok(m) => m,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            fs::DirBuilder::new()
                .recursive(true)
                .mode(mode)
                .create(dir)
                .with_context(|| format!("creating {}", dir.display()))?;
            // The umask may have masked bits off.
            fs::set_permissions(dir, fs::Permissions::from_mode(mode))?;
            return Ok(Outcome::Created);
        }
        Err(e) => return Err(e).with_context(|| format!("inspecting {}", dir.display())),
    };
    let meta = if meta.file_type().is_symlink() {
        let Symlinks::FollowOwnedBy(uid) = symlinks else {
            bail!("{} is a symlink; a real directory is required here", dir.display());
        };
        let target = fs::metadata(dir).with_context(|| format!("following the symlink {}", dir.display()))?;
        if !target.is_dir() {
            bail!("{} is a symlink to something that is not a directory", dir.display());
        }
        if target.uid() != uid {
            bail!(
                "{} links to a directory owned by uid {}, not by you (uid {uid})",
                dir.display(),
                target.uid()
            );
        }
        target
    } else if meta.is_dir() {
        meta
    } else {
        bail!("{} exists and is not a directory", dir.display());
    };
    let cur = meta.permissions().mode() & 0o7777;
    if cur & !mode & 0o777 == 0 {
        return Ok(Outcome::Unchanged);
    }
    // chmod follows a symlink, so this tightens the target.
    fs::set_permissions(dir, fs::Permissions::from_mode(mode))
        .with_context(|| format!("chmod {mode:o} {}", dir.display()))?;
    Ok(Outcome::Tightened(cur))
}

/// Writes `bytes` to `path` through a temp file in the same directory and a rename, so a
/// reader never sees a partial file. Unchanged content is left alone.
pub fn write_atomic(path: &Path, bytes: &[u8], mode: u32) -> Result<Outcome> {
    write_atomic_owned(path, bytes, mode, None)
}

/// [`write_atomic`] that also gives the new file `owner` (uid, gid) before it replaces the
/// old one.
fn write_atomic_owned(path: &Path, bytes: &[u8], mode: u32, owner: Option<(u32, u32)>) -> Result<Outcome> {
    let existed = match fs::read(path) {
        Ok(old) if old == bytes => {
            fs::set_permissions(path, fs::Permissions::from_mode(mode))?;
            return Ok(Outcome::Unchanged);
        }
        Ok(_) => true,
        Err(e) if e.kind() == io::ErrorKind::NotFound => false,
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    let tmp = temp_sibling(path)?;
    let res = (|| -> io::Result<()> {
        let mut f = OpenOptions::new().write(true).create_new(true).mode(mode).open(&tmp)?;
        f.write_all(bytes)?;
        if let Some((uid, gid)) = owner {
            std::os::unix::fs::fchown(&f, Some(uid), Some(gid))?;
        }
        f.set_permissions(fs::Permissions::from_mode(mode))?;
        f.sync_all()?;
        fs::rename(&tmp, path)
    })();
    if let Err(e) = res {
        let _ = fs::remove_file(&tmp);
        return Err(e).with_context(|| format!("writing {}", path.display()));
    }
    Ok(if existed { Outcome::Replaced } else { Outcome::Created })
}

/// The file a rewrite of `path` must replace: `path` itself, or the target of a symlink at
/// `path` (so the link survives). `None` while neither exists.
pub fn rewrite_target(path: &Path) -> Result<Option<PathBuf>> {
    match fs::symlink_metadata(path) {
        Ok(m) if m.file_type().is_symlink() => {
            let target = fs::canonicalize(path).with_context(|| format!("resolving the symlink {}", path.display()))?;
            Ok(Some(target))
        }
        Ok(_) => Ok(Some(path.to_path_buf())),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("inspecting {}", path.display())),
    }
}

/// Atomically rewrites the existing regular file `path` (no symlink; see
/// [`rewrite_target`]) with its current mode and owner.
pub fn rewrite_preserving(path: &Path, bytes: &[u8]) -> Result<Outcome> {
    let meta = fs::symlink_metadata(path).with_context(|| format!("inspecting {}", path.display()))?;
    if !meta.is_file() {
        bail!("{} is not a regular file", path.display());
    }
    write_atomic_owned(
        path,
        bytes,
        meta.permissions().mode() & 0o7777,
        Some((meta.uid(), meta.gid())),
    )
}

/// Creates `path` with `bytes` only if it does not exist yet (never a partial file).
pub fn write_new(path: &Path, bytes: &[u8], mode: u32) -> Result<Outcome> {
    if fs::symlink_metadata(path).is_ok() {
        return Ok(Outcome::Unchanged);
    }
    write_atomic(path, bytes, mode)
}

fn temp_sibling(path: &Path) -> Result<PathBuf> {
    let name = path
        .file_name()
        .with_context(|| format!("{} has no file name", path.display()))?;
    let mut tmp = name.to_os_string();
    tmp.push(format!(".cinelog-new.{}", std::process::id()));
    Ok(path.with_file_name(tmp))
}

/// Points the symlink `link` at `target`, replacing an older symlink. A regular file at
/// `link` is never replaced.
pub fn ensure_symlink(link: &Path, target: &Path) -> Result<Outcome> {
    match fs::symlink_metadata(link) {
        Ok(m) if m.file_type().is_symlink() => {
            if fs::read_link(link)? == target {
                return Ok(Outcome::Unchanged);
            }
            let tmp = temp_sibling(link)?;
            std::os::unix::fs::symlink(target, &tmp)?;
            fs::rename(&tmp, link).with_context(|| format!("replacing {}", link.display()))?;
            Ok(Outcome::Replaced)
        }
        Ok(_) => bail!(
            "{} exists and is not a symlink; remove it or put cinelog elsewhere",
            link.display()
        ),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            std::os::unix::fs::symlink(target, link).with_context(|| format!("creating {}", link.display()))?;
            Ok(Outcome::Created)
        }
        Err(e) => Err(e).with_context(|| format!("inspecting {}", link.display())),
    }
}

/// Removes a file or symlink; a missing path is not an error. Returns whether it existed.
pub fn remove_file(path: &Path) -> Result<bool> {
    match fs::remove_file(path) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e).with_context(|| format!("removing {}", path.display())),
    }
}

/// `~`-relative display of `path` for messages.
pub fn pretty(path: &Path, home: &Path) -> String {
    match path.strip_prefix(home) {
        Ok(rest) if !home.as_os_str().is_empty() && home != Path::new("/") => format!("~/{}", rest.display()),
        _ => path.display().to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mode(p: &Path) -> u32 {
        fs::symlink_metadata(p).unwrap().permissions().mode() & 0o7777
    }

    #[test]
    fn dirs_are_created_and_tightened() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("a/b");
        assert_eq!(ensure_dir(&p, 0o700).unwrap(), Outcome::Created);
        assert_eq!(mode(&p), 0o700);
        assert_eq!(ensure_dir(&p, 0o700).unwrap(), Outcome::Unchanged);
        fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(ensure_dir(&p, 0o700).unwrap(), Outcome::Tightened(0o755));
        assert_eq!(mode(&p), 0o700);
        // Narrower than requested is fine.
        fs::set_permissions(&p, fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(ensure_dir(&p, 0o755).unwrap(), Outcome::Unchanged);
        fs::write(d.path().join("f"), b"").unwrap();
        assert!(ensure_dir(&d.path().join("f"), 0o700).is_err());
    }

    #[test]
    fn symlinked_dirs_are_followed_only_when_allowed() {
        let d = tempfile::tempdir().unwrap();
        let disk = d.path().join("disk/logs");
        fs::create_dir_all(&disk).unwrap();
        fs::set_permissions(&disk, fs::Permissions::from_mode(0o755)).unwrap();
        let link = d.path().join("logs");
        std::os::unix::fs::symlink(&disk, &link).unwrap();

        let err = ensure_dir(&link, 0o700).unwrap_err();
        assert!(err.to_string().contains("symlink"), "{err}");
        assert_eq!(mode(&disk), 0o755, "the strict variant changes nothing");

        let me = nix::unistd::geteuid().as_raw();
        assert_eq!(
            ensure_dir_with(&link, 0o700, Symlinks::FollowOwnedBy(me)).unwrap(),
            Outcome::Tightened(0o755)
        );
        assert_eq!(mode(&disk), 0o700, "the target is tightened");
        assert!(fs::symlink_metadata(&link).unwrap().file_type().is_symlink());
        assert_eq!(
            ensure_dir_with(&link, 0o700, Symlinks::FollowOwnedBy(me)).unwrap(),
            Outcome::Unchanged
        );
        assert!(ensure_dir_with(&link, 0o700, Symlinks::FollowOwnedBy(me.wrapping_add(1))).is_err());

        let file = d.path().join("file");
        fs::write(&file, b"").unwrap();
        let to_file = d.path().join("to-file");
        std::os::unix::fs::symlink(&file, &to_file).unwrap();
        assert!(ensure_dir_with(&to_file, 0o700, Symlinks::FollowOwnedBy(me)).is_err());
    }

    #[test]
    fn rewrites_keep_symlinks_mode_and_owner() {
        let d = tempfile::tempdir().unwrap();
        let real = d.path().join("managed/zshrc");
        fs::create_dir_all(real.parent().unwrap()).unwrap();
        fs::write(&real, b"old").unwrap();
        fs::set_permissions(&real, fs::Permissions::from_mode(0o640)).unwrap();
        let link = d.path().join("zshrc");
        std::os::unix::fs::symlink(&real, &link).unwrap();

        let target = rewrite_target(&link).unwrap().unwrap();
        assert_eq!(target, fs::canonicalize(&real).unwrap());
        assert_eq!(rewrite_preserving(&target, b"new").unwrap(), Outcome::Replaced);
        assert!(fs::symlink_metadata(&link).unwrap().file_type().is_symlink());
        assert_eq!(fs::read(&link).unwrap(), b"new");
        assert_eq!(mode(&real), 0o640);
        assert_eq!(rewrite_preserving(&target, b"new").unwrap(), Outcome::Unchanged);
        assert_eq!(mode(&real), 0o640);
        assert!(
            rewrite_preserving(&link, b"x").is_err(),
            "callers resolve the link first"
        );
        assert_eq!(rewrite_target(&d.path().join("missing")).unwrap(), None);
        assert_eq!(
            fs::read_dir(real.parent().unwrap()).unwrap().count(),
            1,
            "no temp files left"
        );
    }

    #[test]
    fn atomic_and_new_writes() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("x.toml");
        assert_eq!(write_new(&p, b"one", 0o600).unwrap(), Outcome::Created);
        assert_eq!(write_new(&p, b"two", 0o600).unwrap(), Outcome::Unchanged);
        assert_eq!(fs::read(&p).unwrap(), b"one");
        assert_eq!(mode(&p), 0o600);
        assert_eq!(write_atomic(&p, b"one", 0o644).unwrap(), Outcome::Unchanged);
        assert_eq!(mode(&p), 0o644);
        assert_eq!(write_atomic(&p, b"two", 0o600).unwrap(), Outcome::Replaced);
        assert_eq!(fs::read(&p).unwrap(), b"two");
        assert_eq!(mode(&p), 0o600);
        assert_eq!(write_atomic(&d.path().join("y"), b"", 0o755).unwrap(), Outcome::Created);
        assert_eq!(fs::read_dir(d.path()).unwrap().count(), 2, "no temp files left");
    }

    #[test]
    fn symlinks() {
        let d = tempfile::tempdir().unwrap();
        let link = d.path().join("cinelog");
        assert_eq!(ensure_symlink(&link, Path::new("/a")).unwrap(), Outcome::Created);
        assert_eq!(ensure_symlink(&link, Path::new("/a")).unwrap(), Outcome::Unchanged);
        assert_eq!(ensure_symlink(&link, Path::new("/b")).unwrap(), Outcome::Replaced);
        assert_eq!(fs::read_link(&link).unwrap(), Path::new("/b"));
        let file = d.path().join("file");
        fs::write(&file, b"x").unwrap();
        assert!(ensure_symlink(&file, Path::new("/a")).is_err());
        assert!(remove_file(&link).unwrap());
        assert!(!remove_file(&link).unwrap());
    }

    #[test]
    fn pretty_paths() {
        assert_eq!(pretty(Path::new("/h/u/logs"), Path::new("/h/u")), "~/logs");
        assert_eq!(pretty(Path::new("/var/x"), Path::new("/h/u")), "/var/x");
        assert_eq!(pretty(Path::new("/var/x"), Path::new("/")), "/var/x");
    }
}
