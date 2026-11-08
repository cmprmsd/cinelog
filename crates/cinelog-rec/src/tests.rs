//! End-to-end tests of the running recorder: a real PTY pair plays the host terminal, a
//! `/bin/sh` script plays the plugin (it reads the nonce fd and prints markers).

use std::os::fd::{AsFd, AsRawFd, OwnedFd};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use cinelog_core::clock::{Clock, SystemClock};
use cinelog_core::config::RecordConfig;
use cinelog_core::registry::{REGISTRY_FORMAT, RecState, Registry, RegistryEntry, TtyKey};
use cinelog_detect::{
    CheckRequest, Detector, Foreground, Holder, NestedClaim, Pid, Principal, PrincipalRequest, ProcDetector,
    RealProcFs, Reason, StartDecision,
};
use cinelog_proto::{Attribution, Nonce, Scope, env};
use serde_json::Value;

use crate::pty::{self, ChildSpec};
use crate::recorder::{self, HostIo, Launch, ProcessEffects};
use crate::writer::FileFactory;
use crate::writer::tests::{header, template};

/// Detector whose foreground classification the test controls.
#[derive(Default)]
struct FakeDetector {
    fg: Mutex<Option<Foreground>>,
    nested_ok: AtomicBool,
}

impl Detector for FakeDetector {
    fn check_start(&self, _: &CheckRequest) -> StartDecision {
        StartDecision::Skip { reason: Reason::Error }
    }
    fn holder(&self, _: u32, _: Pid) -> Option<Holder> {
        None
    }
    fn foreground(&self, fg: Pid, shell: Pid, _: &[String]) -> Foreground {
        self.fg.lock().unwrap().clone().unwrap_or(if fg == shell {
            Foreground::Shell
        } else {
            Foreground::Job {
                pgrp: fg,
                leader_comm: String::new(),
            }
        })
    }
    fn corroborate_nested(&self, _: Pid, _: Pid, _: NestedClaim) -> bool {
        self.nested_ok.load(Ordering::SeqCst)
    }
    fn principal(&self, _: &PrincipalRequest) -> Principal {
        Principal {
            scope: Scope::Personal,
            user: "test".into(),
            sudo_user: None,
            claimed_user: None,
            attribution: Attribution::Ancestor,
            login_via: None,
        }
    }
    fn relay_in_foreground(&self, _: Pid, _: &[String]) -> bool {
        false
    }
    fn starttime(&self, _: Pid) -> Option<u64> {
        Some(1)
    }
    fn boot_id(&self) -> Option<String> {
        Some("boot".into())
    }
}

/// Shell functions available to every test script.
const PRELUDE: &str = r#"
read -r N <&"$CINELOG_NONCE_FD"
eval "exec $CINELOG_NONCE_FD<&-"
mark() { printf '\033]7777;cinelog;1;%s;j;%s\007' "$N" "$1"; }
rmark() { printf '\033]7777;cinelog;1;-;j;%s\007' "$1"; }
"#;

const NONCE: Nonce = Nonce::from_bytes([0x5a; 16]);

struct Running {
    host_master: Option<Arc<OwnedFd>>,
    reader: Option<std::thread::JoinHandle<()>>,
    stop_reader: Arc<AtomicBool>,
    _stdin: Option<OwnedFd>,
    /// Read end of a host output pipe nobody reads ([`Opts::stalled_output`]).
    _stalled: Option<OwnedFd>,
    output: Arc<Mutex<Vec<u8>>>,
    done: crossbeam_channel::Receiver<i32>,
    cast: PathBuf,
    registry: Registry,
    key: TtyKey,
    _dir: tempfile::TempDir,
}

struct Opts {
    record: RecordConfig,
    detector: Arc<dyn Detector + Send + Sync>,
    tty: bool,
    effects: ProcessEffects,
    /// Host output goes to a pipe that is never read (a frozen terminal emulator).
    stalled_output: bool,
}

const NO_EFFECTS: ProcessEffects = ProcessEffects {
    mirror_cmdline: false,
    mirror_cwd: false,
    ignore_job_signals: false,
};

