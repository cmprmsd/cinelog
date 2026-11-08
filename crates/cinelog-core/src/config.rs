//! `config.toml` (PERSONAL) and `/etc/cinelog/team.toml` (TEAM), plan §10.5.
//! Every key is optional; missing keys take the defaults below.

use std::path::Path;

use cinelog_proto::{NestedMode, ShellOpts};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub paths: PathsConfig,
    pub record: RecordConfig,
    pub shell: ShellConfig,
    pub server: ServerConfig,
    pub index: IndexConfig,
    pub retention: RetentionConfig,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PathsConfig {
    /// Cast root. `~` is expanded. TEAM: pinned, default `/root/logs`.
    pub logs: String,
    /// State root; DBs go to `<state>/<host>/`. Empty: `$XDG_STATE_HOME/cinelog`.
    pub state: String,
}

impl Default for PathsConfig {
    fn default() -> Self {
        Self {
            logs: "~/logs".into(),
            state: String::new(),
        }
    }
}

/// Keystroke suppression rules (§5.5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum InputSuppress {
    /// Slave termios `!ECHO && ICANON`.
    PasswordPrompt,
    /// Last output line looks like a password/passphrase/PIN/token prompt.
    PromptHeuristic,
    /// Nothing is recorded while paused.
    Paused,
    /// DA/CPR/OSC replies and focus events are not keystrokes.
    TerminalResponses,
    /// Any process from `record.relays` in the foreground pgrp (opt-in, §15.1).
    Relay,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HidepidFallback {
    #[default]
    Start,
    Skip,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RecordConfig {
    pub input: bool,
    pub input_suppress: Vec<InputSuppress>,
    pub relays: Vec<String>,
    pub strip_markers: bool,
    /// `record` copies root/team content into the personal cast.
    pub nested: NestedMode,
    pub pause_on_multiplexer: Vec<String>,
    pub hidepid_fallback: HidepidFallback,
    pub inner_login: bool,
    pub mirror_cmdline: bool,
    pub rotate_mb: u64,
    pub rotate_days: u64,
    pub fsync_secs: u64,
}

impl Default for RecordConfig {
    fn default() -> Self {
        Self {
            input: true,
            input_suppress: vec![
                InputSuppress::PasswordPrompt,
                InputSuppress::PromptHeuristic,
                InputSuppress::Paused,
                InputSuppress::TerminalResponses,
            ],
            relays: [
                "sudo",
                "su",
                "doas",
                "ssh",
                "mosh-client",
                "docker",
                "podman",
                "kubectl",
                "tmux: client",
            ]
            .map(String::from)
            .to_vec(),
            strip_markers: true,
            nested: NestedMode::Pause,
            pause_on_multiplexer: ["tmux: client", "screen", "zellij"].map(String::from).to_vec(),
            hidepid_fallback: HidepidFallback::Start,
            inner_login: false,
            mirror_cmdline: true,
            rotate_mb: 512,
            rotate_days: 7,
            fsync_secs: 5,
        }
    }
}

impl RecordConfig {
    pub fn suppresses(&self, rule: InputSuppress) -> bool {
        self.input_suppress.contains(&rule)
    }
}

/// `[shell]`, passed to the inner shell as `CINELOG_OPTS`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ShellConfig {
    pub motd: bool,
    pub banners: bool,
    pub check_free_space: bool,
    pub external_ip: bool,
    pub title: bool,
    pub title_link: bool,
}

impl Default for ShellConfig {
    fn default() -> Self {
        let d = ShellOpts::default();
        Self {
            motd: d.motd,
            banners: d.banners,
            check_free_space: d.check_free_space,
            external_ip: d.external_ip,
            title: d.title,
            title_link: d.title_link,
        }
    }
}

