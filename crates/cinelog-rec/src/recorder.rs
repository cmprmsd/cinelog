//! The running recorder (plan §5.1): main thread (input, signals, foreground polling,
//! mirroring, clock), output thread (master → scanner → host tty → events) and the writer
//! thread. Everything here happens after the decision to record; [`record`] fails only
//! before the fork, which lets the caller fall back to an unrecorded shell.

use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd, RawFd};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use cinelog_core::clock::{Clock, local_tz};
use cinelog_core::config::RecordConfig;
use cinelog_core::logging::RecLog;
use cinelog_core::registry::{Registry, RegistryEntry, TtyKey};
use cinelog_detect::{Detector, Foreground, NestedClaim, foreground_pgrp};
use cinelog_proto::{MarkerScanner, NestedInfo, Nonce, NonceField, Scanner, SessionInfo, WireMessage};
use nix::sys::termios::Termios;
use nix::sys::wait::{WaitPidFlag, WaitStatus, waitpid};
use signal_hook::SigId;

use crate::clockwatch::ClockWatch;
use crate::control::{Control, ControlConfig, FastPoll, Probe};
use crate::events::{self, QUEUE_BUDGET};
use crate::input::InputRules;
use crate::mirror::{self, ArgvArea};
use crate::pty::{self, ChildSpec, RawGuard, Winsize};
use crate::reg::RegistryHandle;
use crate::tmux::TmuxClient;
use crate::writer::{CastFactory, HeaderInfo, Writer, WriterConfig, WriterShared};

/// Drain cap per master read cycle (16 KiB beat 256 KiB in the spike).
const DRAIN_CAP: usize = 16 * 1024;
const FG_POLL: Duration = Duration::from_millis(250);
const FG_POLL_FAST: Duration = Duration::from_millis(20);
const FG_RESCAN_US: u64 = 1_000_000;
const MIRROR_RECHECK_US: u64 = 1_000_000;
const CTX_INTERVAL: Duration = Duration::from_secs(30);
const REAP_DEADLINE: Duration = Duration::from_secs(3);
const MAX_PENDING_INPUT: usize = 1 << 20;

/// The host side the recorder runs on.
#[derive(Debug, Clone, Copy)]
pub(crate) struct HostIo {
    pub input: RawFd,
    pub output: RawFd,
    /// `input` is a terminal: raw mode, resize tracking, EOF is a hangup.
    pub is_tty: bool,
    /// Write `nested-start`/`nested-end` to `output` (pts hosts only; a VT would print it).
    pub announce: bool,
}

/// Process-wide side effects, enabled by `run()` and off in in-process tests.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ProcessEffects {
    pub mirror_cmdline: bool,
    pub mirror_cwd: bool,
    /// Ignore SIGTSTP/SIGTTIN/SIGTTOU so restoring the host termios can never stop us.
    pub ignore_job_signals: bool,
}

/// Everything prepared before the fork.
/// `master` and `slave` must already be close-on-exec.
pub(crate) struct Launch {
    pub child: ChildSpec,
    pub master: OwnedFd,
    pub slave: OwnedFd,
    pub nonce_read: OwnedFd,
    pub nonce: Nonce,
    pub winsize: Winsize,
    pub host_termios: Option<Termios>,
    pub host: HostIo,
    pub effects: ProcessEffects,
    pub clock: Arc<dyn Clock>,
    pub detector: Arc<dyn Detector + Send + Sync>,
    pub record: RecordConfig,
    /// `shell_pid`, start times and tz are filled in at the fork.
    pub session: SessionInfo,
    pub header: HeaderInfo,
    pub factory: Box<dyn CastFactory>,
    pub registry: Option<(Registry, TtyKey, RegistryEntry)>,
    pub log: Option<RecLog>,
    pub tmux: Option<TmuxClient>,
}

#[derive(Debug)]
struct Flags {
    child_gone: AtomicBool,
    stop: AtomicBool,
    host_dead: AtomicBool,
}

struct Signals {
    pipe: std::os::unix::net::UnixStream,
    terminate: Arc<AtomicBool>,
    ids: Vec<SigId>,
}