impl Default for Opts {
    fn default() -> Self {
        Self {
            record: RecordConfig::default(),
            detector: Arc::new(FakeDetector::default()),
            tty: true,
            effects: NO_EFFECTS,
            stalled_output: false,
        }
    }
}

fn start(script: &str, opts: Opts) -> Running {
    let dir = tempfile::tempdir().unwrap();
    // Other tests fork concurrently: every fd made here must be close-on-exec at once.
    let host = nix::pty::openpty(None, None).unwrap();
    pty::set_cloexec(host.master.as_raw_fd()).unwrap();
    pty::set_cloexec(host.slave.as_raw_fd()).unwrap();
    let host_termios = nix::sys::termios::tcgetattr(host.slave.as_fd()).unwrap();
    let ws = pty::Winsize {
        ws_row: 30,
        ws_col: 100,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    pty::set_winsize(host.master.as_raw_fd(), &ws);
    let inner = nix::pty::openpty(Some(&ws), Some(&host_termios)).unwrap();
    pty::set_cloexec(inner.master.as_raw_fd()).unwrap();
    pty::set_cloexec(inner.slave.as_raw_fd()).unwrap();
    let slave_path = nix::unistd::ttyname(inner.slave.as_fd()).unwrap();
    let key = TtyKey::from_path(&slave_path).unwrap();
    let nonce_read = pty::nonce_pipe(format!("{}\n", NONCE.to_hex()).as_bytes()).unwrap();
    let fd = nonce_read.as_raw_fd();
    let extra = [
        (env::NONCE_FD, fd.to_string()),
        (env::REC_PID, std::process::id().to_string()),
        (env::TTY, slave_path.display().to_string()),
        (env::SESSION, "test-session".to_string()),
    ];
    let full = format!("{PRELUDE}{script}");
    let child = ChildSpec::new(
        "/bin/sh".as_ref(),
        &["-c".into(), full.into()],
        pty::child_env(std::env::vars_os(), &extra),
    )
    .unwrap();
    let cast = dir.path().join("t.cast");
    let first = cinelog_cast::create(&cast).unwrap();
    let registry = Registry::new(dir.path().join("run"));
    registry.ensure_dir().unwrap();
    let entry = RegistryEntry {
        format: REGISTRY_FORMAT,
        pid: std::process::id(),
        starttime: 1,
        boot_id: "boot".into(),
        uuid: "test-session".into(),
        nonce: NONCE,
        scope: Scope::Personal,
        cast: cast.clone(),
        pts: slave_path.display().to_string(),
        host_tty: String::new(),
        shell_pid: 0,
        parent_uuid: None,
        state: RecState::Live,
        state_reason: None,
        part: 1,
        version: String::new(),
        opts: String::new(),
    };
    let stdin: Option<OwnedFd> = (!opts.tty).then(|| std::fs::File::open("/dev/null").unwrap().into());
    let input = stdin.as_ref().map_or(host.slave.as_raw_fd(), |f| f.as_raw_fd());
    let stalled = opts
        .stalled_output
        .then(|| nix::unistd::pipe2(nix::fcntl::OFlag::O_CLOEXEC).unwrap());
    let output = stalled.as_ref().map_or(host.slave.as_raw_fd(), |(_, w)| w.as_raw_fd());
    let (stalled_r, stalled_w) = stalled.unzip();
    let is_tty = opts.tty;
    let mut session = template();
    session.uuid = "test-session".into();
    let launch = Launch {
        child,
        master: inner.master,
        slave: inner.slave,
        nonce_read,
        nonce: NONCE,
        winsize: ws,
        host_termios: Some(host_termios),
        host: HostIo {
            input,
            output,
            is_tty,
            announce: true,
        },
        effects: opts.effects,
        clock: Arc::new(SystemClock) as Arc<dyn Clock>,
        detector: opts.detector,
        record: opts.record,
        session,
        header: header(),
        factory: Box::new(FileFactory::new(
            dir.path().to_path_buf(),
            |part| format!("t_part{part}.cast"),
            (Box::new(first), cast.clone()),
        )),
        registry: Some((registry.clone(), key.clone(), entry)),
        log: None,
        tmux: None,
        autostart: None,
    };
    let host_slave = host.slave;
    let (done_tx, done) = crossbeam_channel::bounded(1);
    std::thread::spawn(move || {
        let status = recorder::record(launch).expect("record");
        drop(host_slave);
        drop(stalled_w);
        let _ = done_tx.send(status);
    });
    let output = Arc::new(Mutex::new(Vec::new()));
    let master = Arc::new(host.master);
    let stop_reader = Arc::new(AtomicBool::new(false));
    let reader = {
        let (output, master, stop) = (Arc::clone(&output), Arc::clone(&master), Arc::clone(&stop_reader));
        std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            while !stop.load(Ordering::SeqCst) {
                let mut p = libc::pollfd {
                    fd: master.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                };
                // SAFETY: one valid pollfd.
                if unsafe { libc::poll(&mut p, 1, 50) } <= 0 {
                    continue;
                }
                match pty::read(master.as_raw_fd(), &mut buf) {
                    Ok(n) if n > 0 => output.lock().unwrap().extend_from_slice(&buf[..n]),
                    _ => break,
                }
            }
        })
    };
    Running {
        host_master: Some(master),
        reader: Some(reader),
        stop_reader,
        _stdin: stdin,
        _stalled: stalled_r,
        output,
        done,
        cast,
        registry,
        key,
        _dir: dir,
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        self.hangup();
    }
}

