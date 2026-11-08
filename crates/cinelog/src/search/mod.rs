//! `cinelog search`, `show`, `forget`, `index`, `status` and `import --report` (work package
//! M2-E; plan §9.5, §6.4 "Status", §11.8). The `hist` shim in the plugin calls `search`.
//!
//! Search uses [`cinelog_index::db::open_reader`] on the scope's DB (personal, or the team DB
//! at EUID 0), forget [`cinelog_index::db::open_meta`], index [`crate::serve::open_engine`].
//!
//! Frozen entry points (called from `main.rs`): [`run`], [`show`], [`forget`], [`index`],
//! [`status`] and [`import_report`]. The bodies are the pre-integration stand-ins of the
//! contracts commit.

use std::process::ExitCode;

use cinelog_index::model::OnceOptions;

use crate::cli::{ForgetArgs, ImportArgs, IndexArgs, SearchArgs, ShowArgs, StatusArgs};
use crate::ctx::Ctx;

pub fn run(args: &SearchArgs) -> ExitCode {
    let _ = args;
    unavailable("search")
}

pub fn show(args: &ShowArgs) -> ExitCode {
    let _ = args;
    unavailable("show")
}

pub fn forget(args: &ForgetArgs) -> ExitCode {
    let _ = args;
    unavailable("forget")
}

pub fn index(args: &IndexArgs) -> ExitCode {
    let ctx = Ctx::load();
    let opts = OnceOptions {
        rebuild: args.rebuild,
        paths: args.paths.clone(),
    };
    match crate::serve::open_engine(&ctx).and_then(|mut e| e.index_once(&opts)) {
        Ok(summary) => {
            println!(
                "indexed {} of {} casts ({} unchanged, {} failed)",
                summary.indexed, summary.casts_seen, summary.unchanged, summary.failed
            );
            if summary.failed == 0 {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            }
        }
        Err(e) => {
            eprintln!("cinelog: index: {e}");
            ExitCode::from(2)
        }
    }
}

pub fn status(args: &StatusArgs) -> ExitCode {
    let _ = args;
    unavailable("status")
}

pub fn import_report(args: &ImportArgs) -> ExitCode {
    let _ = args;
    unavailable("import --report")
}

fn unavailable(what: &str) -> ExitCode {
    eprintln!("cinelog: `{what}` is not available in this build yet");
    ExitCode::from(2)
}