impl Signals {
    fn register() -> io::Result<Self> {
        let (r, w) = std::os::unix::net::UnixStream::pair()?;
        r.set_nonblocking(true)?;
        w.set_nonblocking(true)?;
        let terminate = Arc::new(AtomicBool::new(false));
        let mut ids = Vec::new();
        for s in [
            libc::SIGWINCH,
            libc::SIGCHLD,
            libc::SIGHUP,
            libc::SIGTERM,
            libc::SIGQUIT,
            libc::SIGINT,
        ] {
            ids.push(signal_hook::low_level::pipe::register(s, w.try_clone()?)?);
        }
        for s in [libc::SIGHUP, libc::SIGTERM, libc::SIGQUIT] {
            ids.push(signal_hook::flag::register(s, Arc::clone(&terminate))?);
        }
        Ok(Self {
            pipe: r,
            terminate,
            ids,
        })
    }
}

impl Drop for Signals {
    fn drop(&mut self) {
        for id in self.ids.drain(..) {
            signal_hook::low_level::unregister(id);
        }
    }
}

/// [`Probe`] over the real process tree.
struct ProcProbe {
    detector: Arc<dyn Detector + Send + Sync>,
    master: RawFd,
    shell_pgrp: u32,
    multiplexers: Vec<String>,
}

impl ProcProbe {
    fn fg(&self) -> Option<u32> {
        // SAFETY: the master outlives every probe call (output thread joined, main loop left).
        foreground_pgrp(unsafe { BorrowedFd::borrow_raw(self.master) }).ok()
    }
}

