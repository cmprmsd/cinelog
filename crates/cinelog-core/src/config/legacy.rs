//! The pre-rewrite `~/.config/cinelog/cinelog-settings.conf` (a sourced shell file).
//!
//! It is parsed as plain `KEY=VALUE` text and never executed. `install` reads it once, when
//! it creates `config.toml` (§10.5); the file itself is left in place.
//! `CINELOG_AUTH_UUID` is deliberately not carried over: it leaked into casts, and the new
//! server issues fresh tokens.

use std::path::{Path, PathBuf};

use super::template::Override;

pub const FILE_NAME: &str = "cinelog-settings.conf";

/// Where the old plugin kept the file: `$HOME/.config/cinelog/` (it ignored XDG).
pub fn path_in_home(home: &Path) -> PathBuf {
    home.join(".config/cinelog").join(FILE_NAME)
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LegacySettings {
    pub motd: Option<bool>,
    pub banners: Option<bool>,
    pub check_free_space: Option<bool>,
    pub external_ip: Option<bool>,
    pub viewer_port: Option<u16>,
    /// An auth uuid was present (and is ignored).
    pub had_auth_uuid: bool,
    /// Keys that exist in the old format but have no successor (`USE_PER_FOLDER_HISTORY`).
    pub dropped: Vec<String>,
    /// Lines that could not be understood, as `KEY` names or line numbers.
    pub ignored: Vec<String>,
}

impl LegacySettings {
    pub fn parse(text: &str) -> Self {
        let mut s = Self::default();
        for (no, raw) in text.lines().enumerate() {
            let line = raw.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let line = line.strip_prefix("export ").map_or(line, str::trim_start);
            let Some((key, value)) = line.split_once('=') else {
                s.ignored.push(format!("line {}", no + 1));
                continue;
            };
            let key = key.trim();
            let value = unquote(value.trim());
            let flag = || match value {
                "1" | "true" | "yes" => Some(true),
                "0" | "false" | "no" => Some(false),
                _ => None,
            };
            let ok = match key {
                "MOTD_ENABLED" => set(&mut s.motd, flag()),
                "SHOW_START_END_TIME" => set(&mut s.banners, flag()),
                "CHECK_FREE_SPACE" => set(&mut s.check_free_space, flag()),
                "GET_EXTERNAL_IP" => set(&mut s.external_ip, flag()),
                "CINELOG_VIEWER_PORT" => set(&mut s.viewer_port, value.parse().ok().filter(|&p| p >= 1024)),
                "CINELOG_AUTH_UUID" => {
                    s.had_auth_uuid = true;
                    true
                }
                "USE_PER_FOLDER_HISTORY" => {
                    s.dropped.push(key.to_owned());
                    true
                }
                _ => false,
            };
            if !ok {
                s.ignored.push(key.to_owned());
            }
        }
        s
    }

    /// The `[shell]` keys to set in the new config.
    pub fn shell_overrides(&self) -> Vec<Override<'static>> {
        [
            ("motd", self.motd),
            ("banners", self.banners),
            ("check_free_space", self.check_free_space),
            ("external_ip", self.external_ip),
        ]
        .into_iter()
        .filter_map(|(k, v)| v.map(|v| ("shell", k, toml::Value::Boolean(v))))
        .collect()
    }
}

fn set<T>(slot: &mut Option<T>, value: Option<T>) -> bool {
    let ok = value.is_some();
    if ok {
        *slot = value;
    }
    ok
}

fn unquote(v: &str) -> &str {
    // A trailing shell comment is not part of the value.
    let v = match v.find(" #") {
        Some(i) if !v.starts_with(['"', '\'']) => v[..i].trim_end(),
        _ => v,
    };
    for q in ['"', '\''] {
        if let Some(inner) = v.strip_prefix(q).and_then(|r| r.strip_suffix(q)) {
            return inner;
        }
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The file exactly as the old plugin generated it.
    const OLD: &str = r#"############## Config ###############
# Cinelog Viewer Port
export CINELOG_VIEWER_PORT=11000
# Authentication UUID
export CINELOG_AUTH_UUID="0f8fad5b-d9cb-469f-a165-70867728950e"
# Show MOTD
MOTD_ENABLED=1
# Show start and end time ([YYYY-MM-DD HH:MM:SS])
SHOW_START_END_TIME=0
# Per-folder Zsh history
USE_PER_FOLDER_HISTORY=1
# Check each mount point for free space with exceptions for tmpfs and cdroms
CHECK_FREE_SPACE=1
# Get external IP
GET_EXTERNAL_IP=1
"#;

    #[test]
    fn parses_the_generated_file() {
        let s = LegacySettings::parse(OLD);
        assert_eq!(s.motd, Some(true));
        assert_eq!(s.banners, Some(false));
        assert_eq!(s.check_free_space, Some(true));
        assert_eq!(s.external_ip, Some(true));
        assert_eq!(s.viewer_port, Some(11000));
        assert!(s.had_auth_uuid);
        assert_eq!(s.dropped, ["USE_PER_FOLDER_HISTORY"]);
        assert!(s.ignored.is_empty());
        let o = s.shell_overrides();
        assert_eq!(o.len(), 4);
        assert!(o.contains(&("shell", "banners", toml::Value::Boolean(false))));
    }

    #[test]
    fn the_auth_uuid_never_reaches_the_result() {
        let s = LegacySettings::parse(OLD);
        assert!(!format!("{s:?}").contains("0f8fad5b"));
    }

    #[test]
    fn tolerant_of_odd_lines() {
        let s = LegacySettings::parse(
            "MOTD_ENABLED='0'  \nexport  GET_EXTERNAL_IP=maybe\nCHECK_FREE_SPACE=0 # off\nfoo\nCINELOG_VIEWER_PORT=80\nX=1\n",
        );
        assert_eq!(s.motd, Some(false));
        assert_eq!(s.external_ip, None);
        assert_eq!(s.check_free_space, Some(false));
        assert_eq!(s.viewer_port, None);
        assert_eq!(s.ignored, ["GET_EXTERNAL_IP", "line 4", "CINELOG_VIEWER_PORT", "X"]);
        assert!(LegacySettings::parse("").shell_overrides().is_empty());
    }
}
