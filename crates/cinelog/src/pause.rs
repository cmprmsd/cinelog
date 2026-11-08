//! `cinelog pause [--reason]` / `cinelog resume` (PROTOCOL §4.6, §6.1).
//!
//! The request travels in-band: a `user-pause`/`user-resume` marker with the session nonce
//! from the registry, written to `/dev/tty`, which is the slave the recorder reads. The
//! recorder confirms by rewriting its registry entry, which is polled for a short while.

use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use cinelog_core::registry::{RecState, Registry, RegistryEntry};
use cinelog_proto::{EncodeError, NonceField, PauseRequest, WireMessage, try_encode_marker};

use crate::ctx::Ctx;
use crate::link::state_name;
use crate::session;

const CONFIRM_TIMEOUT: Duration = Duration::from_millis(1500);
const CONFIRM_POLL: Duration = Duration::from_millis(20);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Pause,
    Resume,
}

pub fn message(action: Action, reason: Option<String>) -> WireMessage {
    let req = PauseRequest {
        reason,
        pid: Some(std::process::id()),
        extra: Default::default(),
    };
    match action {
        Action::Pause => WireMessage::UserPause(req),
        Action::Resume => WireMessage::UserResume(req),
    }
}

/// Whether the recorder paused the session for a user request (`state_reason` is `user`
/// or `user: <reason>`), as opposed to a multiplexer or a nested recorder.
pub fn user_paused(e: &RegistryEntry) -> bool {
    e.state == RecState::Paused
        && e.state_reason
            .as_deref()
            .is_some_and(|r| r == "user" || r.starts_with("user:"))
}

/// What to do with a request, given the session's entry before it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Plan {
    /// Nothing to send; print this.
    Nothing(String),
    /// Send the marker and wait for the recorder's confirmation.
    SendAndConfirm,
    /// Send the marker; the registry cannot show its effect now, so print this instead.
    SendUnconfirmable(String),
}

fn reason_of(e: &RegistryEntry) -> String {
    e.state_reason
        .clone()
        .unwrap_or_else(|| "no reason recorded".to_owned())
}

pub fn plan(action: Action, before: &RegistryEntry) -> Plan {
    let uuid = &before.uuid;
    match (action, before.state) {
        (Action::Pause, RecState::Live) => Plan::SendAndConfirm,
        (Action::Pause, RecState::Paused) if user_paused(before) => Plan::Nothing(format!("{uuid} is already paused")),
        // The open pause interval keeps its first reason, so the registry does not change.
        (Action::Pause, RecState::Paused) => Plan::SendUnconfirmable(format!(
            "{uuid} is already paused ({}); the request was sent so it stays paused after that ends, until `cinelog resume`",
            reason_of(before)
        )),
        (Action::Pause, RecState::Degraded) => Plan::SendUnconfirmable(format!(
            "{uuid} is degraded ({}) and records nothing right now; the pause request was sent",
            reason_of(before)
        )),
        (Action::Resume, RecState::Live) => Plan::Nothing(format!("{uuid} is not paused")),
        (Action::Resume, RecState::Paused) if user_paused(before) => Plan::SendAndConfirm,
        (Action::Resume, RecState::Paused) => Plan::SendUnconfirmable(format!(
            "{uuid} is paused by {}, not by `cinelog pause`; it resumes by itself when that ends",
            reason_of(before)
        )),
        (Action::Resume, RecState::Degraded) => Plan::SendAndConfirm,
    }
}

/// Whether `now` shows that the recorder handled `action`, given the entry `before` for
/// which [`plan`] said [`Plan::SendAndConfirm`].
pub fn confirmed(action: Action, before: &RegistryEntry, now: &RegistryEntry) -> bool {
    match action {
        Action::Pause => user_paused(now),
        Action::Resume => now.state == RecState::Live && (before.state != RecState::Degraded || now.part > before.part),
    }
}

