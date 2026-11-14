//! Recording decisions shared by the output and main threads (plan §4.4, §5.3, §5.5, §5.7).
//!
//! [`Control`] sits behind one mutex. Every event that reaches the writer is sent while
//! holding it, so `o`/`i` events, markers and pause boundaries are queued in exactly the
//! order the recorder decided them, with monotonic timestamps.

use std::os::fd::{AsRawFd, OwnedFd};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use cinelog_core::clock::Clock;
use cinelog_detect::{Foreground, NestedClaim};
use cinelog_proto::{
    CastMarker, ClockSync, CmdRecord, DoneRecord, MuxTarget, NestedMode, ScannedMarker, TmuxContext, WireMessage,
};

use crate::events::{EventTx, Msg, Payload};
use crate::input::{self, InputContext, InputRules, PromptGate, TermFlags};
use crate::pause::PauseMachine;
use crate::reg::RegistryHandle;
use crate::screen::{AltScreen, PromptTail};
use crate::seq::SeqAssigner;
use crate::writer::WriterShared;

/// Synthetic output that returns a player to the primary screen (§5.3).
pub(crate) const ALT_FIXUP: &[u8] = b"\x1b[?1049l\x1b[0m";
/// After a `cmd`/`done` marker the foreground is polled quickly for this long.
pub(crate) const FAST_POLL_WINDOW_US: u64 = 500_000;

/// Process-tree access needed while handling markers.
pub(crate) trait Probe: Send + Sync {
    /// §5.3: a `nested-start` claim is a recorder in the current foreground job.
    fn corroborate(&self, claim: NestedClaim) -> bool;
    /// Current foreground classification, `None` if it cannot be read.
    fn foreground(&self) -> Option<Foreground>;
}

/// Wakes the main thread so it switches to fast foreground polling.
#[derive(Debug, Default)]
pub(crate) struct FastPoll {
    until: AtomicU64,
    wake: Option<OwnedFd>,
}

impl FastPoll {
    pub(crate) fn new(wake: Option<OwnedFd>) -> Self {
        Self {
            until: AtomicU64::new(0),
            wake,
        }
    }

    pub(crate) fn until(&self) -> u64 {
        self.until.load(Ordering::Acquire)
    }