impl Running {
    fn wait_output(&self, needle: &str) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if String::from_utf8_lossy(&self.output.lock().unwrap()).contains(needle) {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("timed out waiting for {needle:?} in {:?}", self.host_output());
    }

    fn wait_state(&self, state: RecState) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if self
                .registry
                .read(&self.key)
                .ok()
                .flatten()
                .is_some_and(|e| e.state == state)
            {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("registry never reached {state:?}");
    }

    fn master_fd(&self) -> std::os::fd::RawFd {
        self.host_master.as_ref().expect("terminal open").as_raw_fd()
    }

    fn send(&self, s: &str) {
        pty::write_all(self.master_fd(), s.as_bytes(), &AtomicBool::new(false)).unwrap();
    }

    /// Closes the terminal side, like closing a terminal window.
    fn hangup(&mut self) {
        self.stop_reader.store(true, Ordering::SeqCst);
        if let Some(h) = self.reader.take() {
            let _ = h.join();
        }
        self.host_master = None;
    }

    fn host_output(&self) -> String {
        String::from_utf8_lossy(&self.output.lock().unwrap()).into_owned()
    }

    fn finish(&self) -> i32 {
        self.done
            .recv_timeout(Duration::from_secs(20))
            .expect("recorder finished")
    }
}

/// `(code, data)` events of a cast file.
fn events(path: &Path) -> Vec<(String, String)> {
    let text = std::fs::read_to_string(path).unwrap();
    let mut lines = text.lines();
    let head: Value = serde_json::from_str(lines.next().unwrap()).unwrap();
    assert_eq!(head["version"], 3);
    lines
        .map(|l| {
            let v: Value = serde_json::from_str(l).unwrap();
            (v[1].as_str().unwrap().to_owned(), v[2].as_str().unwrap().to_owned())
        })
        .collect()
}

fn output_text(ev: &[(String, String)]) -> String {
    ev.iter().filter(|(c, _)| c == "o").map(|(_, d)| d.as_str()).collect()
}

fn input_text(ev: &[(String, String)]) -> String {
    ev.iter().filter(|(c, _)| c == "i").map(|(_, d)| d.as_str()).collect()
}

fn markers(ev: &[(String, String)]) -> Vec<Value> {
    ev.iter()
        .filter(|(c, _)| c == "m")
        .map(|(_, d)| serde_json::from_str(d).unwrap())
        .collect()
}

fn marker_kinds(ev: &[(String, String)]) -> Vec<String> {
    markers(ev)
        .iter()
        .map(|m| m["k"].as_str().unwrap().to_owned())
        .collect()
}