impl Probe for ProcProbe {
    fn corroborate(&self, claim: NestedClaim) -> bool {
        // The inner recorder announces itself right after starting; give the process tree
        // a moment to show it in the foreground job.
        for attempt in 0..3 {
            if let Some(fg) = self.fg()
                && self.detector.corroborate_nested(fg, self.shell_pgrp, claim)
            {
                return true;
            }
            if attempt < 2 {
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        false
    }

    fn foreground(&self) -> Option<Foreground> {
        let fg = self.fg()?;
        Some(self.detector.foreground(fg, self.shell_pgrp, &self.multiplexers))
    }
}

fn lock(c: &Mutex<Control>) -> std::sync::MutexGuard<'_, Control> {
    c.lock().unwrap_or_else(|e| e.into_inner())
}

/// Error before the fork: nothing was started, the caller falls back.
#[derive(Debug)]
pub(crate) struct SpawnError(pub String);

/// Records until the shell exits or the terminal hangs up; returns the exit status.
pub(crate) fn record(mut l: Launch) -> Result<i32, SpawnError> {
    let signals = Signals::register().map_err(|e| SpawnError(format!("signals: {e}")))?;
    let (wake_r, wake_w) = pty::wake_pipe().map_err(|e| SpawnError(format!("pipe: {e}")))?;
    let (done_r, done_w) = std::os::unix::net::UnixStream::pair().map_err(|e| SpawnError(format!("pipe: {e}")))?;

    let boot0 = l.clock.boottime_us();
    let epoch0 = l.clock.realtime_us();
    let child = pty::spawn(&l.child, &l.slave, &l.master, l.nonce_read.as_raw_fd())
        .map_err(|e| SpawnError(format!("fork: {e}")))?;
    drop(l.slave);
    drop(l.nonce_read);
    if l.effects.ignore_job_signals {
        ignore_job_signals();
    }
    let _ = pty::set_nonblocking(l.master.as_raw_fd());

    let (tz, tz_offset) = local_tz(epoch0);
    l.session.start_epoch_us = epoch0;
    l.session.boottime_us = boot0;
    l.session.tz = tz;
    l.session.tz_offset_s = tz_offset;
    l.session.shell_pid = child;

    let registry = l.registry.take().map(|(reg, key, mut entry)| {
        entry.shell_pid = child;
        let h = Arc::new(RegistryHandle::new(reg, key, entry, l.log.clone()));
        h.publish();
        h
    });

    let writer_shared = Arc::new(WriterShared::default());
    let (tx, rx) = events::channel(QUEUE_BUDGET);
    let writer = {
        let w = Writer::new(
            l.factory,
            Arc::clone(&l.clock),
            l.session.clone(),
            l.header.clone(),
            WriterConfig {
                rotate_bytes: l.record.rotate_mb.saturating_mul(1 << 20),
                rotate_age_us: l.record.rotate_days.saturating_mul(86_400_000_000),
                fsync_us: l.record.fsync_secs.saturating_mul(1_000_000),
            },
            Arc::clone(&writer_shared),
            registry.clone(),
            l.log.clone(),
            // Also with strip_markers off: kept marker bytes reach `o` with the nonce scrubbed.
            Some(l.nonce),
        );
        std::thread::Builder::new()
            .name("cinelog-writer".into())
            .spawn(move || w.run(rx, boot0))
            .expect("spawn writer thread")
    };

    // Raw mode only after the fork: the child got the original termios through openpty.
    let raw_guard = match (l.host.is_tty, l.host_termios.take()) {
        (true, Some(t)) => RawGuard::new(l.host.input, t).ok(),
        _ => None,
    };

    let shell_pgrp = child;
    let probe = Arc::new(ProcProbe {
        detector: Arc::clone(&l.detector),
        master: l.master.as_raw_fd(),
        shell_pgrp,
        multiplexers: l.record.pause_on_multiplexer.clone(),
    });
    let fast = Arc::new(FastPoll::new(Some(wake_w)));
    let rules = InputRules::from_config(&l.record);
    let control = Arc::new(Mutex::new(Control::new(
        tx,
        Arc::clone(&l.clock),
        ControlConfig {
            nested: l.record.nested,
            input: rules,
            mirror_cwd: l.effects.mirror_cwd,
        },
        probe,
        Arc::clone(&writer_shared),
        registry.clone(),
        Arc::clone(&fast),
    )));

    let flags = Arc::new(Flags {
        child_gone: AtomicBool::new(false),
        stop: AtomicBool::new(false),
        host_dead: AtomicBool::new(false),
    });
    let announce = l.host.announce.then(|| nested_info(&l.session));
    if let Some(info) = &announce {
        let msg = cinelog_proto::encode_marker(NonceField::Recorder, &WireMessage::NestedStart(info.clone()));
        let _ = pty::write_all(l.host.output, &msg, &flags.stop);
    }

    let output = {
        let scanner = Scanner::new(Some(l.nonce), l.record.strip_markers);
        let (control, flags) = (Arc::clone(&control), Arc::clone(&flags));
        let (master, host_out) = (l.master.as_raw_fd(), l.host.output);
        std::thread::Builder::new()
            .name("cinelog-output".into())
            .spawn(move || output_loop(master, host_out, scanner, &control, &flags, done_w))
            .expect("spawn output thread")
    };
    let ctx = l.tmux.take().map(|client| spawn_ctx(client, Arc::clone(&control)));

    let mut main = MainLoop {
        host: l.host,
        master: &l.master,
        child,
        shell_pgrp,
        control: &control,
        flags: &flags,
        detector: Arc::clone(&l.detector),
        record: &l.record,
        rules,
        clock: Arc::clone(&l.clock),
        fast: &fast,
        watch: ClockWatch::new(epoch0, boot0),
        argv: if l.effects.mirror_cmdline && l.record.mirror_cmdline {
            ArgvArea::of_self()
        } else {
            None
        },
        status: None,
        winsize: l.winsize,
        fg: FgCache::default(),
    };
    let hangup = main.run(&signals, wake_r.as_fd(), &done_r);
    let reaped = main.status;

    // Shutdown: stop the output thread, close the master (hangs up any remaining slave
    // holder), then reap the shell with a deadline.
    if hangup {
        if reaped.is_none() {
            let pid = nix::unistd::Pid::from_raw(child as i32);
            let _ = nix::sys::signal::kill(pid, nix::sys::signal::Signal::SIGHUP);
        }
        flags.stop.store(true, Ordering::SeqCst);
    }
    flags.child_gone.store(true, Ordering::SeqCst);
    let _ = output.join();
    drop(l.master);
    let status = reaped.unwrap_or_else(|| reap(child));

    if let Some((stop, handle)) = ctx {
        drop(stop);
        let _ = handle.join();
    }
    lock(&control).finish(status);
    drop(control);
    let _ = writer.join();

    if let Some(info) = announce
        && !flags.host_dead.load(Ordering::SeqCst)
    {
        let end = NestedInfo {
            exit: Some(status),
            ..info
        };
        let msg = cinelog_proto::encode_marker(NonceField::Recorder, &WireMessage::NestedEnd(end));
        let _ = pty::write_all(l.host.output, &msg, &AtomicBool::new(true));
    }
    if let Some(reg) = &registry {
        reg.remove();
    }
    drop(raw_guard);
    drop(signals);
    Ok(status)
}

fn ignore_job_signals() {
    use nix::sys::signal::{SigHandler, Signal, signal};
    for sig in [Signal::SIGTSTP, Signal::SIGTTIN, Signal::SIGTTOU] {
        // SAFETY: installing SIG_IGN has no handler code to be unsafe about.
        let _ = unsafe { signal(sig, SigHandler::SigIgn) };
    }
}

fn nested_info(s: &SessionInfo) -> NestedInfo {
    NestedInfo {
        uuid: s.uuid.clone(),
        pid: s.rec_pid,
        starttime: s.rec_starttime,
        scope: s.scope,
        user: s.user.clone(),
        sudo_user: s.sudo_user.clone(),
        url: None,
        exit: None,
        extra: Default::default(),
    }
}

fn exit_code(ws: WaitStatus) -> Option<i32> {
    match ws {
        WaitStatus::Exited(_, c) => Some(c),
        WaitStatus::Signaled(_, s, _) => Some(128 + s as i32),
        _ => None,
    }
}

/// Waits for the shell; SIGKILL after [`REAP_DEADLINE`].
fn reap(child: u32) -> i32 {
    let pid = nix::unistd::Pid::from_raw(child as i32);
    let deadline = Instant::now() + REAP_DEADLINE;
    loop {
        match waitpid(pid, Some(WaitPidFlag::WNOHANG)) {
            Ok(ws) => {
                if let Some(c) = exit_code(ws) {
                    return c;
                }
            }
            Err(_) => return 1,
        }
        if Instant::now() > deadline {
            let _ = nix::sys::signal::kill(pid, nix::sys::signal::Signal::SIGKILL);
            return waitpid(pid, None).ok().and_then(exit_code).unwrap_or(137);
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Output thread: master → scanner → host tty → control (events).
fn output_loop(
    master: RawFd,
    host_out: RawFd,
    mut scanner: Scanner,
    control: &Mutex<Control>,
    flags: &Flags,
    done: std::os::unix::net::UnixStream,
) {
    let mut buf = vec![0u8; DRAIN_CAP + 4096];
    let mut out = Vec::with_capacity(DRAIN_CAP * 2);
    let mut markers = Vec::new();
    loop {
        if flags.stop.load(Ordering::Relaxed) {
            break;
        }
        let mut p = libc::pollfd {
            fd: master,
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: one valid pollfd.
        unsafe { libc::poll(&mut p, 1, 100) };
        let (mut filled, mut eof) = (0, false);
        while filled < DRAIN_CAP {
            match pty::read(master, &mut buf[filled..]) {
                Ok(0) => {
                    eof = true;
                    break;
                }
                Ok(n) => filled += n,
                Err(e) if e.raw_os_error() == Some(libc::EINTR) => {}
                Err(e) => {
                    // EIO: every slave fd is closed.
                    eof = e.raw_os_error() != Some(libc::EAGAIN);
                    break;
                }
            }
        }
        if filled > 0 {
            out.clear();
            markers.clear();
            scanner.feed(&buf[..filled], &mut out, &mut markers);
            emit_output(host_out, &out, &markers, control, flags);
        }
        // Shell reaped and the pty drained: done even if a background job holds the slave.
        if eof || (filled == 0 && flags.child_gone.load(Ordering::SeqCst)) {
            break;
        }
    }
    out.clear();
    scanner.finish(&mut out);
    emit_output(host_out, &out, &[], control, flags);
    lock(control).set_scanner_bad(scanner.stats().bad_markers());
    let _ = std::io::Write::write_all(&mut &done, b"x");
}

fn emit_output(
    host_out: RawFd,
    out: &[u8],
    markers: &[cinelog_proto::ScannedMarker],
    control: &Mutex<Control>,
    flags: &Flags,
) {
    if out.is_empty() && markers.is_empty() {
        return;
    }
    if !out.is_empty()
        && !flags.host_dead.load(Ordering::Relaxed)
        && pty::write_all(host_out, out, &flags.stop).is_err()
    {
        flags.host_dead.store(true, Ordering::SeqCst);
    }
    lock(control).on_output(out, markers);
}

/// tmux `ctx` thread: queries at start and every 30 s; stops when the sender is dropped.
fn spawn_ctx(client: TmuxClient, control: Arc<Mutex<Control>>) -> (crossbeam_channel::Sender<()>, JoinHandle<()>) {
    let (stop, stopped) = crossbeam_channel::bounded::<()>(0);
    let h = std::thread::Builder::new()
        .name("cinelog-ctx".into())
        .spawn(move || {
            loop {
                if let Some(ctx) = client.context() {
                    lock(&control).on_ctx(ctx);
                }
                match stopped.recv_timeout(CTX_INTERVAL) {
                    Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
                    _ => break,
                }
            }
        })
        .expect("spawn ctx thread");
    (stop, h)
}

#[derive(Debug, Default)]
struct FgCache {
    pgrp: Option<u32>,
    last_scan: u64,
    relay: bool,
    last_mirror: u64,
    mirror_pgrp: Option<u32>,
}

struct MainLoop<'a> {
    host: HostIo,
    master: &'a OwnedFd,
    child: u32,
    shell_pgrp: u32,
    control: &'a Mutex<Control>,
    flags: &'a Flags,
    detector: Arc<dyn Detector + Send + Sync>,
    record: &'a RecordConfig,
    rules: InputRules,
    clock: Arc<dyn Clock>,
    fast: &'a FastPoll,
    watch: ClockWatch,
    argv: Option<ArgvArea>,
    status: Option<i32>,
    winsize: Winsize,
    fg: FgCache,
}

impl MainLoop<'_> {
    /// Returns whether the loop ended by a hangup (terminal gone, SIGHUP/SIGTERM).
    fn run(&mut self, signals: &Signals, wake: BorrowedFd<'_>, done: &std::os::unix::net::UnixStream) -> bool {
        let mfd = self.master.as_raw_fd();
        let mut buf = vec![0u8; 64 * 1024];
        let mut pending: Vec<u8> = Vec::new();
        let mut stdin_open = true;
        let mut next_tick = 0u64;
        loop {
            let now = self.clock.boottime_us();
            if now >= next_tick {
                self.tick(now);
                let fast = now < self.fast.until();
                next_tick = now + duration_us(if fast { FG_POLL_FAST } else { FG_POLL });
            }
            let timeout_ms = next_tick
                .saturating_sub(self.clock.boottime_us())
                .div_ceil(1000)
                .min(250) as i32;
            let mut fds = [
                pollfd(if pending.is_empty() { -1 } else { mfd }, libc::POLLOUT),
                pollfd(signals.pipe.as_raw_fd(), libc::POLLIN),
                pollfd(
                    if stdin_open && pending.len() < MAX_PENDING_INPUT {
                        self.host.input
                    } else {
                        -1
                    },
                    libc::POLLIN,
                ),
                pollfd(done.as_raw_fd(), libc::POLLIN),
                pollfd(wake.as_raw_fd(), libc::POLLIN),
            ];
            // SAFETY: valid pollfd array.
            let n = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, timeout_ms) };
            if n < 0 {
                if io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) {
                    continue;
                }
                return true;
            }
            if fds[4].revents != 0 {
                pty::drain(wake);
                // Re-evaluate the poll interval right away (fast polling after a marker).
                next_tick = next_tick.min(self.clock.boottime_us() + duration_us(FG_POLL_FAST));
            }
            if fds[1].revents != 0 {
                pty::drain(signals.pipe.as_fd());
                self.on_signals();
            }
            if fds[3].revents != 0 {
                return false;
            }
            if !pending.is_empty() && fds[0].revents & libc::POLLOUT != 0 {
                forward(mfd, &mut pending);
            }
            if fds[2].revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0 {
                match pty::read(self.host.input, &mut buf) {
                    Ok(0) => {
                        stdin_open = false;
                        if self.host.is_tty {
                            return true;
                        }
                    }
                    Ok(n) => {
                        let data = &buf[..n];
                        // Sample the termios before the child can see the bytes: getpass()
                        // and `read -s` turn ECHO back on right after reading the line.
                        let flags = pty::term_flags(self.master.as_fd());
                        pending.extend_from_slice(data);
                        forward(mfd, &mut pending);
                        #[cfg(test)]
                        tests_hook::after_forward();
                        lock(self.control).on_input(data, flags, self.fg.relay);
                    }
                    Err(e) if matches!(e.raw_os_error(), Some(libc::EAGAIN | libc::EINTR)) => {}
                    Err(_) => {
                        stdin_open = false;
                        if self.host.is_tty {
                            return true;
                        }
                    }
                }
            }
            if self.flags.host_dead.load(Ordering::Relaxed) || signals.terminate.load(Ordering::Relaxed) {
                return true;
            }
        }
    }

