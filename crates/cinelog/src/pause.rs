//! `cinelog pause [--reason]` / `cinelog resume` (PROTOCOL §4.6, §6.1).
//!
//! The request travels in-band: a `user-pause`/`user-resume` marker with the session nonce
//! from the registry, written to `/dev/tty`, which is the slave the recorder reads. The
//! recorder confirms by rewriting its registry entry, which is polled for a short while.
//!
//! [`write_unrecorded`] uses the same markers around a secret that a command prints once
//! (the login token of `install` and `token new`), so it never lands in a cast.

use std::io::{self, IsTerminal, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use cinelog_core::registry::{RecState, Registry, RegistryEntry};
use cinelog_detect::{Detector, Pid, ProcFs, RealProcFs};
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

// ------------------------------------------------------------------ secrets on screen

/// `--reason` of the pause around a secret (`state_reason` `user: secret output`).
const SECRET_REASON: &str = "secret output";
/// Ancestors searched for the terminal a `sudo` relays to.
const MAX_ANCESTORS: usize = 64;

/// A recording that sees what this process writes to its terminal.
struct Watched {
    entry: RegistryEntry,
    /// Re-reads its registry entry (same recorder pid), for the confirmation.
    reread: Box<dyn Fn() -> Option<RegistryEntry>>,
}

/// The recording of this terminal: this scope's session of the controlling tty (§6.5),
/// else, for root, a recording under another uid of a pts further up the process tree.
/// That is a member's PERSONAL recording around `sudo cinelog install --team`: sudo
/// relays this terminal to theirs, pause markers included.
fn recording_of_terminal(ctx: &Ctx) -> Option<Watched> {
    if let Ok(cur) = session::current(ctx) {
        let registry = ctx.registry();
        let (key, pid) = (cur.key, cur.entry.pid);
        return Some(Watched {
            entry: cur.entry,
            reread: Box::new(move || registry.read(&key).ok().flatten().filter(|e| e.pid == pid)),
        });
    }
    if ctx.env.euid != 0 {
        return None;
    }
    let detector = ctx.detector();
    let ppid = nix::unistd::getppid().as_raw().unsigned_abs();
    ancestor_ptys(&RealProcFs::new()).into_iter().find_map(|n| {
        let holder = detector.holder(n, ppid)?;
        let entry = cinelog_rec::foreign_recording(&holder, n, &detector)?;
        let detector = ctx.detector();
        let pid = entry.pid;
        Some(Watched {
            entry,
            reread: Box::new(move || cinelog_rec::foreign_recording(&holder, n, &detector).filter(|e| e.pid == pid)),
        })
    })
}

/// The pts numbers that this process and its ancestors have as controlling tty, innermost
/// first and each once.
fn ancestor_ptys(proc: &dyn ProcFs) -> Vec<u32> {
    let mut out = Vec::new();
    let mut pid: Pid = proc.self_pid();
    for _ in 0..MAX_ANCESTORS {
        let Ok(st) = proc.stat(pid) else { break };
        if let Some(cinelog_core::registry::TtyKey::Pts(n)) = cinelog_core::tty::key_from_dev(st.tty_nr)
            && !out.contains(&n)
        {
            out.push(n);
        }
        if st.ppid <= 1 {
            break;
        }
        pid = st.ppid;
    }
    out
}

/// Writes `text`, a secret shown once (a login token), to `out` with the recording of this
/// terminal paused, so it never lands in a cast: in a recorded root shell that would be a
/// TEAM cast every member can search. The pause and resume markers travel in the same
/// byte stream as the text and take effect exactly where they occur (§4.3), so only the
/// text is left out, behind a visible pause marker. Only output that reaches the terminal
/// at once is covered, so nothing is paused unless stdout is a terminal (a file or pipe
/// is not recorded now; a pager that shows it later is).
pub fn write_unrecorded(ctx: &Ctx, out: &mut dyn Write, text: &str) -> io::Result<()> {
    let watched = io::stdout().is_terminal().then(|| recording_of_terminal(ctx)).flatten();
    let entry = watched.as_ref().map(|w| &w.entry);
    let reread = || watched.as_ref().and_then(|w| (w.reread)());
    let paused = around_secret(entry, &mut |m| write_tty(m), &reread, out, text, CONFIRM_TIMEOUT)?;
    if !paused && let Some(e) = entry {
        eprintln!(
            "cinelog: the recording {} did not confirm the pause: the secret above may be in {}",
            e.uuid,
            e.cast.display()
        );
    }
    Ok(())
}

/// [`write_unrecorded`] with its effects passed in. A recording that is not live records
/// nothing now and is left alone (a user pause stays). Returns `false` when a pause was
/// due but not confirmed in time; the text is written anyway (the secret exists now and
/// is never shown again) and the resume is still sent, so a late pause does not stay.
fn around_secret(
    rec: Option<&RegistryEntry>,
    send: &mut dyn FnMut(&[u8]) -> io::Result<()>,
    reread: &dyn Fn() -> Option<RegistryEntry>,
    out: &mut dyn Write,
    text: &str,
    timeout: Duration,
) -> io::Result<bool> {
    let Some(rec) = rec.filter(|e| e.state == RecState::Live) else {
        out.write_all(text.as_bytes())?;
        return out.flush().map(|()| true);
    };
    let marker = |action| {
        try_encode_marker(
            NonceField::Session(rec.nonce),
            &message(action, Some(SECRET_REASON.to_owned())),
        )
        .expect("a short reason fits in a marker")
    };
    let mut paused = send(&marker(Action::Pause)).is_ok();
    if paused {
        let deadline = Instant::now() + timeout;
        while !reread().is_some_and(|now| user_paused(&now)) {
            if Instant::now() >= deadline {
                paused = false;
                break;
            }
            std::thread::sleep(CONFIRM_POLL);
        }
    }
    let written = out.write_all(text.as_bytes()).and_then(|()| out.flush());
    let _ = send(&marker(Action::Resume));
    written.map(|()| paused)
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

    /// Pause markers and output in one stream, in the order the terminal gets them.
    #[test]
    fn a_secret_is_written_between_pause_and_resume() {
        use std::cell::RefCell;
        use std::rc::Rc;

        #[derive(Clone, Default)]
        struct Tape(Rc<RefCell<Vec<String>>>);
        impl Write for Tape {
            fn write(&mut self, b: &[u8]) -> io::Result<usize> {
                self.0.borrow_mut().push(String::from_utf8_lossy(b).into_owned());
                Ok(b.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let kind = |m: &str| {
            let nonce = Nonce::from_bytes([0xab; 16]);
            let wire = |a| {
                String::from_utf8(
                    try_encode_marker(NonceField::Session(nonce), &message(a, Some(SECRET_REASON.into()))).unwrap(),
                )
                .unwrap()
            };
            if m == wire(Action::Pause) {
                "<pause>".to_owned()
            } else if m == wire(Action::Resume) {
                "<resume>".to_owned()
            } else {
                m.to_owned()
            }
        };
        let mut live = entry(1, "u");
        live.nonce = Nonce::from_bytes([0xab; 16]);
        let paused = with_state(RecState::Paused, Some("user: secret output"));
        let run = |rec: Option<&RegistryEntry>, confirm: Option<RegistryEntry>| {
            let tape = Tape::default();
            let mut send_tape = tape.clone();
            let mut send = |m: &[u8]| send_tape.write_all(m);
            let mut out = tape.clone();
            let ok = around_secret(
                rec,
                &mut send,
                &|| confirm.clone(),
                &mut out,
                "token: S\n",
                Duration::from_millis(50),
            )
            .unwrap();
            let log: Vec<String> = tape.0.borrow().iter().map(|m| kind(m)).collect();
            (ok, log)
        };

        assert_eq!(
            run(Some(&live), Some(paused.clone())),
            (true, vec!["<pause>".into(), "token: S\n".into(), "<resume>".into()])
        );
        assert_eq!(
            run(Some(&live), Some(live.clone())),
            (false, vec!["<pause>".into(), "token: S\n".into(), "<resume>".into()]),
            "unconfirmed: still shown, still resumed"
        );
        assert_eq!(run(None, None), (true, vec!["token: S\n".into()]), "not recorded");
        for state in [paused, with_state(RecState::Degraded, Some("enospc"))] {
            assert_eq!(
                run(Some(&state), None),
                (true, vec!["token: S\n".into()]),
                "{:?} is left alone",
                state.state
            );
        }
    }

    /// `stat` of a process tree; nothing else is supported.
    struct Tree(Vec<(Pid, Pid, u64)>);
    impl ProcFs for Tree {
        fn self_pid(&self) -> Pid {
            self.0[0].0
        }
        fn stat(&self, pid: Pid) -> io::Result<cinelog_detect::ProcStat> {
            let &(pid, ppid, tty_nr) = self.0.iter().find(|p| p.0 == pid).ok_or(io::ErrorKind::NotFound)?;
            Ok(cinelog_detect::ProcStat {
                pid,
                comm: String::new(),
                state: 'S',
                ppid,
                pgrp: pid,
                session: pid,
                tty_nr,
                tpgid: -1,
                starttime: 0,
            })
        }
        fn pids(&self) -> io::Result<Vec<Pid>> {
            Err(io::ErrorKind::Unsupported.into())
        }
        fn comm(&self, _: Pid) -> io::Result<String> {
            Err(io::ErrorKind::Unsupported.into())
        }
        fn cmdline(&self, _: Pid) -> io::Result<Vec<String>> {
            Err(io::ErrorKind::Unsupported.into())
        }
        fn exe(&self, _: Pid) -> io::Result<std::path::PathBuf> {
            Err(io::ErrorKind::Unsupported.into())
        }
        fn uids(&self, _: Pid) -> io::Result<cinelog_detect::Uids> {
            Err(io::ErrorKind::Unsupported.into())
        }
        fn owner_uid(&self, _: Pid) -> io::Result<u32> {
            Err(io::ErrorKind::Unsupported.into())
        }
        fn loginuid(&self, _: Pid) -> io::Result<Option<u32>> {
            Err(io::ErrorKind::Unsupported.into())
        }
        fn fds(&self, _: Pid) -> io::Result<Vec<cinelog_detect::FdEntry>> {
            Err(io::ErrorKind::Unsupported.into())
        }
        fn fdinfo_tty_index(&self, _: Pid, _: u32) -> io::Result<Option<u32>> {
            Err(io::ErrorKind::Unsupported.into())
        }
        fn boot_id(&self) -> io::Result<String> {
            Err(io::ErrorKind::Unsupported.into())
        }
        fn hidepid(&self) -> io::Result<cinelog_detect::Hidepid> {
            Err(io::ErrorKind::Unsupported.into())
        }
    }

    #[test]
    fn ancestor_ptys_are_listed_innermost_first() {
        let pts = |n: u64| (136 << 8) | n;
        // cinelog (sudo's pts 9) <- sudo (pts 9) <- sudo (the member's pts 4) <- zsh (pts 4)
        // <- recorder (pts 2) <- terminal (no tty) <- init.
        let tree = Tree(vec![
            (50, 49, pts(9)),
            (49, 48, pts(9)),
            (48, 47, pts(4)),
            (47, 46, pts(4)),
            (46, 45, pts(2)),
            (45, 1, 0),
        ]);
        assert_eq!(ancestor_ptys(&tree), [9, 4, 2]);
        assert_eq!(
            ancestor_ptys(&Tree(vec![(50, 7, (4 << 8) | 1)])),
            [0u32; 0],
            "tty1 and a gone parent"
        );
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
