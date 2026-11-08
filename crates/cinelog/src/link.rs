//! `cinelog link [--json]`: session id, cast path and (once a server runs) the web URL.

use std::fmt::Write as _;
use std::process::ExitCode;

use cinelog_core::registry::{RecState, RegistryEntry};
use cinelog_core::server::ServerInfo;
use serde_json::json;

use crate::cli::LinkArgs;
use crate::ctx::Ctx;
use crate::session;

pub const fn state_name(state: RecState) -> &'static str {
    match state {
        RecState::Live => "live",
        RecState::Paused => "paused",
        RecState::Degraded => "degraded",
    }
}

pub fn render(entry: &RegistryEntry, server: Option<&ServerInfo>, as_json: bool) -> String {
    let url = server.map(|s| s.session_url(&entry.uuid));
    if as_json {
        let state = state_name(entry.state);
        let v = json!({ "uuid": entry.uuid, "cast": entry.cast, "url": url, "state": state });
        return format!("{v}\n");
    }
    let mut out = format!("session {}\ncast {}\n", entry.uuid, entry.cast.display());
    if let Some(url) = url {
        let _ = writeln!(out, "url {url}");
    }
    out
}

pub fn run(args: &LinkArgs) -> ExitCode {
    let ctx = Ctx::load();
    match session::current(&ctx) {
        Ok(cur) => {
            let server = ServerInfo::read_live(&ctx.paths.runtime_dir);
            print!("{}", render(&cur.entry, server.as_ref(), args.json));
            if !args.json && cur.entry.state != RecState::Live {
                let reason = cur.entry.state_reason.as_deref().unwrap_or("no reason given");
                eprintln!("cinelog: recording is {} ({reason})", state_name(cur.entry.state));
            }
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("cinelog: not in a recorded session: {e}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::check::tests::entry;

    #[test]
    fn text_and_json() {
        let mut e = entry(1, "0193-u");
        assert_eq!(render(&e, None, false), "session 0193-u\ncast /l/x.cast\n");
        let server = ServerInfo {
            pid: 1,
            url: "https://box:11000".into(),
            port: 11000,
            fingerprint: String::new(),
            version: String::new(),
        };
        assert_eq!(
            render(&e, Some(&server), false),
            "session 0193-u\ncast /l/x.cast\nurl https://box:11000/s/0193-u\n"
        );
        e.state = RecState::Paused;
        let v: serde_json::Value = serde_json::from_str(&render(&e, None, true)).unwrap();
        assert_eq!(
            v,
            json!({"uuid": "0193-u", "cast": "/l/x.cast", "url": null, "state": "paused"})
        );
        assert!(!render(&e, None, true).contains("0707"), "the nonce is never printed");
    }
}