    fn on_signals(&mut self) {
        if self.host.is_tty
            && let Some(ws) = pty::winsize(self.host.input)
            && (ws.ws_col, ws.ws_row) != (self.winsize.ws_col, self.winsize.ws_row)
        {
            self.winsize = ws;
            // The kernel sends SIGWINCH to the slave's foreground pgrp.
            pty::set_winsize(self.master.as_raw_fd(), &ws);
            lock(self.control).on_resize(ws.ws_col, ws.ws_row);
        }
        if self.status.is_none()
            && let Ok(ws) = waitpid(
                nix::unistd::Pid::from_raw(self.child as i32),
                Some(WaitPidFlag::WNOHANG),
            )
            && let Some(code) = exit_code(ws)
        {
            self.status = Some(code);
            self.flags.child_gone.store(true, Ordering::SeqCst);
        }
    }

    /// Foreground polling (§5.3), relay rule, name mirroring (§5.7) and clock markers (§5.4).
    fn tick(&mut self, now: u64) {
        if let Ok(pg) = foreground_pgrp(self.master.as_fd()) {
            let changed = self.fg.pgrp != Some(pg);
            // The shell's own pgrp is cheap (leader comm only, catches `exec tmux`); other
            // jobs are rescanned every second (`sh -c '…; tmux a'` changes comm later).
            if changed || pg == self.shell_pgrp || now.saturating_sub(self.fg.last_scan) >= FG_RESCAN_US {
                let fg = self
                    .detector
                    .foreground(pg, self.shell_pgrp, &self.record.pause_on_multiplexer);
                self.fg.relay = self.rules.relay
                    && pg != self.shell_pgrp
                    && self.detector.relay_in_foreground(pg, &self.record.relays);
                self.fg.pgrp = Some(pg);
                self.fg.last_scan = now;
                lock(self.control).on_foreground(&fg);
            }
            if let Some(argv) = &mut self.argv
                && (self.fg.mirror_pgrp != Some(pg) || now.saturating_sub(self.fg.last_mirror) >= MIRROR_RECHECK_US)
            {
                self.fg.mirror_pgrp = Some(pg);
                self.fg.last_mirror = now;
                if let Some(name) = mirror::name_of(pg) {
                    argv.set(&name);
                }
            }
        }
        if let Some(c) = self.watch.tick(self.clock.realtime_us(), now) {
            lock(self.control).on_clock(c);
        }
    }
}

fn duration_us(d: Duration) -> u64 {
    u64::try_from(d.as_micros()).unwrap_or(u64::MAX)
}

fn pollfd(fd: RawFd, events: libc::c_short) -> libc::pollfd {
    libc::pollfd { fd, events, revents: 0 }
}

/// Writes as much pending input to the master as it accepts now.
fn forward(master: RawFd, pending: &mut Vec<u8>) {
    // SAFETY: pending is a valid readable buffer.
    let w = unsafe { libc::write(master, pending.as_ptr().cast(), pending.len()) };
    if w > 0 {
        pending.drain(..w as usize);
    }
}

/// Lets tests widen the window between forwarding input and classifying it.
#[cfg(test)]
pub(crate) mod tests_hook {
    use std::sync::atomic::{AtomicU64, Ordering};

    pub(crate) static FORWARD_DELAY_US: AtomicU64 = AtomicU64::new(0);

    pub(crate) fn after_forward() {
        let us = FORWARD_DELAY_US.load(Ordering::Relaxed);
        if us > 0 {
            std::thread::sleep(std::time::Duration::from_micros(us));
        }
    }
}
