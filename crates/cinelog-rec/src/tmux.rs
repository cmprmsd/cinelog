//! tmux integration of pane recorders (plan §5.6, §5.8): identity from `$TMUX`, the
//! creating client for shared root servers, and `ctx` context queries.

use std::io::Read;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use cinelog_proto::{Extra, TmuxContext, TmuxIdentity};

const CTX_FORMAT: &str = "#{session_name}\t#{window_index}\t#{window_name}\t#{pane_index}\t#{pane_id}";
/// A tmux query that takes longer (stopped or wedged server) is killed: the recorder
/// queries before the fork (root) and joins the ctx thread at exit, so nothing may hang.
const QUERY_TIMEOUT: Duration = Duration::from_secs(1);
const MAX_REPLY: u64 = 64 * 1024;

/// Waits for `child` until `timeout`, then kills and reaps it; `None` on timeout.
fn wait_deadline(child: &mut Child, timeout: Duration) -> Option<std::process::ExitStatus> {
    let deadline = Instant::now() + timeout;
    let mut nap = Duration::from_millis(1);
    loop {
        match child.try_wait() {
            Ok(Some(st)) => return Some(st),
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(nap);
                nap = (nap * 2).min(Duration::from_millis(20));
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
}

/// `$TMUX` (`socket,server_pid,session_id`) and `$TMUX_PANE` of a pane shell.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TmuxEnv {
    pub socket: String,
    pub server_pid: u32,
    pub session_id: String,
    pub pane: String,
}

impl TmuxEnv {
    pub(crate) fn parse(tmux: &str, pane: &str) -> Option<Self> {
        let mut it = tmux.rsplitn(3, ',');
        let session_id = it.next()?.to_owned();
        let server_pid = it.next()?.parse().ok()?;
        let socket = it.next()?.to_owned();
        (!socket.is_empty() && pane.starts_with('%')).then(|| Self {
            socket,
            server_pid,
            session_id: format!("${session_id}"),
            pane: pane.to_owned(),
        })
    }

    pub(crate) fn from_env() -> Option<Self> {
        let tmux = std::env::var("TMUX").ok()?;
        let pane = std::env::var("TMUX_PANE").ok()?;
        Self::parse(&tmux, &pane)
    }

    pub(crate) fn identity(&self, server_start: u64) -> TmuxIdentity {
        TmuxIdentity {
            socket: self.socket.clone(),
            server_pid: self.server_pid,
            server_start,
            session_id: self.session_id.clone(),
            pane: self.pane.clone(),
        }
    }
}

/// tmux binaries tried when the server's own one cannot be used.
const SYSTEM_TMUX: [&str; 3] = ["/usr/bin/tmux", "/usr/local/bin/tmux", "/bin/tmux"];

/// Root may only execute binaries that are root-owned along the whole path and free of
/// group/world write (§5.6), like the cinelog binary itself.
fn root_may_exec(path: &Path) -> bool {
    cinelog_core::location::insecure_component(path, 0, Path::new("/")).is_none()
}

/// The tmux binary to run: the server's own one (the client must speak the server's
/// protocol version), else a system one, else (not root) `tmux` from PATH. At EUID 0 only
/// binaries `trusted` accepts are used, and `None` means no queries at all (attribution
/// then falls back to loginuid).
fn choose_bin(server_exe: Option<PathBuf>, euid: u32, trusted: impl Fn(&Path) -> bool) -> Option<PathBuf> {
    let usable = |p: &Path| p.is_absolute() && p.exists() && (euid != 0 || trusted(p));
    server_exe
        .filter(|p| usable(p))
        .or_else(|| {
            SYSTEM_TMUX
                .iter()
                .map(Path::new)
                .find(|p| usable(p))
                .map(Path::to_path_buf)
        })
        .or_else(|| (euid != 0).then(|| PathBuf::from("tmux")))
}

/// Runs tmux commands against one server.
#[derive(Debug, Clone)]
pub(crate) struct TmuxClient {
    bin: Option<PathBuf>,
    env: TmuxEnv,
}

impl TmuxClient {
    pub(crate) fn new(env: TmuxEnv, euid: u32) -> Self {
        let server_exe = std::fs::read_link(format!("/proc/{}/exe", env.server_pid)).ok();
        let bin = choose_bin(server_exe, euid, root_may_exec);
        Self { bin, env }
    }

    fn display(&self, format: &str) -> Option<String> {
        let mut child = Command::new(self.bin.as_ref()?)
            .arg("-S")
            .arg(&self.env.socket)
            .args(["display-message", "-p", "-t", &self.env.pane, format])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .env_remove("TMUX")
            .spawn()
            .ok()?;
        // The reply is one short line and fits the pipe buffer, so the client never
        // blocks on its stdout while we wait for it to exit.
        let status = wait_deadline(&mut child, QUERY_TIMEOUT)?;
        let stdout = child.stdout.take()?;
        // Non-blocking: a leftover holder of the pipe's write end cannot stall the read.
        crate::pty::set_nonblocking(stdout.as_raw_fd()).ok()?;
        let mut reply = Vec::new();
        let _ = stdout.take(MAX_REPLY).read_to_end(&mut reply);
        status
            .success()
            .then(|| String::from_utf8_lossy(&reply).trim_end_matches('\n').to_owned())
    }

    /// Pid of the client tmux reports for the pane (the creating one right after
    /// `new-window`/`split-window`), for shared root servers (§5.6, Q4).
    pub(crate) fn client_pid(&self) -> Option<u32> {
        self.display("#{client_pid}")?.trim().parse().ok().filter(|&p| p > 0)
    }

    pub(crate) fn context(&self) -> Option<TmuxContext> {
        parse_context(&self.display(CTX_FORMAT)?)
    }
}

fn parse_context(line: &str) -> Option<TmuxContext> {
    let mut f = line.split('\t');
    let ctx = TmuxContext {
        session_name: f.next()?.to_owned(),
        window_index: f.next()?.to_owned(),
        window_name: f.next()?.to_owned(),
        pane_index: f.next()?.to_owned(),
        pane_id: f.next()?.to_owned(),
        extra: Extra::new(),
    };
    Some(ctx)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_tmux_env() {
        let e = TmuxEnv::parse("/tmp/tmux-1000/default,4242,3", "%12").unwrap();
        assert_eq!(e.socket, "/tmp/tmux-1000/default");
        assert_eq!(e.server_pid, 4242);
        assert_eq!(e.session_id, "$3");
        assert_eq!(e.pane, "%12");
        // Sockets may contain commas.
        let e = TmuxEnv::parse("/tmp/a,b/sock,1,0", "%0").unwrap();
        assert_eq!(e.socket, "/tmp/a,b/sock");
        assert!(TmuxEnv::parse("garbage", "%1").is_none());
        assert!(TmuxEnv::parse("/s,1,0", "").is_none());
        assert_eq!(e.identity(9).server_start, 9);
    }

    #[test]
    fn parses_context_lines() {
        let c = parse_context("main\t1\tvim\t0\t%3").unwrap();
        assert_eq!(
            (c.session_name.as_str(), c.window_index.as_str(), c.window_name.as_str()),
            ("main", "1", "vim")
        );
        assert_eq!((c.pane_index.as_str(), c.pane_id.as_str()), ("0", "%3"));
        assert!(parse_context("too\tshort").is_none());
    }

    #[test]
    fn root_only_runs_trusted_tmux_binaries() {
        let dir = tempfile::tempdir().unwrap();
        let user_tmux = dir.path().join("tmux");
        std::fs::write(&user_tmux, b"#!/bin/sh\n").unwrap();
        let exe = Some(user_tmux.clone());
        // Not root: the server's own binary, whoever owns it.
        assert_eq!(choose_bin(exe.clone(), 1000, |_| false), exe);
        // Root: a user-owned server binary (`su` keeping $TMUX) is never executed.
        let picked = choose_bin(exe.clone(), 0, root_may_exec);
        assert_ne!(picked.as_deref(), Some(user_tmux.as_path()));
        assert!(
            picked
                .as_deref()
                .is_none_or(|p| SYSTEM_TMUX.iter().any(|s| p == Path::new(s)))
        );
        assert_eq!(choose_bin(exe.clone(), 0, |_| false), None, "no PATH lookup as root");
        assert_eq!(choose_bin(exe.clone(), 0, |p| p == user_tmux), exe);
        assert!(choose_bin(None, 1000, |_| false).is_some());
        // The trust rule itself: a file in a user-owned temp dir is not root's.
        if nix::unistd::geteuid().as_raw() != 0 {
            assert!(!root_may_exec(&user_tmux));
        }
    }

    #[test]
    fn a_client_without_a_binary_makes_no_queries() {
        let env = TmuxEnv::parse("/nonexistent/sock,1,0", "%0").unwrap();
        let c = TmuxClient { bin: None, env };
        assert_eq!(c.client_pid(), None);
        assert!(c.context().is_none());
    }

    /// Regression: a wedged server (here a "tmux" that never answers) made every query,
    /// and with it the recorder's start (root) and exit (ctx thread join), hang.
    #[test]
    fn a_query_that_does_not_answer_is_killed() {
        let dir = tempfile::tempdir().unwrap();
        let fake = dir.path().join("tmux");
        let pidfile = dir.path().join("pid");
        std::fs::write(
            &fake,
            format!("#!/bin/sh\necho $$ > '{}'\nexec sleep 30\n", pidfile.display()),
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o700)).unwrap();
        let env = TmuxEnv::parse("/nonexistent/sock,1,0", "%0").unwrap();
        let c = TmuxClient { bin: Some(fake), env };
        let started = Instant::now();
        assert_eq!(c.client_pid(), None);
        let took = started.elapsed();
        assert!(
            took >= QUERY_TIMEOUT && took < QUERY_TIMEOUT + Duration::from_secs(2),
            "{took:?}"
        );
        let pid = std::fs::read_to_string(&pidfile).unwrap();
        assert!(
            !Path::new(&format!("/proc/{}", pid.trim())).exists(),
            "the hung query was killed and reaped"
        );
    }

    #[test]
    fn query_reply_and_exit_status_are_used() {
        let dir = tempfile::tempdir().unwrap();
        let fake = dir.path().join("tmux");
        std::fs::write(&fake, "#!/bin/sh\n[ \"$6\" = '%7' ] && echo 4242\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o700)).unwrap();
        let client = |pane: &str| TmuxClient {
            bin: Some(fake.clone()),
            env: TmuxEnv::parse("/s,1,0", pane).unwrap(),
        };
        assert_eq!(client("%7").client_pid(), Some(4242));
        assert_eq!(client("%8").client_pid(), None, "non-zero exit");
    }

    /// Real tmux on a private `-L cinelog-…` server (skipped when tmux is not installed).
    #[test]
    fn queries_a_real_server() {
        if Command::new("tmux").arg("-V").output().is_err() {
            return;
        }
        let label = format!("cinelog-rec-test-{}", std::process::id());
        let tmux = |args: &[&str]| {
            Command::new("tmux")
                .args(["-L", &label, "-f", "/dev/null"])
                .args(args)
                .env_remove("TMUX")
                .stdin(Stdio::null())
                .stderr(Stdio::null())
                .output()
                .unwrap()
        };
        struct Kill<'a>(
            &'a dyn Fn(&[&str]) -> std::process::Output,
            std::cell::RefCell<Option<String>>,
        );
        impl Drop for Kill<'_> {
            fn drop(&mut self) {
                (self.0)(&["kill-server"]);
                if let Some(sock) = self.1.borrow().as_deref() {
                    let _ = std::fs::remove_file(sock);
                }
            }
        }
        let kill = Kill(&tmux, std::cell::RefCell::new(None));
        let fmt = "#{socket_path},#{pid},#{pane_id}";
        let out = tmux(&["new-session", "-d", "-s", "cl-rec-test", "-P", "-F", fmt, "sleep 30"]);
        let line = String::from_utf8_lossy(&out.stdout).trim().to_owned();
        let mut f = line.rsplitn(3, ',');
        let (pane, pid, sock) = (f.next().unwrap(), f.next().unwrap(), f.next().unwrap());
        kill.1.replace(Some(sock.to_owned()));
        let env = TmuxEnv::parse(&format!("{sock},{pid},0"), pane).unwrap();
        let ctx = TmuxClient::new(env, 1000).context().expect("context");
        assert_eq!(ctx.session_name, "cl-rec-test");
        assert_eq!(ctx.pane_id, pane);
    }
}