    fn trigger(&self, until: u64) {
        self.until.fetch_max(until, Ordering::AcqRel);
        if let Some(fd) = &self.wake {
            // Non-blocking pipe: a full pipe already guarantees a wake-up.
            // SAFETY: valid fd owned by self, one-byte buffer.
            unsafe { libc::write(fd.as_raw_fd(), b"w".as_ptr().cast(), 1) };
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct ControlConfig {
    pub nested: NestedMode,
    pub input: InputRules,
    /// chdir to the cwd of validated `cmd`/`done` markers (§5.7).
    pub mirror_cwd: bool,
}

pub(crate) struct Control {
    tx: EventTx,
    clock: Arc<dyn Clock>,
    cfg: ControlConfig,
    probe: Arc<dyn Probe>,
    writer: Arc<WriterShared>,
    registry: Option<Arc<RegistryHandle>>,
    fast: Arc<FastPoll>,
    pause: PauseMachine,
    seq: SeqAssigner,
    alt: AltScreen,
    /// Alternate-screen state of the *recorded* stream.
    alt_recorded: bool,
    prompt: PromptTail,
    gate: PromptGate,
    last_cwd: Option<String>,
    last_ctx: Option<TmuxContext>,
    /// Bumped whenever a marker changes the pause state or re-reads the foreground; the
    /// main thread's readings taken before that are stale and dropped.
    fg_gen: u64,
    bad_markers: u64,
    scanner_bad: u64,
}

impl Control {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        tx: EventTx,
        clock: Arc<dyn Clock>,
        cfg: ControlConfig,
        probe: Arc<dyn Probe>,
        writer: Arc<WriterShared>,
        registry: Option<Arc<RegistryHandle>>,
        fast: Arc<FastPoll>,
    ) -> Self {
        Self {
            tx,
            clock,
            cfg,
            probe,
            writer,
            registry,
            fast,
            pause: PauseMachine::default(),
            seq: SeqAssigner::default(),
            alt: AltScreen::default(),
            alt_recorded: false,
            prompt: PromptTail::default(),
            gate: PromptGate::default(),
            last_cwd: None,
            last_ctx: None,
            fg_gen: 0,
            bad_markers: 0,
            scanner_bad: 0,
        }
    }

    fn now(&self) -> u64 {
        self.clock.boottime_us()
    }

    #[cfg(test)]
    pub(crate) fn is_paused(&self) -> bool {
        self.pause.is_paused()
    }

    fn degraded(&self) -> bool {
        self.writer.degraded.load(Ordering::Acquire)
    }

    fn alt_open(&self) -> bool {
        self.alt_recorded && !self.alt.active()
    }

    /// One chunk of terminal output after marker scanning. `markers[i].at` indexes `out`.
    pub(crate) fn on_output(&mut self, out: &[u8], markers: &[ScannedMarker]) {
        let t = self.now();
        let mut pos = 0;
        for m in markers {
            let at = m.at.clamp(pos, out.len());
            self.output_segment(t, &out[pos..at]);
            pos = at;
            self.on_marker(t, m);
        }
        self.output_segment(t, &out[pos..]);
        self.prompt.push(out);
        if self.cfg.input.prompt_heuristic && self.prompt.looks_like_secret_prompt() {
            self.gate.arm();
        }
    }

    fn output_segment(&mut self, t: u64, seg: &[u8]) {
        if seg.is_empty() {
            return;
        }
        self.alt.feed(seg);
        if self.pause.is_paused() {
            return;
        }
        self.alt_recorded = self.alt.active();
        if !self.degraded() {
            self.tx.data(t, Payload::Output(seg.to_vec()));
        }
    }

    fn record_marker(&mut self, t: u64, m: CastMarker) {
        if !self.pause.is_paused() {
            self.tx.marker(t, m);
        }
    }

    /// Writes the markers of a pause transition and mirrors the state into the registry
    /// (also without markers: the remaining cause may have changed).
    fn emit(&mut self, t: u64, markers: Vec<CastMarker>) {
        for m in markers {
            let fixup = matches!(&m, CastMarker::Resume(r) if r.alt_fixup);
            self.tx.marker(t, m);
            if fixup {
                self.tx.data(t, Payload::Output(ALT_FIXUP.to_vec()));
                self.alt_recorded = false;
            }
        }
        if let Some(reg) = &self.registry {
            reg.set_paused(self.pause.pause_reason());
        }
    }

    /// A marker from our own shell proves the shell is in front again: re-check the
    /// foreground at once so the resume lands exactly before it (`done` after `tmux a`).
    fn refresh_foreground(&mut self, t: u64) {
        if self.pause.is_paused()
            && let Some(fg) = self.probe.foreground()
        {
            self.fg_gen += 1;
            let alt = self.alt_open();
            let ms = self.pause.on_foreground(&fg, alt);
            self.emit(t, ms);
        }
    }

    fn mirror_cwd(&mut self, cwd: &str) {
        if !self.cfg.mirror_cwd || !cwd.starts_with('/') || self.last_cwd.as_deref() == Some(cwd) {
            return;
        }
        // Errors are ignored: the directory may be unreadable for us or already gone.
        let _ = std::env::set_current_dir(cwd);
        self.last_cwd = Some(cwd.to_owned());
    }

    fn on_marker(&mut self, t: u64, m: &ScannedMarker) {
        // Undecodable payloads and nonce/kind mismatches (PROTOCOL §4.1 step 4) are bad.
        let Ok(msg) = m.decode() else {
            self.bad_markers += 1;
            return;
        };
        if matches!(
            msg,
            WireMessage::NestedStart(_)
                | WireMessage::NestedEnd(_)
                | WireMessage::UserPause(_)
                | WireMessage::UserResume(_)
        ) {
            self.fg_gen += 1;
        }
        match msg {
            WireMessage::NestedStart(info) => {
                let claim = NestedClaim {
                    pid: info.pid,
                    starttime: info.starttime,
                };
                if self.probe.corroborate(claim) {
                    let ms = self.pause.on_nested_start(&info, self.cfg.nested);
                    self.emit(t, ms);
                } else {
                    self.bad_markers += 1;
                }
            }
            WireMessage::NestedEnd(info) => {
                let alt = self.alt_open();
                let ms = self.pause.on_nested_end(&info, alt);
                self.emit(t, ms);
            }
            WireMessage::UserPause(req) => {
                let ms = self.pause.on_user_pause(req.reason);
                self.emit(t, ms);
            }
            WireMessage::UserResume(_) => {
                let alt = self.alt_open();
                let ms = self.pause.on_user_resume(alt);
                self.emit(t, ms);
                if self.degraded() {
                    self.tx.control(Msg::ResumeDegraded { t });
                }
            }
            WireMessage::Hello(h) => {
                self.refresh_foreground(t);
                self.record_marker(t, CastMarker::Hello(h));
            }
            WireMessage::Cmd(cmd) => {
                self.mirror_cwd(&cmd.cwd);
                self.fast.trigger(t + FAST_POLL_WINDOW_US);
                self.refresh_foreground(t);
                if !self.pause.is_paused() {
                    let seq = self.seq.on_cmd(cmd.sh, cmd.n);
                    self.tx.marker(t, CastMarker::Cmd(CmdRecord { seq, cmd }));
                }
            }
            WireMessage::Done(done) => {
                self.mirror_cwd(&done.cwd);
                self.fast.trigger(t + FAST_POLL_WINDOW_US);
                self.refresh_foreground(t);
                if !self.pause.is_paused() {
                    let seq = self.seq.on_done(done.sh, done.n);
                    self.tx.marker(t, CastMarker::Done(DoneRecord { seq, done }));
                }
            }
            WireMessage::Skip(skip) => {
                self.refresh_foreground(t);
                self.record_marker(t, CastMarker::Skip(skip));
            }
            WireMessage::Unknown { .. } => {}
        }
    }

    /// One chunk of host input, already forwarded to the child.
    pub(crate) fn on_input(&mut self, data: &[u8], flags: TermFlags, relay_in_foreground: bool) {
        let ctx = InputContext {
            flags,
            paused: self.pause.is_paused(),
            relay_in_foreground,
        };
        let rec = input::recordable(&self.cfg.input, ctx, &mut self.gate, data);
        if !rec.is_empty() && !self.degraded() {
            let t = self.now();
            self.tx.data(t, Payload::Input(rec.into_owned()));
        }
    }

    /// Generation to pass to [`Self::on_foreground`] with a reading taken after this call.
    pub(crate) fn fg_generation(&self) -> u64 {
        self.fg_gen
    }

    /// A foreground reading of the main thread, taken without the lock. It is dropped
    /// (returns `false`) if a marker changed the pause state or re-read the foreground
    /// since `generation`: applying it would undo a newer decision (a stale tmux reading
    /// right after the resume before `done`).
    pub(crate) fn on_foreground(&mut self, fg: &Foreground, generation: u64) -> bool {
        if generation != self.fg_gen {
            return false;
        }
        let t = self.now();
        let alt = self.alt_open();
        let ms = self.pause.on_foreground(fg, alt);
        self.emit(t, ms);
        true
    }

    /// The tmux session the client `pid` attached to (resolved off-thread after its pause).
    pub(crate) fn on_mux_target(&mut self, pid: u32, target: MuxTarget) {
        if let Some(m) = self.pause.on_mux_target(pid, target) {
            let t = self.now();
            self.tx.marker(t, m);
        }
    }

    /// Resizes are recorded even while paused.
    pub(crate) fn on_resize(&mut self, cols: u16, rows: u16) {
        let t = self.now();
        self.tx.always(t, Payload::Resize { cols, rows });
    }

    pub(crate) fn on_clock(&mut self, c: ClockSync) {
        let t = self.now();
        self.tx.marker(t, CastMarker::Clock(c));
    }

    /// tmux context; written at start and on change, never while paused (retried later).
    pub(crate) fn on_ctx(&mut self, ctx: TmuxContext) {
        if self.pause.is_paused() || self.last_ctx.as_ref() == Some(&ctx) {
            return;
        }
        let t = self.now();
        self.last_ctx = Some(ctx.clone());
        self.tx.marker(t, CastMarker::Ctx(ctx));
    }

    /// Counters of the marker scanner, reported by the output thread.
    pub(crate) fn set_scanner_bad(&mut self, n: u64) {
        self.scanner_bad = n;
    }

    pub(crate) fn bad_markers(&self) -> u64 {
        self.bad_markers + self.scanner_bad
    }

    /// Final `x` event.
    pub(crate) fn finish(&mut self, status: i32) {
        let t = self.now();
        self.tx.flush_gap();
        self.tx.always(
            t,
            Payload::Exit {
                status,
                bad_markers: self.bad_markers(),
                dropped_bytes: self.tx.dropped_total(),
            },
        );
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::events::{EventRx, channel};
    use crate::writer::tests::FakeClock;
    use cinelog_proto::{Cmd, Done, Encoding, Extra, NestedInfo, Nonce, NonceField, PauseRequest, Scope};
    use std::sync::Mutex;

    pub(crate) const NONCE: Nonce = Nonce::from_bytes([0xab; 16]);

    #[derive(Default)]
    pub(crate) struct FakeProbe {
        pub nested_ok: std::sync::atomic::AtomicBool,
        pub fg: Mutex<Option<Foreground>>,
    }

    impl Probe for FakeProbe {
        fn corroborate(&self, _claim: NestedClaim) -> bool {
            self.nested_ok.load(Ordering::SeqCst)
        }
        fn foreground(&self) -> Option<Foreground> {
            self.fg.lock().unwrap().clone()
        }
    }

    pub(crate) const RULES: InputRules = InputRules {
        enabled: true,
        password_prompt: true,
        prompt_heuristic: true,
        paused: true,
        relay: false,
        terminal_responses: true,
    };

    pub(crate) fn control(probe: Arc<FakeProbe>, nested: NestedMode) -> (Control, EventRx, Arc<WriterShared>) {
        let (tx, rx) = channel(crate::events::QUEUE_BUDGET);
        let shared = Arc::new(WriterShared::default());
        let c = Control::new(
            tx,
            Arc::new(FakeClock::default()),
            ControlConfig {
                nested,
                input: RULES,
                mirror_cwd: false,
            },
            probe,
            Arc::clone(&shared),
            None,
            Arc::new(FastPoll::new(None)),
        );
        (c, rx, shared)
    }

    pub(crate) fn marker(nonce: NonceField, msg: &WireMessage, at: usize) -> ScannedMarker {
        ScannedMarker {
            at,
            version: 1,
            nonce,
            enc: Encoding::Json,
            payload: msg.to_json(),
        }
    }

    fn cmd(sh: u32, n: u64, typed: &str) -> WireMessage {
        WireMessage::Cmd(Cmd {
            sh,
            n,
            typed: typed.into(),
            expanded: typed.into(),
            cwd: "/tmp".into(),
            user: "alice".into(),
            euid: None,
            host: "box".into(),
            pane: None,
            t: 0.0,
            trunc: false,
            extra: Extra::new(),
        })
    }

    fn done(sh: u32, n: u64) -> WireMessage {
        WireMessage::Done(Done {
            sh,
            n,
            exit: 0,
            pipestatus: vec![0],
            cwd: "/tmp".into(),
            t: 0.0,
            extra: Extra::new(),
        })
    }

    fn nested_info(kind_end: bool) -> WireMessage {
        let info = NestedInfo {
            uuid: "child".into(),
            pid: 77,
            starttime: 5,
            scope: Scope::Team,
            user: "root".into(),
            sudo_user: Some("alice".into()),
            url: None,
            exit: None,
            extra: Extra::new(),
        };
        if kind_end {
            WireMessage::NestedEnd(info)
        } else {
            WireMessage::NestedStart(info)
        }
    }

    /// Drains the queue into readable tokens.
    pub(crate) fn drain(rx: &EventRx) -> Vec<String> {
        let mut out = Vec::new();
        while let Ok(m) = rx.try_recv() {
            out.push(match m {
                Msg::Event { payload, .. } => match payload {
                    Payload::Output(d) => format!("o:{}", String::from_utf8_lossy(&d)),
                    Payload::Input(d) => format!("i:{}", String::from_utf8_lossy(&d)),
                    Payload::Marker(m) => match &*m {
                        CastMarker::Cmd(c) => format!("cmd#{}:{}", c.seq, c.cmd.typed),
                        CastMarker::Done(d) => format!("done#{:?}", d.seq),
                        other => format!("m:{}", other.kind()),
                    },
                    Payload::Resize { cols, rows } => format!("r:{cols}x{rows}"),
                    Payload::Exit {
                        status, bad_markers, ..
                    } => format!("x:{status}/bad={bad_markers}"),
                },
                Msg::Gap { .. } => "gap".into(),
                Msg::ResumeDegraded { .. } => "resume-degraded".into(),
            });
        }
        out
    }

    const S: NonceField = NonceField::Session(NONCE);

    #[test]
    fn markers_split_output_at_their_offsets() {
        let (mut c, rx, _) = control(Arc::default(), NestedMode::Pause);
        let out = b"ab$ ls\r\nfile\r\n";
        c.on_output(
            out,
            &[marker(S, &cmd(1, 1, "ls"), 4), marker(S, &done(1, 1), out.len())],
        );
        assert_eq!(drain(&rx), ["o:ab$ ", "cmd#1:ls", "o:ls\r\nfile\r\n", "done#Some(1)"]);
    }

    #[test]
    fn seq_assignment_with_interleaved_shells() {
        let (mut c, rx, _) = control(Arc::default(), NestedMode::Pause);
        c.on_output(
            b"",
            &[
                marker(S, &cmd(100, 1, "zsh"), 0),
                marker(S, &cmd(200, 1, "echo inner"), 0),
                marker(S, &done(200, 1), 0),
                marker(S, &done(100, 1), 0),
                marker(S, &cmd(100, 2, "ls"), 0),
            ],
        );
        assert_eq!(
            drain(&rx),
            [
                "cmd#1:zsh",
                "cmd#2:echo inner",
                "done#Some(2)",
                "done#Some(1)",
                "cmd#3:ls"
            ]
        );
    }

    #[test]
    fn nonce_kind_mismatch_and_garbage_count_as_bad() {
        let (mut c, rx, _) = control(Arc::default(), NestedMode::Pause);
        let mut garbage = marker(S, &cmd(1, 1, "x"), 0);
        garbage.payload = b"{not json".to_vec();
        c.on_output(
            b"",
            &[
                marker(NonceField::Recorder, &cmd(1, 1, "forged"), 0),
                marker(S, &nested_info(false), 0),
                garbage,
            ],
        );
        c.set_scanner_bad(2);
        c.finish(0);
        assert_eq!(drain(&rx), ["x:0/bad=5"]);
    }

    #[test]
    fn nested_start_requires_corroboration() {
        let probe = Arc::new(FakeProbe::default());
        let (mut c, rx, _) = control(Arc::clone(&probe), NestedMode::Pause);
        c.on_output(b"a", &[marker(NonceField::Recorder, &nested_info(false), 1)]);
        assert!(!c.is_paused());
        probe.nested_ok.store(true, Ordering::SeqCst);
        c.on_output(b"b", &[marker(NonceField::Recorder, &nested_info(false), 1)]);
        assert!(c.is_paused());
        c.on_output(b"root secret", &[]);
        c.on_input(b"whoami\r", TermFlags::UNKNOWN, false);
        c.on_resize(90, 20);
        c.on_output(b"", &[marker(NonceField::Recorder, &nested_info(true), 0)]);
        c.on_output(b"back", &[]);
        assert_eq!(
            drain(&rx),
            ["o:a", "o:b", "m:nested", "m:pause", "r:90x20", "m:resume", "o:back"]
        );
    }

    #[test]
    fn nested_record_mode_keeps_recording() {
        let probe = Arc::new(FakeProbe::default());
        probe.nested_ok.store(true, Ordering::SeqCst);
        let (mut c, rx, _) = control(probe, NestedMode::Record);
        c.on_output(b"x", &[marker(NonceField::Recorder, &nested_info(false), 0)]);
        assert!(!c.is_paused());
        assert_eq!(drain(&rx), ["m:nested", "o:x"]);
    }

    #[test]
    fn multiplexer_pause_and_alt_fixup() {
        let probe = Arc::new(FakeProbe::default());
        let (mut c, rx, _) = control(Arc::clone(&probe), NestedMode::Pause);
        // tmux's first frame leaks during the detection lag ...
        c.on_output(b"\x1b[?1049hTMUX", &[]);
        let g = c.fg_generation();
        assert!(c.on_foreground(&tmux_client(), g));
        // ... and leaves the alternate screen while paused.
        c.on_output(b"pane content\x1b[?1049l", &[]);
        // The shell's `done` arrives before the next poll: the probe sees the shell again.
        *probe.fg.lock().unwrap() = Some(Foreground::Shell);
        c.on_output(b"$ ", &[marker(S, &done(1, 1), 0)]);
        assert_eq!(
            drain(&rx),
            [
                "o:\u{1b}[?1049hTMUX",
                "m:multiplexer",
                "m:pause",
                "m:resume",
                "o:\u{1b}[?1049l\u{1b}[0m",
                "done#None",
                "o:$ "
            ]
        );
    }

    fn tmux_client() -> Foreground {
        Foreground::Multiplexer {
            pid: 9,
            comm: "tmux: client".into(),
            pgrp: 9,
        }
    }

    /// Regression: the main thread read "tmux in front" without the lock, the output
    /// thread then resumed on `done` (tmux detached), and the stale reading re-paused.
    #[test]
    fn stale_foreground_reading_after_a_marker_driven_resume_is_dropped() {
        let probe = Arc::new(FakeProbe::default());
        let (mut c, rx, _) = control(Arc::clone(&probe), NestedMode::Pause);
        assert!(c.on_foreground(&tmux_client(), c.fg_generation()));
        assert!(c.is_paused());
        // Main thread: generation taken, /proc scan in progress (tmux still attached) ...
        let g = c.fg_generation();
        // ... output thread: tmux detached, `done` arrives, the probe sees the shell.
        *probe.fg.lock().unwrap() = Some(Foreground::Shell);
        c.on_output(b"", &[marker(S, &done(1, 1), 0)]);
        assert!(!c.is_paused());
        // ... main thread applies its reading.
        assert!(!c.on_foreground(&tmux_client(), g), "stale reading dropped");
        assert!(!c.is_paused());
        c.on_output(b"$ ", &[]);
        assert_eq!(
            drain(&rx),
            ["m:multiplexer", "m:pause", "m:resume", "done#None", "o:$ "]
        );
        // A fresh reading still applies.
        assert!(c.on_foreground(&Foreground::Shell, c.fg_generation()));
    }

    /// The same race the other way round: a "shell in front" reading taken before a
    /// corroborated `nested-start` must not end the nested pause.
    #[test]
    fn stale_shell_reading_does_not_end_a_nested_pause() {
        let probe = Arc::new(FakeProbe::default());
        probe.nested_ok.store(true, Ordering::SeqCst);
        let (mut c, rx, _) = control(Arc::clone(&probe), NestedMode::Pause);
        let g = c.fg_generation();
        c.on_output(b"", &[marker(NonceField::Recorder, &nested_info(false), 0)]);
        assert!(!c.on_foreground(&Foreground::Shell, g));
        assert!(c.is_paused());
        assert_eq!(drain(&rx), ["m:nested", "m:pause"]);
    }

    #[test]
    fn registry_reason_tracks_the_remaining_cause() {
        let dir = tempfile::tempdir().unwrap();
        let reg = cinelog_core::registry::Registry::new(dir.path());
        let key = cinelog_core::registry::TtyKey::Pts(3);
        let entry = cinelog_core::registry::RegistryEntry {
            format: cinelog_core::registry::REGISTRY_FORMAT,
            pid: std::process::id(),
            starttime: 1,
            boot_id: "b".into(),
            uuid: "u".into(),
            nonce: NONCE,
            scope: Scope::Personal,
            cast: "/a.cast".into(),
            pts: "/dev/pts/3".into(),
            host_tty: String::new(),
            shell_pid: 2,
            parent_uuid: None,
            state: cinelog_core::registry::RecState::Live,
            state_reason: None,
            part: 1,
            version: String::new(),
            opts: String::new(),
        };
        let handle = Arc::new(RegistryHandle::new(reg.clone(), key.clone(), entry, None));
        handle.publish();
        let (tx, _rx) = channel(crate::events::QUEUE_BUDGET);
        let mut c = Control::new(
            tx,
            Arc::new(FakeClock::default()),
            ControlConfig {
                nested: NestedMode::Pause,
                input: RULES,
                mirror_cwd: false,
            },
            Arc::new(FakeProbe::default()),
            Arc::new(WriterShared::default()),
            Some(handle),
            Arc::new(FastPoll::new(None)),
        );
        let state = || {
            let e = reg.read(&key).unwrap().unwrap();
            (e.state, e.state_reason)
        };
        let pause = WireMessage::UserPause(PauseRequest {
            reason: Some("lunch".into()),
            ..PauseRequest::default()
        });
        c.on_output(b"", &[marker(S, &pause, 0)]);
        assert!(c.on_foreground(&tmux_client(), c.fg_generation()));
        assert_eq!(
            state(),
            (cinelog_core::registry::RecState::Paused, Some("user: lunch".into()))
        );
        c.on_output(b"", &[marker(S, &WireMessage::UserResume(PauseRequest::default()), 0)]);
        assert_eq!(
            state(),
            (
                cinelog_core::registry::RecState::Paused,
                Some("multiplexer: tmux: client".into())
            ),
            "`cinelog resume` sees why it is still paused"
        );
        assert!(c.on_foreground(&Foreground::Shell, c.fg_generation()));
        assert_eq!(state(), (cinelog_core::registry::RecState::Live, None));
    }

    #[test]
    fn user_pause_and_resume_while_degraded() {
        let (mut c, rx, shared) = control(Arc::default(), NestedMode::Pause);
        let pause = WireMessage::UserPause(PauseRequest {
            reason: Some("meeting".into()),
            ..PauseRequest::default()
        });
        c.on_output(b"", &[marker(S, &pause, 0)]);
        c.on_output(b"hidden", &[marker(S, &cmd(1, 1, "secret"), 6)]);
        shared.degraded.store(true, Ordering::SeqCst);
        c.on_output(b"", &[marker(S, &WireMessage::UserResume(PauseRequest::default()), 0)]);
        c.on_output(b"dropped while degraded", &[]);
        assert_eq!(drain(&rx), ["m:pause", "m:resume", "resume-degraded"]);
    }

    #[test]
    fn input_follows_pause_and_prompt_heuristic() {
        let (mut c, rx, _) = control(Arc::default(), NestedMode::Pause);
        let zle = TermFlags {
            echo: false,
            icanon: false,
        };
        c.on_input(b"ls\r", zle, false);
        c.on_output(b"alice@remote's password: ", &[]);
        c.on_input(b"hunter2\r", zle, false);
        c.on_output(b"\r\nWelcome\r\n", &[]);
        c.on_input(b"\x1b[?62cexit\r", zle, false);
        assert_eq!(
            drain(&rx),
            ["i:ls\r", "o:alice@remote's password: ", "o:\r\nWelcome\r\n", "i:exit\r"]
        );
    }

    #[test]
    fn ctx_is_written_on_change_only() {
        let (mut c, rx, _) = control(Arc::default(), NestedMode::Pause);
        let ctx = TmuxContext {
            session_name: "main".into(),
            ..TmuxContext::default()
        };
        c.on_ctx(ctx.clone());
        c.on_ctx(ctx.clone());
        c.on_ctx(TmuxContext {
            window_name: "vim".into(),
            ..ctx
        });
        assert_eq!(drain(&rx), ["m:ctx", "m:ctx"]);
    }
}