impl ShellConfig {
    pub fn to_opts(&self, url: Option<&str>) -> ShellOpts {
        ShellOpts {
            motd: self.motd,
            banners: self.banners,
            check_free_space: self.check_free_space,
            external_ip: self.external_ip,
            title: self.title,
            title_link: self.title_link,
            url: url.unwrap_or_default().to_owned(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Autostart {
    /// systemd user unit if present, else flock autostart from the recorder.
    #[default]
    Auto,
    Systemd,
    Shell,
    Off,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ServerConfig {
    pub autostart: Autostart,
    /// `addr:port`; port `auto` = 10000 (TEAM) or 10000+uid / hashed (PERSONAL), §7.1.
    pub bind: Vec<String>,
    pub hostnames: Vec<String>,
    pub tls_cert: String,
    pub tls_key: String,
    pub tls_auto_regen: bool,
    pub session_idle_hours: u64,
    pub session_max_days: u64,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            autostart: Autostart::Auto,
            bind: vec!["0.0.0.0:auto".into(), "[::]:auto".into()],
            hostnames: Vec::new(),
            tls_cert: String::new(),
            tls_key: String::new(),
            tls_auto_regen: true,
            session_idle_hours: 12,
            session_max_days: 7,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct IndexConfig {
    pub output: bool,
    pub gap_output: bool,
    pub chunk_kb: u64,
    pub max_output_per_command_mb: u64,
    pub alt_snapshots: bool,
    pub playback_segment_threshold_mb: u64,
    /// Optional regexes; empty by default in both scopes (§15.7).
    pub redact: Vec<String>,
}

impl Default for IndexConfig {
    fn default() -> Self {
        Self {
            output: true,
            gap_output: true,
            chunk_kb: 16,
            max_output_per_command_mb: 64,
            alt_snapshots: true,
            playback_segment_threshold_mb: 8,
            redact: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RetentionConfig {
    pub compress_after_days: u64,
    pub zstd_level: i32,
}

impl Default for RetentionConfig {
    fn default() -> Self {
        Self {
            compress_after_days: 14,
            zstd_level: 9,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("reading {path}: {source}")]
    Io { path: String, source: std::io::Error },
    #[error("parsing {path}: {source}")]
    Parse { path: String, source: Box<toml::de::Error> },
}

impl Config {
    pub fn from_toml_str(s: &str) -> Result<Self, toml::de::Error> {
        toml::from_str(s)
    }

    /// Loads a config file; a missing file yields the defaults, any other error is returned
    /// (the recorder then falls back to an unrecorded shell, §5.1).
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        match std::fs::read_to_string(path) {
            Ok(s) => Self::from_toml_str(&s).map_err(|e| ConfigError::Parse {
                path: path.display().to_string(),
                source: Box::new(e),
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(ConfigError::Io {
                path: path.display().to_string(),
                source: e,
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_config_is_default() {
        assert_eq!(Config::from_toml_str("").unwrap(), Config::default());
    }

    #[test]
    fn partial_config_overrides() {
        let c = Config::from_toml_str(
            "[record]\nnested = \"record\"\ninput_suppress = [\"relay\"]\n[shell]\nmotd = false\n",
        )
        .unwrap();
        assert_eq!(c.record.nested, NestedMode::Record);
        assert!(c.record.suppresses(InputSuppress::Relay));
        assert!(!c.record.suppresses(InputSuppress::PasswordPrompt));
        assert!(!c.shell.motd);
        assert_eq!(c.record.rotate_mb, 512);
        assert!(c.index.redact.is_empty());
    }

    #[test]
    fn unknown_keys_are_ignored_but_bad_types_fail() {
        // A newer config must not stop an older recorder from recording.
        assert_eq!(
            Config::from_toml_str("[record]\nfuture = 1\n").unwrap(),
            Config::default()
        );
        assert!(Config::from_toml_str("[record]\ninput = \"yes\"\n").is_err());
    }

    #[test]
    fn missing_file_is_default() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(Config::load(&dir.path().join("nope.toml")).unwrap(), Config::default());
    }

    #[test]
    fn shell_config_to_opts() {
        let o = ShellConfig {
            banners: false,
            ..ShellConfig::default()
        }
        .to_opts(Some("https://h/s/1"));
        assert!(!o.banners && o.motd);
        assert_eq!(o.url, "https://h/s/1");
    }
}
