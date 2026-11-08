//! Shared server state handed to every handler and middleware (contracts, frozen for M3).

use std::ops::Deref;
use std::path::Path;
use std::sync::Arc;

use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use cinelog_core::paths::{EnvSnapshot, Paths};
use cinelog_core::{Config, Scope};
use cinelog_index::model::DbPaths;

use crate::error::WebError;
use crate::{auth, hosts, play};

/// Who the server is and where its data lives; built by `cinelog serve` from its `Ctx`.
#[derive(Debug, Clone)]
pub struct WebContext {
    pub scope: Scope,
    /// `sessions.host` of local casts, and the default hostname of URLs.
    pub host: String,
    /// The user running the server (PERSONAL: the token principal; TEAM: `root`).
    pub user: String,
    pub uid: u32,
    pub config: Config,
    pub paths: Paths,
    pub db: DbPaths,
}

impl WebContext {
    pub fn new(env: &EnvSnapshot, config: Config, paths: Paths, host: String) -> Self {
        let db = DbPaths::in_dir(&paths.db_dir);
        Self {
            scope: paths.scope,
            host,
            user: env.user.clone(),
            uid: env.euid,
            config,
            paths,
            db,
        }
    }

    /// A PERSONAL context with every path below `root` (tests).
    pub fn for_tests(root: &Path) -> Self {
        let conf = root.join("config/cinelog");
        let state = root.join("state/cinelog");
        let paths = Paths {
            scope: Scope::Personal,
            config_file: conf.join("config.toml"),
            disable_file: conf.join("disable"),
            logs_root: root.join("logs"),
            db_dir: state.join("testhost"),
            runtime_dir: root.join("run/cinelog"),
            rec_log: state.join("rec.log"),
            state_dir: state,
            tls_dir: conf.join("tls"),
            auth_file: conf.join("auth.toml"),
        };
        let db = DbPaths::in_dir(&paths.db_dir);
        Self {
            scope: Scope::Personal,
            host: "testhost".into(),
            user: "tester".into(),
            uid: 1000,
            config: Config::default(),
            paths,
            db,
        }
    }
}

/// Facts known once the listeners are bound and TLS is loaded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bound {
    pub port: u16,
    /// Certificate fingerprint (`tls::fingerprint` format).
    pub fingerprint: String,
    /// Base URL without a trailing slash (`https://host:port`).
    pub url: String,
}

pub struct Shared {
    pub ctx: WebContext,
    pub bound: Bound,
    /// ts the server started.
    pub started_at: f64,
    pub auth: auth::Auth,
    pub hosts: hosts::HostAllowlist,
    pub play: play::PlayState,
}

/// Cheap to clone; derefs to [`Shared`].
#[derive(Clone)]
pub struct AppState(Arc<Shared>);

impl AppState {
    /// `hosts` is passed in because TLS needs its names before the state exists.
    pub fn new(ctx: WebContext, bound: Bound, hosts: hosts::HostAllowlist) -> Self {
        let auth = auth::Auth::new(&ctx, bound.port);
        let play = play::PlayState::new(&ctx);
        Self(Arc::new(Shared {
            started_at: crate::now(),
            ctx,
            bound,
            auth,
            hosts,
            play,
        }))
    }

    /// A state for handler tests: [`WebContext::for_tests`] below `root`, port 10443.
    pub fn for_tests(root: &Path) -> Self {
        let ctx = WebContext::for_tests(root);
        let hosts = hosts::HostAllowlist::new(&ctx);
        Self::new(
            ctx,
            Bound {
                port: 10443,
                fingerprint: "00".into(),
                url: "https://testhost:10443".into(),
            },
            hosts,
        )
    }
}

impl Deref for AppState {
    type Target = Shared;
    fn deref(&self) -> &Shared {
        &self.0
    }
}

/// The authenticated principal (token name). `auth::require_session` inserts it as a
/// request extension; extracting it on a route without that middleware yields 401.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Principal(pub String);

impl<S: Send + Sync> FromRequestParts<S> for Principal {
    type Rejection = WebError;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        parts.extensions.get::<Self>().cloned().ok_or(WebError::Unauthorized)
    }
}
