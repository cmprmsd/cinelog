//! File locations per scope (plan §6.1) and cast file naming (§5.1).
//!
//! Root hardening (§5.6): at EUID 0 the environment (`HOME`, `XDG_*`, `CINELOG_*`) is never
//! consulted. TEAM paths are fixed system paths below a [`SystemRoot`], which is `/` in
//! production and a temporary prefix in tests (`install --team --prefix <dir>`).

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use cinelog_proto::Scope;

use crate::config::PathsConfig;
use crate::ids::SessionId;

/// Fixed TEAM locations, relative to the [`SystemRoot`].
pub mod team {
    pub const CONFIG_DIR: &str = "/etc/cinelog";
    pub const CONFIG: &str = "/etc/cinelog/team.toml";
    pub const DISABLE: &str = "/etc/cinelog/disable";
    pub const TLS_DIR: &str = "/etc/cinelog/tls";
    pub const TOKENS: &str = "/etc/cinelog/tokens.toml";
    pub const STATE_DIR: &str = "/var/lib/cinelog";
    pub const RUNTIME_DIR: &str = "/run/cinelog";
    pub const BINARY: &str = "/usr/local/bin/cinelog";
    pub const PLUGIN_COPY: &str = "/usr/local/share/cinelog/cinelog.plugin.zsh";
    pub const SHARE_DIR: &str = "/usr/local/share/cinelog";
    /// Debian and Arch system zshrc.
    pub const SYSTEM_ZSHRC: &str = "/etc/zsh/zshrc";
    pub const TMPFILES: &str = "/etc/tmpfiles.d/cinelog.conf";
    pub const UNIT: &str = "/etc/systemd/system/cinelog-team.service";
    /// Default log root when `paths.logs` is not set in team.toml.
    pub const LOGS: &str = "/root/logs";
}

/// Prefix for system paths: `/` in production, a temp dir in tests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SystemRoot(PathBuf);

impl SystemRoot {
    pub fn real() -> Self {
        Self(PathBuf::from("/"))
    }

    pub fn prefixed(prefix: impl Into<PathBuf>) -> Self {
        Self(prefix.into())
    }

    pub fn is_real(&self) -> bool {
        self.0 == Path::new("/")
    }

    pub fn prefix(&self) -> &Path {
        &self.0
    }

    /// Maps an absolute system path below this root.
    pub fn join(&self, abs: impl AsRef<Path>) -> PathBuf {
        let abs = abs.as_ref();
        self.0.join(abs.strip_prefix("/").unwrap_or(abs))
    }
}

impl Default for SystemRoot {
    fn default() -> Self {
        Self::real()
    }
}

/// The process identity and the environment values paths may depend on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvSnapshot {
    pub uid: u32,
    pub euid: u32,
    /// Name of the effective user.
    pub user: String,
    /// PERSONAL: `$HOME` (falls back to the passwd entry). EUID 0: always the passwd entry.
    pub home: PathBuf,
    pub xdg_config_home: Option<PathBuf>,
    pub xdg_state_home: Option<PathBuf>,
    pub xdg_runtime_dir: Option<PathBuf>,
}

impl EnvSnapshot {
    /// Captures the current process. At EUID 0 the environment is ignored entirely.
    pub fn capture() -> Self {
        let uid = nix::unistd::getuid().as_raw();
        let euid = nix::unistd::geteuid().as_raw();
        let pw = nix::unistd::User::from_uid(nix::unistd::Uid::from_raw(euid))
            .ok()
            .flatten();
        let user = pw.as_ref().map(|u| u.name.clone()).unwrap_or_else(|| euid.to_string());
        let pw_home = pw
            .map(|u| u.dir)
            .unwrap_or_else(|| PathBuf::from(if euid == 0 { "/root" } else { "/" }));
        if euid == 0 {
            return Self {
                uid,
                euid,
                user,
                home: pw_home,
                xdg_config_home: None,
                xdg_state_home: None,
                xdg_runtime_dir: None,
            };
        }
        let abs = |k: &str| std::env::var_os(k).map(PathBuf::from).filter(|p| p.is_absolute());
        Self {
            uid,
            euid,
            user,
            home: abs("HOME").unwrap_or(pw_home),
            xdg_config_home: abs("XDG_CONFIG_HOME"),
            xdg_state_home: abs("XDG_STATE_HOME"),
            xdg_runtime_dir: abs("XDG_RUNTIME_DIR"),
        }
    }