#[test]
fn records_output_markers_and_exit_status() {
    let detector: Arc<dyn Detector + Send + Sync> =
        Arc::new(ProcDetector::new(RealProcFs::new(), Registry::new("/nonexistent")));
    let r = start(
        r#"
mark '{"v":1,"k":"hello","sh":1,"cwd":"/"}'
mark '{"v":1,"k":"cmd","sh":1,"n":1,"typed":"echo hi","cwd":"/"}'
echo hi
mark '{"v":1,"k":"done","sh":1,"n":1,"exit":0,"cwd":"/"}'
printf '\033]7777;cinelog;1;00000000000000000000000000000000;j;{"v":1,"k":"cmd","sh":1,"n":2,"typed":"forged"}\007'
mark '{"v":1,"k":"skip","sh":1}'
echo "leak=$N"
echo READY
read go
echo "end-$go"
exit 3
"#,
        Opts {
            detector,
            ..Opts::default()
        },
    );
    r.wait_output("READY");
    let entry = r
        .registry
        .read(&r.key)
        .unwrap()
        .expect("registry entry while recording");
    assert_eq!(entry.state, RecState::Live);
    assert!(entry.shell_pid > 0);
    r.send("ok\n");
    assert_eq!(r.finish(), 3);

    let host = r.host_output();
    assert!(host.contains("hi") && host.contains("end-ok"));
    assert!(host.contains("forged"), "a spoofed marker passes through unchanged");
    let hex = NONCE.to_hex();
    assert!(
        !host.contains(&format!("cinelog;1;{hex}")),
        "accepted markers are stripped"
    );
    assert!(
        host.contains(&format!("leak={hex}")),
        "the terminal still shows what the shell printed"
    );
    assert!(
        host.contains("\x1b]7777;cinelog;1;-;b;"),
        "nested-start/end announced on the host side"
    );
    let raw = std::fs::read_to_string(&r.cast).unwrap();
    assert!(!raw.contains(&hex), "the nonce never reaches the cast");
    assert!(raw.contains("leak=<cinelog-nonce>"));

    let ev = events(&r.cast);
    assert_eq!(ev[0].0, "m");
    assert_eq!(
        marker_kinds(&ev),
        ["session", "hello", "cmd", "done", "skip", "stats"],
        "no marker for the forgery"
    );
    let ms = markers(&ev);
    assert_eq!(ms[0]["shell_pid"].as_u64(), Some(u64::from(entry.shell_pid)));
    assert_eq!(
        (ms[2]["seq"].as_u64(), ms[2]["typed"].as_str()),
        (Some(1), Some("echo hi"))
    );
    assert_eq!(ms[3]["seq"].as_u64(), Some(1));
    assert_eq!(ms[5]["bad_markers"].as_u64(), Some(1));
    let out = output_text(&ev);
    assert!(out.contains("hi\r\n") && out.contains("end-ok") && out.contains("forged"));
    assert!(
        input_text(&ev).contains("ok"),
        "keystrokes at a cooked prompt are recorded"
    );
    assert_eq!(ev.last().unwrap(), &("x".to_owned(), "3".to_owned()));
    assert!(
        r.registry.read(&r.key).unwrap().is_none(),
        "registry entry removed on exit"
    );
}

#[test]
fn password_like_input_is_not_recorded() {
    let r = start(
        r#"
echo PROMPT1
read a
stty -echo
echo PROMPT2
read pw
stty echo
echo "len=${#pw}"
"#,
        Opts::default(),
    );
    r.wait_output("PROMPT1");
    r.send("visible\n");
    r.wait_output("PROMPT2");
    // stty runs before PROMPT2 is printed, so the slave is !ECHO && ICANON now.
    r.send("hunter2\n");
    r.wait_output("len=7");
    assert_eq!(r.finish(), 0);
    let ev = events(&r.cast);
    let typed = input_text(&ev);
    assert!(typed.contains("visible"));
    assert!(!typed.contains("hunter2"), "{typed:?}");
    assert!(!output_text(&ev).contains("hunter2"));
}

