//! The zim module checkout (repository) the running binary belongs to: the source of the
//! plugin, `VERSION` and `shell/checksums/` for `install --team` and `doctor`.

use std::path::{Path, PathBuf};

pub const PLUGIN_FILE: &str = "cinelog.plugin.zsh";
/// The release binary `cinelog-fetch` puts into a module checkout.
pub const BINARY: &str = "bin/cinelog";

/// The running binary, without the ` (deleted)` suffix an upgrade leaves behind.
pub fn current_exe() -> std::io::Result<PathBuf> {
    let exe = std::fs::read_link("/proc/self/exe")?;
    let s = exe.to_string_lossy();
    Ok(match s.strip_suffix(" (deleted)") {
        Some(stripped) => PathBuf::from(stripped),
        None => exe,
    })
}

/// The full `sudo … install --team` command line for messages. A bare `sudo cinelog`
/// would usually find the installed team binary first (secure_path), not this one.
pub fn sudo_install_team(flags: &str) -> String {
    let exe = current_exe().map_or_else(|_| "cinelog".to_owned(), |p| p.display().to_string());
    format!("sudo {exe} install --team{flags}")
}

/// The nearest directory at or up to four levels above `exe`'s directory that contains
/// the plugin (`<module>/bin/cinelog`, `<repo>/target/release/cinelog`).
pub fn find_from_exe(exe: &Path) -> Option<PathBuf> {
    exe.parent()?
        .ancestors()
        .take(4)
        .find(|d| d.join(PLUGIN_FILE).is_file())
        .map(Path::to_path_buf)
}

/// `VERSION` of a module checkout.
pub fn version(module: &Path) -> Option<String> {
    let v = std::fs::read_to_string(module.join("VERSION")).ok()?;
    Some(v.trim().to_owned()).filter(|v| !v.is_empty())
}

/// `x.y.z` → comparable tuple; pre-release suffixes are ignored.
pub fn parse_version(v: &str) -> Option<(u64, u64, u64)> {
    let v = v.trim().trim_start_matches('v');
    let core = v.split(['-', '+']).next()?;
    let mut it = core.split('.').map(|p| p.parse::<u64>().ok());
    let parsed = (it.next()??, it.next()??, it.next()??);
    it.next().is_none().then_some(parsed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn module_discovery() {
        let dir = tempfile::tempdir().unwrap();
        let m = dir.path().join("cinelog");
        std::fs::create_dir_all(m.join("bin")).unwrap();
        std::fs::create_dir_all(m.join("target/release")).unwrap();
        std::fs::write(m.join(PLUGIN_FILE), "").unwrap();
        std::fs::write(m.join("VERSION"), "0.10.0\n").unwrap();
        assert_eq!(find_from_exe(&m.join("bin/cinelog")), Some(m.clone()));
        assert_eq!(find_from_exe(&m.join("target/release/cinelog")), Some(m.clone()));
        assert_eq!(find_from_exe(&dir.path().join("elsewhere/cinelog")), None);
        assert_eq!(version(&m).as_deref(), Some("0.10.0"));
    }

    #[test]
    fn versions() {
        assert_eq!(parse_version("0.10.0"), Some((0, 10, 0)));
        assert_eq!(parse_version("v1.2.3-rc1\n"), Some((1, 2, 3)));
        assert!(parse_version("0.10.0") > parse_version("0.9.12"));
        assert_eq!(parse_version("1.2"), None);
        assert_eq!(parse_version("1.2.3.4"), None);
    }

    #[test]
    fn exe_is_readable() {
        assert!(current_exe().unwrap().is_absolute());
        let cmd = sudo_install_team(" --upgrade");
        assert!(
            cmd.starts_with("sudo /") && cmd.ends_with(" install --team --upgrade"),
            "{cmd}"
        );
    }
}
