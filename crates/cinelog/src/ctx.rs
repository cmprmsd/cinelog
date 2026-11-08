//! Process context shared by the subcommands: identity, scope, config and paths.

use cinelog_core::config::ConfigError;
use cinelog_core::paths::{self, EnvSnapshot, Paths, SystemRoot};
use cinelog_core::registry::Registry;
use cinelog_core::{Config, Scope};
use cinelog_detect::{ProcDetector, RealProcFs};

#[derive(Debug)]
pub struct Ctx {
    pub env: EnvSnapshot,
    pub root: SystemRoot,
    /// The loaded config, or the defaults when it could not be loaded (see `config_error`).
    pub config: Config,
    pub config_error: Option<ConfigError>,
    pub paths: Paths,
}

impl Ctx {
    /// Context of the current process. At EUID 0 the environment is never consulted
    /// (root hardening, §5.6).
    pub fn load() -> Self {
        Self::with(EnvSnapshot::capture(), SystemRoot::real())
    }

    pub fn with(env: EnvSnapshot, root: SystemRoot) -> Self {
        let config_file = Paths::config_file_for(env.scope(), &env, &root);
        let (config, config_error) = match Config::load(&config_file) {
            Ok(c) => (c, None),
            Err(e) => (Config::default(), Some(e)),
        };
        let paths = Paths::resolve(&env, &root, &config.paths, &paths::hostname());
        Self {
            env,
            root,
            config,
            config_error,
            paths,
        }
    }

    pub fn scope(&self) -> Scope {
        self.env.scope()
    }

    pub fn registry(&self) -> Registry {
        Registry::new(&self.paths.runtime_dir)
    }

    /// `$CINELOG_DISABLE`, which only PERSONAL shells honour.
    pub fn env_disable(&self) -> Option<std::ffi::OsString> {
        (self.scope() == Scope::Personal)
            .then(|| std::env::var_os(cinelog_proto::env::DISABLE))
            .flatten()
    }

    pub fn is_disabled(&self) -> bool {
        self.paths.is_disabled(self.env_disable().as_deref())
    }

    pub fn detector(&self) -> ProcDetector<RealProcFs, Registry> {
        ProcDetector::new(RealProcFs::new(), self.registry())
    }
}