/// `read -s` restores ECHO right after reading the line: the policy must use the termios
/// from before the bytes were forwarded (regression: it sampled them afterwards).
#[test]
fn secret_read_that_restores_echo_at_once_is_not_recorded() {
    struct Delay;
    impl Drop for Delay {
        fn drop(&mut self) {
            recorder::tests_hook::FORWARD_DELAY_US.store(0, Ordering::SeqCst);
        }
    }
    // Long enough for the child to read the line and restore ECHO before classification.
    recorder::tests_hook::FORWARD_DELAY_US.store(50_000, Ordering::SeqCst);
    let _delay = Delay;
    let r = start(
        r#"
echo READY
if [ -x /bin/bash ]; then
  /bin/bash -c 'read -rs pw; echo "len=${#pw}"'
else
  stty -echo; read pw; stty echo; echo "len=${#pw}"
fi
"#,
        Opts::default(),
    );
    r.wait_output("READY");
    let pts = r.registry.read(&r.key).unwrap().expect("registry entry").pts;
    let slave = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOCTTY)
        .open(&pts)
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while nix::sys::termios::tcgetattr(&slave)
        .unwrap()
        .local_flags
        .contains(nix::sys::termios::LocalFlags::ECHO)
    {
        assert!(Instant::now() < deadline, "the child never turned ECHO off");
        std::thread::sleep(Duration::from_millis(5));
    }
    drop(slave);
    r.send("secret\r");
    r.wait_output("len=6");
    assert_eq!(r.finish(), 0);
    let raw = std::fs::read_to_string(&r.cast).unwrap();
    assert!(!raw.contains("secret"), "{raw}");
}

#[test]
fn user_pause_hides_output_but_keeps_passthrough() {
    let r = start(
        r#"
echo before
mark '{"v":1,"k":"user-pause","reason":"lunch"}'
echo hidden-output
mark '{"v":1,"k":"cmd","sh":1,"n":1,"typed":"secret"}'
mark '{"v":1,"k":"user-resume"}'
echo after
"#,
        Opts::default(),
    );
    assert_eq!(r.finish(), 0);
    assert!(r.host_output().contains("hidden-output"));
    let ev = events(&r.cast);
    let out = output_text(&ev);
    assert!(out.contains("before") && out.contains("after"));
    assert!(!out.contains("hidden-output"));
    assert_eq!(marker_kinds(&ev), ["session", "pause", "resume", "stats"]);
    let ms = markers(&ev);
    assert_eq!(
        (ms[1]["reason"].as_str(), ms[1]["detail"].as_str()),
        (Some("user"), Some("lunch"))
    );
}

#[test]
fn multiplexer_in_foreground_pauses_recording() {
    let fake = Arc::new(FakeDetector::default());
    let r = start(
        r#"
echo before
read a
echo "during-$a"
read b
echo "after-$b"
"#,
        Opts {
            detector: Arc::clone(&fake) as Arc<dyn Detector + Send + Sync>,
            ..Opts::default()
        },
    );
    r.wait_output("before");
    *fake.fg.lock().unwrap() = Some(Foreground::Multiplexer {
        pid: 4242,
        comm: "tmux: client".into(),
        pgrp: 4242,
    });
    r.wait_state(RecState::Paused);
    r.send("x\n");
    r.wait_output("during-x");
    *fake.fg.lock().unwrap() = Some(Foreground::Shell);
    r.wait_state(RecState::Live);
    r.send("y\n");
    assert_eq!(r.finish(), 0);
    let ev = events(&r.cast);
    let out = output_text(&ev);
    assert!(out.contains("before") && out.contains("after-y"), "{out:?}");
    assert!(!out.contains("during"), "{out:?}");
    let typed = input_text(&ev);
    assert!(!typed.contains('x') && typed.contains('y'), "{typed:?}");
    let kinds = marker_kinds(&ev);
    assert_eq!(kinds, ["session", "multiplexer", "pause", "resume", "stats"]);
    assert_eq!(markers(&ev)[3]["reason"], "foreground");
}

