//! cinelog core: configuration, scopes, paths, the recorder registry, ids, clock and
//! cinelog's own logging. Shared by the recorder, the CLI and (later) the server.

pub mod clock;
pub mod config;
pub mod ids;
pub mod location;
pub mod logging;
pub mod paths;
pub mod registry;
pub mod server;
pub mod tty;

pub use cinelog_proto::{Nonce, Scope};
pub use config::Config;
pub use ids::SessionId;
pub use paths::{EnvSnapshot, Paths, SystemRoot};
pub use registry::{RecState, RegistryEntry, TtyKey};

/// Version of this build; equals the repository `VERSION` file.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