    pub fn scope(&self) -> Scope {
        Scope::for_euid(self.euid)
    }

    pub fn config_home(&self) -> PathBuf {
        self.xdg_config_home
            .clone()
            .unwrap_or_else(|| self.home.join(".config"))
    }

    pub fn state_home(&self) -> PathBuf {
        self.xdg_state_home
            .clone()
            .unwrap_or_else(|| self.home.join(".local/state"))
    }
}

/// Resolved locations for one scope.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Paths {
    pub scope: Scope,
    pub config_file: PathBuf,
    /// Kill switch file (PERSONAL: `~/.config/cinelog/disable`, TEAM: `/etc/cinelog/disable`).
    pub disable_file: PathBuf,
    /// Cast root. TEAM casts go to `<logs_root>/<sudo_user>/YYYY-MM/`.
    pub logs_root: PathBuf,
    pub state_dir: PathBuf,
    /// Directory of `index.db` and `meta.db` (PERSONAL: per host).
    pub db_dir: PathBuf,
    /// Registry files, `serve.lock`, `server.json`. PERSONAL: `$XDG_RUNTIME_DIR/cinelog`,
    /// else `$XDG_STATE_HOME/cinelog/run` (never from config: the plugin derives it from env).
    pub runtime_dir: PathBuf,
    pub rec_log: PathBuf,
    pub tls_dir: PathBuf,
    /// PERSONAL `auth.toml`, TEAM `tokens.toml`.
    pub auth_file: PathBuf,
}

impl Paths {
    /// Config file location, needed before the config (and so `[paths]`) is known.
    pub fn config_file_for(scope: Scope, env: &EnvSnapshot, root: &SystemRoot) -> PathBuf {
        match scope {
            Scope::Team => root.join(team::CONFIG),
            Scope::Personal => env.config_home().join("cinelog/config.toml"),
        }
    }

    pub fn personal(env: &EnvSnapshot, cfg: &PathsConfig, host: &str) -> Self {
        let conf = env.config_home().join("cinelog");
        let state_dir = if cfg.state.is_empty() {
            env.state_home().join("cinelog")
        } else {
            expand_tilde(&cfg.state, &env.home)
        };
        let runtime_dir = env
            .xdg_runtime_dir
            .as_ref()
            .map(|d| d.join("cinelog"))
            .unwrap_or_else(|| env.state_home().join("cinelog/run"));
        Self {
            scope: Scope::Personal,
            config_file: conf.join("config.toml"),
            disable_file: conf.join("disable"),
            logs_root: expand_tilde(&cfg.logs, &env.home),
            db_dir: state_dir.join(sanitize_component(host)),
            rec_log: state_dir.join("rec.log"),
            state_dir,
            runtime_dir,
            tls_dir: conf.join("tls"),
            auth_file: conf.join("auth.toml"),
        }
    }

    /// TEAM paths. `root_home` is root's passwd home (used only to expand `~` in team.toml).
    pub fn team(root: &SystemRoot, cfg: &PathsConfig, root_home: &Path) -> Self {
        let logs = if cfg.logs.is_empty() {
            team::LOGS.into()
        } else {
            expand_tilde(&cfg.logs, root_home)
        };
        let state_dir = root.join(team::STATE_DIR);
        Self {
            scope: Scope::Team,
            config_file: root.join(team::CONFIG),
            disable_file: root.join(team::DISABLE),
            logs_root: root.join(logs),
            db_dir: state_dir.clone(),
            rec_log: state_dir.join("rec.log"),
            state_dir,
            runtime_dir: root.join(team::RUNTIME_DIR),
            tls_dir: root.join(team::TLS_DIR),
            auth_file: root.join(team::TOKENS),
        }
    }