#[test]
fn corroborated_nested_recorder_pauses_until_nested_end() {
    let fake = Arc::new(FakeDetector::default());
    fake.nested_ok.store(true, Ordering::SeqCst);
    let r = start(
        r#"
echo outer
rmark '{"v":1,"k":"nested-start","uuid":"child-1","pid":77,"starttime":5,"scope":"team","user":"root","sudo_user":"alice"}'
echo root-only
rmark '{"v":1,"k":"nested-end","uuid":"child-1","pid":77,"starttime":5,"scope":"team"}'
echo back
"#,
        Opts {
            detector: Arc::clone(&fake) as Arc<dyn Detector + Send + Sync>,
            ..Opts::default()
        },
    );
    assert_eq!(r.finish(), 0);
    let ev = events(&r.cast);
    let out = output_text(&ev);
    assert!(out.contains("outer") && out.contains("back") && !out.contains("root-only"));
    assert_eq!(marker_kinds(&ev), ["session", "nested", "pause", "resume", "stats"]);
    let ms = markers(&ev);
    assert_eq!(
        (ms[1]["child_uuid"].as_str(), ms[1]["mode"].as_str()),
        (Some("child-1"), Some("pause"))
    );
    assert_eq!(ms[3]["reason"], "nested-end");
}

#[test]
fn resize_is_recorded_and_forwarded() {
    let r = start(
        r#"
echo READY
read go
stty size
"#,
        Opts::default(),
    );
    r.wait_output("READY");
    let ws = pty::Winsize {
        ws_row: 40,
        ws_col: 132,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    pty::set_winsize(r.master_fd(), &ws);
    // The host pty is not our controlling tty: deliver the SIGWINCH ourselves.
    nix::sys::signal::raise(nix::sys::signal::Signal::SIGWINCH).unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !events(&r.cast).iter().any(|(c, _)| c == "r") && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    r.send("go\n");
    assert_eq!(r.finish(), 0);
    let ev = events(&r.cast);
    assert!(ev.contains(&("r".into(), "132x40".into())), "{ev:?}");
    assert!(output_text(&ev).contains("40 132"));
}

#[test]
fn hangup_ends_the_shell_and_writes_exit() {
    let mut r = start(
        r#"
sleep 30 &
echo "BG=$!"
wait
"#,
        Opts::default(),
    );
    r.wait_output("BG=");
    let bg: u32 = r
        .host_output()
        .split("BG=")
        .nth(1)
        .and_then(|s| s.split_whitespace().next())
        .and_then(|s| s.parse().ok())
        .unwrap();
    let started = Instant::now();
    r.hangup();
    let status = r.finish();
    assert!(started.elapsed() < Duration::from_secs(5));
    assert_eq!(status, 128 + libc::SIGHUP);
    let ev = events(&r.cast);
    assert_eq!(ev.last().unwrap().0, "x");
    let deadline = Instant::now() + Duration::from_secs(3);
    while Path::new(&format!("/proc/{bg}")).exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(!Path::new(&format!("/proc/{bg}")).exists(), "background job got SIGHUP");
}

#[test]
fn headless_stdin_eof_is_not_a_hangup() {
    let r = start(
        r#"
sleep 0.3
echo headless-done
"#,
        Opts {
            tty: false,
            ..Opts::default()
        },
    );
    assert_eq!(r.finish(), 0);
    assert!(output_text(&events(&r.cast)).contains("headless-done"));
}

#[test]
fn keeping_markers_leaves_them_in_output() {
    let r = start(
        r#"mark '{"v":1,"k":"cmd","sh":1,"n":1,"typed":"x"}'"#,
        Opts {
            record: RecordConfig {
                strip_markers: false,
                ..RecordConfig::default()
            },
            ..Opts::default()
        },
    );
    assert_eq!(r.finish(), 0);
    let hex = NONCE.to_hex();
    assert!(r.host_output().contains(&hex));
    let raw = std::fs::read_to_string(&r.cast).unwrap();
    assert!(!raw.contains(&hex), "the nonce never reaches the cast: {raw}");
    let ev = events(&r.cast);
    assert!(marker_kinds(&ev).contains(&"cmd".to_owned()));
    assert!(
        output_text(&ev).contains("\x1b]7777;cinelog;1;<cinelog-nonce>;j;"),
        "the kept marker bytes stay in `o`, scrubbed"
    );
}

#[test]
fn input_disabled_records_no_keystrokes() {
    let r = start(
        "echo READY; read a; echo \"got-$a\"",
        Opts {
            record: RecordConfig {
                input: false,
                ..RecordConfig::default()
            },
            ..Opts::default()
        },
    );
    r.wait_output("READY");
    r.send("abc\n");
    r.wait_output("got-abc");
    assert_eq!(r.finish(), 0);
    assert!(input_text(&events(&r.cast)).is_empty());
}

/// Regression: the output thread blocked in write(2) on a host that stopped reading, and
/// shutdown joined it forever (the recorder then ignored hangups and SIGTERM).
#[test]
fn hangup_with_a_stalled_host_output_still_ends() {
    let mut r = start(
        r#"
echo started
head -c 3000000 /dev/zero | tr '\0' x
echo after
sleep 30
"#,
        Opts {
            stalled_output: true,
            ..Opts::default()
        },
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    while !std::fs::read_to_string(&r.cast).unwrap_or_default().contains("started") {
        assert!(Instant::now() < deadline, "no output recorded");
        std::thread::sleep(Duration::from_millis(10));
    }
    // Let the pipe fill up so the recorder is stuck on the host side.
    std::thread::sleep(Duration::from_millis(300));
    let started = Instant::now();
    r.hangup();
    let status = r.finish();
    assert!(started.elapsed() < Duration::from_secs(5), "{:?}", started.elapsed());
    assert_eq!(status, 128 + libc::SIGHUP);
    let ev = events(&r.cast);
    assert_eq!(ev.last().unwrap().0, "x");
    assert!(!output_text(&ev).contains("after"));
}

const EFFECTS_CHILD: &str = "CINELOG_REC_TEST_EFFECTS_CHILD";

fn wait_for(what: &str, mut ok: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !ok() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Child side of [`cwd_and_cmdline_follow_the_shell`]: a no-op in a normal test run.
#[test]
fn process_effects_child() {
    let Some(dirs) = std::env::var_os(EFFECTS_CHILD) else {
        return;
    };
    let dirs = dirs.into_string().unwrap();
    let (a, b) = dirs.split_once(':').unwrap();
    let comm = std::fs::read("/proc/self/comm").unwrap();
    let mut r = start(
        &format!(
            r#"
mark '{{"v":1,"k":"cmd","sh":1,"n":1,"typed":"cd a","cwd":"{a}"}}'
echo READY1
read go
mark '{{"v":1,"k":"done","sh":1,"n":1,"exit":0,"cwd":"{b}"}}'
echo READY2
read go
exec sleep 30
"#
        ),
        Opts {
            effects: ProcessEffects {
                mirror_cmdline: true,
                mirror_cwd: true,
                ignore_job_signals: false,
            },
            ..Opts::default()
        },
    );
    let cwd = || std::fs::read_link("/proc/self/cwd").unwrap();
    r.wait_output("READY1");
    wait_for("cwd of the cmd marker", || cwd() == Path::new(a));
    r.send("go\n");
    r.wait_output("READY2");
    wait_for("cwd of the done marker", || cwd() == Path::new(b));
    r.send("go\n");
    // The shell's pgrp now runs `sleep`: the recorder's own cmdline shows it (tmux
    // automatic-rename reads it), while comm stays the same for detection.
    wait_for("cmdline mirroring", || {
        std::fs::read("/proc/self/cmdline").unwrap().starts_with(b"sleep\0")
    });
    assert_eq!(std::fs::read("/proc/self/comm").unwrap(), comm);
    r.hangup();
    assert_eq!(r.finish(), 128 + libc::SIGHUP);
    println!("EFFECTS-OK");
}

/// §5.7 cwd and name mirroring are process-wide, so they run in a re-executed copy of
/// this test binary.
#[test]
fn cwd_and_cmdline_follow_the_shell() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().canonicalize().unwrap();
    let (a, b) = (base.join("a"), base.join("b"));
    std::fs::create_dir(&a).unwrap();
    std::fs::create_dir(&b).unwrap();
    let out = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "tests::process_effects_child",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(EFFECTS_CHILD, format!("{}:{}", a.display(), b.display()))
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success() && stdout.contains("EFFECTS-OK"),
        "{stdout}{}",
        String::from_utf8_lossy(&out.stderr)
    );
}
