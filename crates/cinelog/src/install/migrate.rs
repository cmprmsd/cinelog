//! Writing `config.toml`/`team.toml`, carrying over the old `cinelog-settings.conf` once.

use std::path::Path;

use anyhow::{Context, Result};
use cinelog_core::Config;
use cinelog_core::Scope;
use cinelog_core::config::{legacy::LegacySettings, template};

use super::fsutil::{self, Outcome, Symlinks};

#[derive(Debug, Default)]
pub struct ConfigOutcome {
    /// Whether the config file was created now.
    pub created: bool,
    pub notes: Vec<String>,
    /// `CINELOG_VIEWER_PORT` of the old settings, to be persisted (§10.5).
    pub legacy_port: Option<u16>,
}

/// Creates `config_file` from the commented template unless it exists. A config that
/// exists must parse; it is never overwritten. When the file is created and `legacy` names
/// an existing old settings file, its `[shell]` values are migrated.
pub fn ensure_config(
    scope: Scope,
    config_file: &Path,
    legacy: Option<&Path>,
    symlinks: Symlinks,
) -> Result<ConfigOutcome> {
    let mut out = ConfigOutcome::default();
    if config_file.exists() {
        Config::load(config_file).with_context(|| "fix or remove it, then run install again".to_owned())?;
        match fsutil::tighten_file(config_file, 0o600, symlinks)? {
            Outcome::Tightened(old) => out
                .notes
                .push(format!("kept {}, tightened from {old:o} to 600", config_file.display())),
            _ => out.notes.push(format!("kept {}", config_file.display())),
        }
        return Ok(out);
    }
    let mut overrides = Vec::new();
    if let Some(path) = legacy.filter(|p| p.is_file()) {
        let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let old = LegacySettings::parse(&text);
        overrides = old.shell_overrides();
        let keys: Vec<&str> = overrides.iter().map(|(_, k, _)| *k).collect();
        out.notes.push(format!(
            "migrated [shell] {} from {} (the file is left in place)",
            if keys.is_empty() {
                "nothing".to_owned()
            } else {
                keys.join(", ")
            },
            path.display()
        ));
        if old.had_auth_uuid {
            out.notes
                .push("CINELOG_AUTH_UUID is not reused: the new server issues fresh tokens".into());
        }
        for k in &old.dropped {
            out.notes.push(format!("{k} has no successor and is ignored"));
        }
        if !old.ignored.is_empty() {
            out.notes
                .push(format!("ignored unknown settings: {}", old.ignored.join(", ")));
        }
        out.legacy_port = old.viewer_port;
    }
    if let Some(dir) = config_file.parent() {
        fsutil::ensure_dir_with(dir, 0o700, symlinks)?;
    }
    let text = template::render(scope, &overrides);
    out.created = fsutil::write_new(config_file, text.as_bytes(), 0o600)? == Outcome::Created;
    out.notes.push(format!("wrote {}", config_file.display()));
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn creates_once_and_migrates() {
        let d = tempfile::tempdir().unwrap();
        let legacy = d.path().join("cinelog-settings.conf");
        std::fs::write(
            &legacy,
            "export CINELOG_VIEWER_PORT=11000\nexport CINELOG_AUTH_UUID=\"secret-uuid\"\nMOTD_ENABLED=0\nUSE_PER_FOLDER_HISTORY=1\n",
        )
        .unwrap();
        let cfg = d.path().join("conf/cinelog/config.toml");
        let out = ensure_config(Scope::Personal, &cfg, Some(&legacy), Symlinks::Refuse).unwrap();
        assert!(out.created);
        assert_eq!(out.legacy_port, Some(11000));
        let notes = out.notes.join("\n");
        assert!(notes.contains("migrated [shell] motd"), "{notes}");
        assert!(!notes.contains("secret-uuid"));
        let c = Config::load(&cfg).unwrap();
        assert!(!c.shell.motd && c.shell.banners);
        let text = std::fs::read_to_string(&cfg).unwrap();
        assert!(!text.contains("secret-uuid"));

        // A second run keeps the file, even if the old settings changed.
        std::fs::write(&legacy, "MOTD_ENABLED=1\n").unwrap();
        let again = ensure_config(Scope::Personal, &cfg, Some(&legacy), Symlinks::Refuse).unwrap();
        assert!(!again.created && again.legacy_port.is_none());
        assert_eq!(std::fs::read_to_string(&cfg).unwrap(), text);
    }

    #[test]
    fn an_existing_world_readable_config_is_tightened() {
        use std::os::unix::fs::PermissionsExt;
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o7777;
        let d = tempfile::tempdir().unwrap();
        let cfg = d.path().join("config.toml");
        std::fs::write(&cfg, "[record]\n").unwrap();
        std::fs::set_permissions(&cfg, std::fs::Permissions::from_mode(0o644)).unwrap();
        let out = ensure_config(Scope::Personal, &cfg, None, Symlinks::Refuse).unwrap();
        assert_eq!(mode(&cfg), 0o600);
        assert!(out.notes.join("\n").contains("tightened from 644"), "{:?}", out.notes);
        assert_eq!(std::fs::read_to_string(&cfg).unwrap(), "[record]\n");

        // Narrower modes are kept as they are.
        std::fs::set_permissions(&cfg, std::fs::Permissions::from_mode(0o400)).unwrap();
        ensure_config(Scope::Personal, &cfg, None, Symlinks::Refuse).unwrap();
        assert_eq!(mode(&cfg), 0o400);

        // A symlinked config: followed only when allowed, and the target is tightened.
        let real = d.path().join("dotfiles/config.toml");
        std::fs::create_dir_all(real.parent().unwrap()).unwrap();
        std::fs::write(&real, "").unwrap();
        std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o664)).unwrap();
        let link = d.path().join("linked.toml");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        assert!(ensure_config(Scope::Team, &link, None, Symlinks::Refuse).is_err());
        assert_eq!(mode(&real), 0o664);
        let uid = nix::unistd::geteuid().as_raw();
        ensure_config(Scope::Personal, &link, None, Symlinks::FollowOwnedBy(uid)).unwrap();
        assert_eq!(mode(&real), 0o600);
        let other = Symlinks::FollowOwnedBy(uid.wrapping_add(1));
        std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o664)).unwrap();
        assert!(ensure_config(Scope::Personal, &link, None, other).is_err());
    }

    #[test]
    fn broken_existing_config_is_an_error_and_kept() {
        let d = tempfile::tempdir().unwrap();
        let cfg = d.path().join("config.toml");
        std::fs::write(&cfg, "[record\n").unwrap();
        let err = ensure_config(Scope::Personal, &cfg, None, Symlinks::Refuse).unwrap_err();
        assert!(format!("{err:#}").contains("fix or remove it"));
        assert_eq!(std::fs::read_to_string(&cfg).unwrap(), "[record\n");
    }

    #[test]
    fn team_config_without_legacy() {
        let d = tempfile::tempdir().unwrap();
        let cfg = d.path().join("etc/cinelog/team.toml");
        let out = ensure_config(Scope::Team, &cfg, Some(&d.path().join("missing")), Symlinks::Refuse).unwrap();
        assert!(out.created);
        assert_eq!(Config::load(&cfg).unwrap(), Config::default());
        assert!(std::fs::read_to_string(&cfg).unwrap().contains("/root/logs"));
    }
}