    /// Resolves the paths of the scope implied by `env.euid`.
    pub fn resolve(env: &EnvSnapshot, root: &SystemRoot, cfg: &PathsConfig, host: &str) -> Self {
        match env.scope() {
            Scope::Team => Self::team(root, cfg, &env.home),
            Scope::Personal => Self::personal(env, cfg, host),
        }
    }

    /// Kill switch, TEAM: only the file. PERSONAL: the file or a non-empty
    /// `$CINELOG_DISABLE` (`env_disable`, read by the caller; ignored at EUID 0).
    pub fn is_disabled(&self, env_disable: Option<&std::ffi::OsStr>) -> bool {
        let env = self.scope == Scope::Personal && env_disable.is_some_and(|v| !v.is_empty());
        env || self.disable_file.exists()
    }

    /// `server.json` of this scope's server.
    pub fn server_json(&self) -> PathBuf {
        crate::server::ServerInfo::path(&self.runtime_dir)
    }

    /// Port carried over from the old `cinelog-settings.conf` by `install`. `serve` moves it
    /// into `meta.settings` (the persisted port, §7.1) on its first start.
    pub fn migrated_port_file(&self) -> PathBuf {
        self.db_dir.join("migrated-port")
    }

    /// Directory for a new cast: `<logs>/YYYY-MM` or, TEAM, `<logs>/<sudo_user>/YYYY-MM`.
    pub fn cast_dir(&self, sudo_user: Option<&str>, start: &jiff::Zoned) -> PathBuf {
        let base = match self.scope {
            Scope::Team => self.logs_root.join(sanitize_component(sudo_user.unwrap_or("root"))),
            Scope::Personal => self.logs_root.clone(),
        };
        base.join(month_dir(start))
    }
}

/// `YYYY-MM` of the (local) start time.
pub fn month_dir(start: &jiff::Zoned) -> String {
    start.strftime("%Y-%m").to_string()
}

/// `<host>_<YYYYmmddTHHMMSS.ffffff>_<uuid8>.cast`, or `…_<uuid8>_part<N>.cast` for N ≥ 2.
/// The time is the session's local start time; all parts keep the first part's stem.
pub fn cast_file_name(host: &str, start: &jiff::Zoned, id: &SessionId, part: u32) -> String {
    let stem = format!(
        "{}_{}_{}",
        sanitize_component(host),
        start.strftime("%Y%m%dT%H%M%S.%6f"),
        id.short()
    );
    if part <= 1 {
        format!("{stem}.cast")
    } else {
        format!("{stem}_part{part}.cast")
    }
}

/// Keeps `[A-Za-z0-9._-]`, maps everything else to `-`; empty, `.` and `..` become `unknown`.
pub fn sanitize_component(s: &str) -> String {
    let out: String = s
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '-'
            }
        })
        .collect();
    if out.is_empty() || out == "." || out == ".." {
        "unknown".into()
    } else {
        out
    }
}

/// Expands a leading `~` or `~/`; other paths are returned unchanged.
pub fn expand_tilde(s: &str, home: &Path) -> PathBuf {
    if s == "~" {
        home.to_path_buf()
    } else if let Some(rest) = s.strip_prefix("~/") {
        home.join(rest)
    } else {
        PathBuf::from(s)
    }
}