fn wait_for(registry: &Registry, cur: &session::Current, action: Action) -> Option<RegistryEntry> {
    let deadline = Instant::now() + CONFIRM_TIMEOUT;
    loop {
        if let Ok(Some(now)) = registry.read(&cur.key)
            && now.pid == cur.entry.pid
            && confirmed(action, &cur.entry, &now)
        {
            return Some(now);
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(CONFIRM_POLL);
    }
}

fn write_tty(bytes: &[u8]) -> std::io::Result<()> {
    let mut tty = std::fs::OpenOptions::new()
        .write(true)
        .custom_flags(libc::O_NOCTTY)
        .open("/dev/tty")?;
    tty.write_all(bytes)?;
    tty.flush()
}

pub fn run(action: Action, reason: Option<String>) -> ExitCode {
    let ctx = Ctx::load();
    let cur = match session::current(&ctx) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("cinelog: not in a recorded session: {e}");
            return ExitCode::FAILURE;
        }
    };
    let uuid = cur.entry.uuid.as_str();
    let plan = plan(action, &cur.entry);
    if let Plan::Nothing(msg) = &plan {
        println!("{msg}");
        return ExitCode::SUCCESS;
    }
    let marker = match try_encode_marker(NonceField::Session(cur.entry.nonce), &message(action, reason)) {
        Ok(m) => m,
        Err(EncodeError::Oversized { len }) => {
            eprintln!("cinelog: the --reason is too long (the marker would be {len} bytes); shorten it");
            return ExitCode::FAILURE;
        }
    };
    if let Err(e) = write_tty(&marker) {
        eprintln!("cinelog: cannot write to /dev/tty: {e}");
        return ExitCode::FAILURE;
    }
    if let Plan::SendUnconfirmable(msg) = &plan {
        println!("{msg}");
        return ExitCode::SUCCESS;
    }
    match (action, wait_for(&ctx.registry(), &cur, action)) {
        (Action::Pause, Some(_)) => println!("paused {uuid} (a pause marker marks the gap)"),
        (Action::Resume, Some(now)) if now.part > cur.entry.part => {
            println!("resumed {uuid}, continuing in part {}", now.part);
        }
        (Action::Resume, Some(_)) => println!("resumed {uuid}"),
        (_, None) => {
            let state = ctx
                .registry()
                .read(&cur.key)
                .ok()
                .flatten()
                .map_or("gone", |e| state_name(e.state));
            eprintln!("cinelog: request sent, but the recorder has not confirmed it (state: {state})");
        }
    }
    ExitCode::SUCCESS
}

#[cfg(test)]
mod tests {
    use cinelog_proto::Nonce;

    use super::*;
    use crate::check::tests::entry;

    #[test]
    fn markers_carry_kind_and_reason_but_never_leak_the_nonce_elsewhere() {
        let m = message(Action::Pause, Some("lunch".into()));
        assert_eq!(m.kind(), "user-pause");
        let json = String::from_utf8(m.to_json()).unwrap();
        assert!(
            json.starts_with(r#"{"v":1,"k":"user-pause","reason":"lunch""#),
            "{json}"
        );
        assert_eq!(message(Action::Resume, None).kind(), "user-resume");

        let nonce = Nonce::from_bytes([0xab; 16]);
        let wire = try_encode_marker(NonceField::Session(nonce), &m).unwrap();
        let text = String::from_utf8(wire).unwrap();
        assert!(text.starts_with(&format!("\x1b]7777;cinelog;1;{};b;", nonce.to_hex())));
        assert!(text.ends_with('\x07'));
        assert!(!json.contains(&nonce.to_hex()));
    }

    #[test]
    fn an_overlong_reason_is_an_error_not_a_panic() {
        let nonce = NonceField::Session(Nonce::from_bytes([0xab; 16]));
        let long = "x".repeat(64 * 1024);
        let err = try_encode_marker(nonce, &message(Action::Pause, Some(long))).unwrap_err();
        assert!(matches!(err, EncodeError::Oversized { len } if len > cinelog_proto::MAX_ENCODED_PAYLOAD));
        assert!(try_encode_marker(nonce, &message(Action::Pause, Some("x".repeat(1024)))).is_ok());
    }

    fn with_state(state: RecState, reason: Option<&str>) -> RegistryEntry {
        let mut e = entry(1, "u");
        e.state = state;
        e.state_reason = reason.map(str::to_owned);
        e
    }

    #[test]
    fn confirmation_rules() {
        let live = entry(1, "u");
        let paused = with_state(RecState::Paused, Some("user"));
        let paused_lunch = with_state(RecState::Paused, Some("user: lunch"));
        let mux = with_state(RecState::Paused, Some("multiplexer: tmux: client"));
        let degraded = with_state(RecState::Degraded, Some("enospc"));
        let mut part2 = live.clone();
        part2.part = 2;

        assert!(confirmed(Action::Pause, &live, &paused));
        assert!(confirmed(Action::Pause, &live, &paused_lunch));
        assert!(!confirmed(Action::Pause, &live, &live));
        assert!(
            !confirmed(Action::Pause, &live, &mux),
            "another cause's pause is no confirmation"
        );
        assert!(!confirmed(
            Action::Pause,
            &live,
            &with_state(RecState::Paused, Some("username"))
        ));
        assert!(confirmed(Action::Resume, &paused, &live));
        assert!(!confirmed(Action::Resume, &paused, &paused));
        assert!(!confirmed(Action::Resume, &degraded, &live), "needs the next part");
        assert!(confirmed(Action::Resume, &degraded, &part2));
    }

    #[test]
    fn existing_pauses_of_other_causes_are_reported_not_confirmed() {
        let live = entry(1, "u");
        let user = with_state(RecState::Paused, Some("user: lunch"));
        let mux = with_state(RecState::Paused, Some("multiplexer: tmux: client"));
        let nested = with_state(RecState::Paused, Some("nested: 0193-child"));
        let degraded = with_state(RecState::Degraded, Some("enospc"));

        assert_eq!(plan(Action::Pause, &live), Plan::SendAndConfirm);
        assert_eq!(plan(Action::Pause, &user), Plan::Nothing("u is already paused".into()));
        let Plan::SendUnconfirmable(m) = plan(Action::Pause, &mux) else {
            panic!()
        };
        assert!(m.contains("already paused (multiplexer: tmux: client)"), "{m}");
        assert!(matches!(plan(Action::Pause, &degraded), Plan::SendUnconfirmable(m) if m.contains("enospc")));

        assert_eq!(plan(Action::Resume, &live), Plan::Nothing("u is not paused".into()));
        assert_eq!(plan(Action::Resume, &user), Plan::SendAndConfirm);
        assert_eq!(plan(Action::Resume, &degraded), Plan::SendAndConfirm);
        let Plan::SendUnconfirmable(m) = plan(Action::Resume, &nested) else {
            panic!()
        };
        assert!(m.contains("paused by nested: 0193-child"), "{m}");
    }

    #[test]
    fn waiting_sees_a_registry_update() {
        let dir = tempfile::tempdir().unwrap();
        let reg = Registry::new(dir.path());
        let key = cinelog_core::TtyKey::Pts(9);
        let e = entry(1, "u");
        reg.write(&key, &e).unwrap();
        let cur = session::Current {
            key: key.clone(),
            entry: e.clone(),
        };
        let mut paused = e;
        paused.state = RecState::Paused;
        paused.state_reason = Some("user".into());
        let writer = {
            let reg = reg.clone();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(60));
                reg.write(&key, &paused).unwrap();
            })
        };
        assert!(wait_for(&reg, &cur, Action::Pause).is_some());
        writer.join().unwrap();
        assert!(wait_for(&reg, &cur, Action::Resume).is_none());
    }
}