/// Short host name (as `uname -n`), `unknown` if unavailable.
pub fn hostname() -> String {
    nix::unistd::gethostname()
        .ok()
        .map(OsString::into_string)
        .and_then(Result::ok)
        .filter(|h| !h.is_empty())
        .unwrap_or_else(|| "unknown".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(euid: u32) -> EnvSnapshot {
        EnvSnapshot {
            uid: euid,
            euid,
            user: "alice".into(),
            home: "/home/alice".into(),
            xdg_config_home: None,
            xdg_state_home: Some("/xdg/state".into()),
            xdg_runtime_dir: Some("/run/user/1000".into()),
        }
    }

    #[test]
    fn personal_defaults() {
        let p = Paths::personal(&env(1000), &PathsConfig::default(), "box/1");
        assert_eq!(p.config_file, Path::new("/home/alice/.config/cinelog/config.toml"));
        assert_eq!(p.logs_root, Path::new("/home/alice/logs"));
        assert_eq!(p.db_dir, Path::new("/xdg/state/cinelog/box-1"));
        assert_eq!(p.runtime_dir, Path::new("/run/user/1000/cinelog"));
        let mut e = env(1000);
        e.xdg_runtime_dir = None;
        let cfg = PathsConfig {
            state: "/custom".into(),
            ..PathsConfig::default()
        };
        let p = Paths::personal(&e, &cfg, "h");
        assert_eq!(p.db_dir, Path::new("/custom/h"));
        assert_eq!(p.runtime_dir, Path::new("/xdg/state/cinelog/run"));
    }

    #[test]
    fn team_paths_live_under_the_root_prefix() {
        let root = SystemRoot::prefixed("/tmp/t");
        let p = Paths::team(
            &root,
            &PathsConfig {
                logs: String::new(),
                state: String::new(),
            },
            Path::new("/root"),
        );
        assert_eq!(p.logs_root, Path::new("/tmp/t/root/logs"));
        assert_eq!(p.config_file, Path::new("/tmp/t/etc/cinelog/team.toml"));
        let p = Paths::team(&root, &PathsConfig::default(), Path::new("/root"));
        assert_eq!(p.logs_root, Path::new("/tmp/t/root/logs"));
        let start = jiff::Timestamp::from_second(1_731_070_000)
            .unwrap()
            .to_zoned(jiff::tz::TimeZone::UTC);
        assert_eq!(
            p.cast_dir(Some("../bob"), &start),
            Path::new("/tmp/t/root/logs/..-bob/2024-11")
        );
    }

    #[test]
    fn kill_switch() {
        let dir = tempfile::tempdir().unwrap();
        let mut e = env(1000);
        e.xdg_config_home = Some(dir.path().into());
        let p = Paths::personal(&e, &PathsConfig::default(), "h");
        assert!(!p.is_disabled(None));
        assert!(!p.is_disabled(Some("".as_ref())));
        assert!(p.is_disabled(Some("1".as_ref())));
        std::fs::create_dir_all(p.disable_file.parent().unwrap()).unwrap();
        std::fs::write(&p.disable_file, b"").unwrap();
        assert!(p.is_disabled(None));

        let root = SystemRoot::prefixed(dir.path().join("sys"));
        let t = Paths::team(&root, &PathsConfig::default(), Path::new("/root"));
        assert!(!t.is_disabled(Some("1".as_ref())), "root ignores CINELOG_DISABLE");
        assert_eq!(t.server_json(), dir.path().join("sys/run/cinelog/server.json"));
    }

    #[test]
    fn cast_names() {
        let start = jiff::Timestamp::from_microsecond(1_731_070_000_123_456)
            .unwrap()
            .to_zoned(jiff::tz::TimeZone::UTC);
        let id: SessionId = "01930a3e-0000-7000-8000-00000000abcd".parse().unwrap();
        assert_eq!(
            cast_file_name("box", &start, &id, 1),
            "box_20241108T124640.123456_0000abcd.cast"
        );
        assert_eq!(
            cast_file_name("box", &start, &id, 3),
            "box_20241108T124640.123456_0000abcd_part3.cast"
        );
    }

    #[test]
    fn sanitize_and_tilde() {
        assert_eq!(sanitize_component(".."), "unknown");
        assert_eq!(sanitize_component("a b/c"), "a-b-c");
        assert_eq!(expand_tilde("~/x", Path::new("/h")), Path::new("/h/x"));
        assert_eq!(expand_tilde("/abs", Path::new("/h")), Path::new("/abs"));
    }
}
