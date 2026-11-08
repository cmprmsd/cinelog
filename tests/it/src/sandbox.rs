//! Throwaway user environment for one scenario.
//!
//! Layout below the sandbox root:
//!
//! ```text
//! home/                      HOME and ZDOTDIR (.zshrc sources plugin/cinelog.plugin.zsh)
//! home/.config/cinelog/      config.toml (logs → logs/, autostart off, quiet shell options)
//! home/.local/state/         XDG_STATE_HOME (rec.log, per-host DB dir)
//! home/.local/bin/cinelog    symlink to the binary under test
//! run/                       XDG_RUNTIME_DIR (registry: run/cinelog/pts-N.json)
//! logs/                      paths.logs: every cast of the scenario
//! plugin/                    copy of cinelog.plugin.zsh + shell/, bin/cinelog symlink
//! work/                      scratch files of the scenario
//! ```
//!
//! tmux and screen sockets live in a short directory (`$TMUX_TMPDIR`, `$SCREENDIR`) because
//! of the 108-byte `sun_path` limit: `<root>/s` when that is short enough, otherwise a
//! private `clit-<id>` below `$CINELOG_IT_SOCKDIR` (default `/tmp`) that is removed on drop.

use std::fs;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde_json::Value;

use crate::binary::{self, Flavour};
use crate::cast::{Cast, Marker, find_casts};
use crate::procs;
use crate::tmux::{Pane, Tmux};
use crate::util::{scaled, sh_quote, unique_id, wait_until, which};

/// Environment marker every sandboxed process inherits (it is not a `CINELOG_*` variable,
/// so neither the plugin nor the recorder strips it). Cleanup finds processes by it.
pub const MARKER_VAR: &str = "CLIT_SANDBOX";

#[derive(Debug, Clone)]
pub struct SandboxOptions {
    /// Binary to install; `None` installs none (the "missing binary" scenario).
    pub flavour: Option<Flavour>,
    /// Extra `key = value` lines for `[record]`.
    pub record: Vec<String>,
    /// `[shell]` overrides as `(key, value)`; defaults are all quiet (`false`).
    pub shell: Vec<(String, String)>,
    /// Written verbatim instead of the generated config (corrupt-config scenario).
    pub raw_config: Option<String>,
    /// `paths.logs` override (e.g. a tmpfs mount point); relative paths are below the root.
    pub logs: Option<PathBuf>,
}

impl Default for SandboxOptions {
    fn default() -> Self {
        Self {
            flavour: Some(Flavour::Plain),
            record: Vec::new(),
            shell: Vec::new(),
            raw_config: None,
            logs: None,
        }
    }
}

#[derive(Debug)]
pub struct Sandbox {
    pub id: String,
    pub root: PathBuf,
    pub home: PathBuf,
    pub run: PathBuf,
    pub logs: PathBuf,
    pub plugin_dir: PathBuf,
    pub work: PathBuf,
    pub sock_dir: PathBuf,
    pub zsh: PathBuf,
    /// The binary under test (if installed).
    pub bin: Option<PathBuf>,
    /// `PATH` of every sandboxed process; no directory on it holds a foreign `cinelog`.
    path: String,
    user: String,
    own_sock_dir: bool,
    tempdir: Option<tempfile::TempDir>,
    servers: Mutex<Vec<Tmux>>,
    /// `/proc/<pid>/root` of a process whose private mount namespace holds the logs.
    view: Mutex<Option<PathBuf>>,
}

fn mkdir(p: &Path, mode: u32) -> Result<()> {
    fs::DirBuilder::new()
        .recursive(true)
        .mode(mode)
        .create(p)
        .with_context(|| format!("mkdir {}", p.display()))?;
    fs::set_permissions(p, fs::Permissions::from_mode(mode))?;
    Ok(())
}

fn copy_tree(from: &Path, to: &Path) -> Result<()> {
    mkdir(to, 0o755)?;
    for e in fs::read_dir(from)? {
        let e = e?;
        let dest = to.join(e.file_name());
        if e.file_type()?.is_dir() {
            copy_tree(&e.path(), &dest)?;
        } else if e.file_type()?.is_file() {
            fs::copy(e.path(), &dest)?;
        }
    }
    Ok(())
}

/// Longest socket path the harness creates below the socket dir.
const SOCKET_SUFFIX_LEN: usize = "/tmux-4294967295/cinelog-it-inner-xxxxxxxx-xxxxxxxx".len();

/// The directories of the tools the scenarios run, then the standard system directories.
fn default_path_dirs() -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    let tools = ["zsh", "tmux", "screen", "python3", "unshare", "script"];
    let tool_dirs = tools.iter().filter_map(|t| which(t)?.parent().map(Path::to_path_buf));
    for dir in tool_dirs.chain(["/usr/local/bin", "/usr/bin", "/bin"].map(PathBuf::from)) {
        if !dirs.contains(&dir) {
            dirs.push(dir);
        }
    }
    dirs
}

/// The sandbox `PATH`: `own_bin`, then `dirs`. A directory that holds a `cinelog` (a
/// system-wide or personal install on the test machine) is replaced by a shadow copy below
/// `shadow_root` that links every other entry, so only the sandbox decides which `cinelog`
/// the plugin finds.
fn isolated_path(own_bin: &Path, dirs: &[PathBuf], shadow_root: &Path) -> Result<String> {
    let mut path = vec![own_bin.to_string_lossy().into_owned()];
    for (i, dir) in dirs.iter().enumerate() {
        let entry = if dir.join("cinelog").symlink_metadata().is_ok() {
            shadow_dir(dir, &shadow_root.join(i.to_string()), &["cinelog"])?
        } else {
            dir.clone()
        };
        path.push(entry.to_string_lossy().into_owned());
    }
    Ok(path.join(":"))
}

/// A directory of symlinks to every entry of `dir` except `exclude`.
fn shadow_dir(dir: &Path, shadow: &Path, exclude: &[&str]) -> Result<PathBuf> {
    mkdir(shadow, 0o755)?;
    for e in fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
        let e = e?;
        let name = e.file_name();
        if exclude.iter().any(|x| name == *x) {
            continue;
        }
        symlink(e.path(), shadow.join(&name))?;
    }
    Ok(shadow.to_path_buf())
}

/// The sandbox's socket directory and whether it lives outside the sandbox tree:
/// `<root>/s` when socket paths below it fit into `sun_path`, else `clit-<id>` below
/// `CINELOG_IT_SOCKDIR` (default `/tmp`).
fn socket_dir(root: &Path, id: &str) -> Result<(PathBuf, bool)> {
    let short = root.join("s");
    if short.as_os_str().len() + SOCKET_SUFFIX_LEN < 100 {
        return Ok((short, false));
    }
    let base = std::env::var_os("CINELOG_IT_SOCKDIR")
        .filter(|v| !v.is_empty())
        .map_or_else(|| PathBuf::from("/tmp"), PathBuf::from);
    let dir = base.join(format!("clit-{id}"));
    if dir.as_os_str().len() + SOCKET_SUFFIX_LEN >= 100 {
        bail!(
            "socket dir {} is too long for sun_path; set CINELOG_IT_SOCKDIR to a shorter directory",
            dir.display()
        );
    }
    Ok((dir, true))
}

impl Sandbox {
    pub fn new() -> Result<Self> {
        Self::with(SandboxOptions::default())
    }

    pub fn with(opts: SandboxOptions) -> Result<Self> {
        let tempdir = crate::util::scratch_dir()?;
        let root = tempdir.path().to_path_buf();
        let id = unique_id();
        let home = root.join("home");
        let run = root.join("run");
        let logs = match &opts.logs {
            Some(p) if p.is_relative() => root.join(p),
            Some(p) => p.clone(),
            None => root.join("logs"),
        };
        let plugin_dir = root.join("plugin");
        let work = root.join("work");
        for (d, mode) in [
            (&home, 0o700),
            (&home.join(".config/cinelog"), 0o700),
            (&home.join(".local/state"), 0o700),
            (&home.join(".local/bin"), 0o755),
            (&run, 0o700),
            (&logs, 0o700),
            (&work, 0o755),
        ] {
            mkdir(d, mode)?;
        }
        let (sock_dir, own_sock_dir) = socket_dir(&root, &id)?;
        mkdir(&sock_dir, 0o700)?;
        mkdir(&sock_dir.join("screen"), 0o700)?;

        let zsh = which("zsh").unwrap_or_else(|| PathBuf::from("/bin/zsh"));
        let user = nix::unistd::User::from_uid(nix::unistd::getuid())
            .ok()
            .flatten()
            .map(|u| u.name)
            .unwrap_or_else(|| "user".into());

        // Plugin copy: the entry file and the shell/ helpers it may source.
        mkdir(&plugin_dir.join("bin"), 0o755)?;
        let repo = binary::repo_root();
        let plugin_src = binary::plugin_file();
        if plugin_src.is_file() {
            fs::copy(&plugin_src, plugin_dir.join("cinelog.plugin.zsh"))?;
        }
        if repo.join("shell").is_dir() {
            copy_tree(&repo.join("shell"), &plugin_dir.join("shell"))?;
        }

        let bin = match opts.flavour {
            Some(f) => {
                let b = binary::cinelog(f)?;
                symlink(&b, plugin_dir.join("bin/cinelog"))?;
                symlink(&b, home.join(".local/bin/cinelog"))?;
                Some(b)
            }
            None => None,
        };

        let path = isolated_path(&home.join(".local/bin"), &default_path_dirs(), &root.join("pathshadow"))?;
        let sb = Self {
            id,
            root,
            home,
            run,
            logs,
            plugin_dir,
            work,
            sock_dir,
            zsh,
            bin,
            path,
            user,
            own_sock_dir,
            tempdir: Some(tempdir),
            servers: Mutex::new(Vec::new()),
            view: Mutex::new(None),
        };
        sb.write_zshrc()?;
        let config = opts.raw_config.clone().unwrap_or_else(|| sb.config_text(&opts));
        sb.write_config(&config)?;
        Ok(sb)
    }

    /// The sandbox `.zshrc`; `with_plugin = false` leaves out only the `source` line.
    fn zshrc_text(&self, with_plugin: bool) -> String {
        let mut rc = format!(
            "# cinelog integration-test sandbox {id}\n\
             PROMPT='clit%# '\n\
             RPROMPT=\n\
             path=({bin} $path)\n\
             unsetopt beep\n\
             setopt interactive_comments\n\
             HISTFILE={home}/.zsh_history HISTSIZE=1000 SAVEHIST=0\n",
            id = self.id,
            bin = sh_quote(&self.home.join(".local/bin").to_string_lossy()),
            home = sh_quote(&self.home.to_string_lossy()),
        );
        if with_plugin {
            let plugin = self.plugin_dir.join("cinelog.plugin.zsh");
            rc.push_str(&format!("source {}\n", sh_quote(&plugin.to_string_lossy())));
        }
        rc
    }

    fn write_zshrc(&self) -> Result<()> {
        fs::write(self.home.join(".zshrc"), self.zshrc_text(true))?;
        fs::write(self.home.join(".zshenv"), "")?;
        Ok(())
    }

    /// A `ZDOTDIR` whose `.zshrc` equals the sandbox one without the plugin: the shell
    /// startup cinelog is measured against (B2).
    pub fn baseline_zdotdir(&self) -> Result<PathBuf> {
        let dir = self.root.join("zdot-baseline");
        mkdir(&dir, 0o700)?;
        fs::write(dir.join(".zshrc"), self.zshrc_text(false))?;
        fs::write(dir.join(".zshenv"), "")?;
        Ok(dir)
    }

    fn config_text(&self, opts: &SandboxOptions) -> String {
        let q = |p: &Path| serde_json::to_string(&p.to_string_lossy()).expect("string");
        let mut shell: Vec<(String, String)> = ["motd", "banners", "check_free_space", "external_ip", "title"]
            .iter()
            .map(|k| ((*k).to_owned(), "false".to_owned()))
            .collect();
        for (k, v) in &opts.shell {
            match shell.iter_mut().find(|(key, _)| key == k) {
                Some(slot) => slot.1 = v.clone(),
                None => shell.push((k.clone(), v.clone())),
            }
        }
        let mut s = format!(
            "[paths]\nlogs = {}\nstate = {}\n\n[record]\n",
            q(&self.logs),
            q(&self.state_dir())
        );
        for line in &opts.record {
            s.push_str(line);
            s.push('\n');
        }
        s.push_str("\n[shell]\n");
        for (k, v) in shell {
            s.push_str(&format!("{k} = {v}\n"));
        }
        s.push_str("\n[server]\nautostart = \"off\"\n");
        s
    }

    pub fn config_file(&self) -> PathBuf {
        self.home.join(".config/cinelog/config.toml")
    }

    pub fn write_config(&self, text: &str) -> Result<()> {
        let p = self.config_file();
        fs::write(&p, text)?;
        fs::set_permissions(&p, fs::Permissions::from_mode(0o600))?;
        Ok(())
    }

    /// `paths.state` of the sandbox (rec.log lives here).
    pub fn state_dir(&self) -> PathBuf {
        self.home.join(".local/state/cinelog")
    }

    pub fn registry_dir(&self) -> PathBuf {
        self.run.join("cinelog")
    }

    pub fn user(&self) -> &str {
        &self.user
    }

    /// Environment of every sandboxed shell (the pane command uses `env -i` with exactly this).
    pub fn env(&self) -> Vec<(String, String)> {
        let s = |p: &Path| p.to_string_lossy().into_owned();
        vec![
            ("HOME".into(), s(&self.home)),
            ("ZDOTDIR".into(), s(&self.home)),
            ("USER".into(), self.user.clone()),
            ("LOGNAME".into(), self.user.clone()),
            ("SHELL".into(), s(&self.zsh)),
            ("PATH".into(), self.path.clone()),
            ("TERM".into(), "xterm-256color".into()),
            ("LANG".into(), "C.UTF-8".into()),
            ("XDG_CONFIG_HOME".into(), s(&self.home.join(".config"))),
            ("XDG_STATE_HOME".into(), s(&self.home.join(".local/state"))),
            ("XDG_RUNTIME_DIR".into(), s(&self.run)),
            ("TMUX_TMPDIR".into(), s(&self.sock_dir)),
            ("SCREENDIR".into(), s(&self.sock_dir.join("screen"))),
            (MARKER_VAR.into(), self.id.clone()),
        ]
    }

    /// Resolves `name` on the sandbox `PATH`, like the shell's command lookup.
    pub fn resolve(&self, name: &str) -> Option<PathBuf> {
        self.path
            .split(':')
            .map(|d| Path::new(d).join(name))
            .find(|p| crate::util::is_executable(p))
    }

    /// `env -i <sandbox env> <extra> <program…>`.
    pub fn env_argv(&self, extra: &[(&str, &str)], program: &[String]) -> Vec<String> {
        let mut v = vec!["env".to_owned(), "-i".to_owned()];
        v.extend(self.env().into_iter().map(|(k, val)| format!("{k}={val}")));
        v.extend(extra.iter().map(|(k, val)| format!("{k}={val}")));
        v.extend(program.iter().cloned());
        v
    }

    /// Pane command for an interactive sandbox zsh (the plugin decides about recording).
    pub fn shell_argv(&self) -> Vec<String> {
        self.env_argv(&[], &[self.zsh.to_string_lossy().into_owned(), "-i".into()])
    }

    /// Pane command that runs `<bin> rec -- zsh` directly (environment for the recorder
    /// itself, e.g. test hooks, survives because no outer plugin unsets it).
    pub fn direct_rec_argv(&self, bin: &Path, extra: &[(&str, &str)]) -> Vec<String> {
        self.env_argv(
            extra,
            &[
                bin.to_string_lossy().into_owned(),
                "rec".into(),
                "--".into(),
                self.zsh.to_string_lossy().into_owned(),
            ],
        )
    }

    /// Wraps `inner` so it runs with a fresh `size`-byte tmpfs mounted on `mountpoint`
    /// (visible only inside the namespaces), pre-filled with `prefill` bytes, as the real uid:
    /// `unshare -rm` for the mount, then a nested `unshare -U --map-user` back to our ids.
    pub fn tmpfs_argv(&self, mountpoint: &Path, size: &str, prefill: u64, inner: Vec<String>) -> Vec<String> {
        let (uid, gid) = (nix::unistd::getuid(), nix::unistd::getgid());
        let script = format!(
            "mount -t tmpfs -o size={size},mode=0700 tmpfs \"$1\" || exit 99; \
             if [ \"$2\" -gt 0 ]; then head -c \"$2\" /dev/zero > \"$1/filler\" 2>/dev/null; fi; \
             shift 2; exec unshare -U --map-user={uid} --map-group={gid} \"$@\""
        );
        let mut v = vec![
            "unshare".to_owned(),
            "-rm".into(),
            "sh".into(),
            "-c".into(),
            script,
            "sh".into(),
            mountpoint.to_string_lossy().into_owned(),
            prefill.to_string(),
        ];
        v.extend(inner);
        v
    }

    /// Starts this sandbox's harness tmux server (`-L cinelog-it-<id>`).
    pub fn tmux(&self) -> Result<Tmux> {
        let t = Tmux::new(format!("cinelog-it-{}", self.id), self.env());
        t.start_server()?;
        self.servers.lock().expect("lock").push(t.clone());
        Ok(t)
    }

    /// A tmux server that a sandboxed shell will start (`tmux -L <socket>`), registered for
    /// cleanup. Returns the handle and the socket name to type into the shell.
    pub fn inner_tmux(&self, tag: &str) -> Tmux {
        let t = Tmux::new(format!("cinelog-it-{tag}-{}", self.id), self.env());
        self.servers.lock().expect("lock").push(t.clone());
        t
    }

    /// `tmux -L <socket> -f /dev/null` as typed into a sandboxed shell.
    pub fn tmux_cmd(t: &Tmux) -> String {
        format!("tmux -L {} -f /dev/null", t.socket)
    }

    /// Reads the logs through `/proc/<pid>/root` from now on: the logs live on a tmpfs that
    /// is mounted only inside that process's namespace ([`Sandbox::tmpfs_argv`]).
    pub fn view_through(&self, pid: u32) {
        *self.view.lock().expect("lock") = Some(PathBuf::from(format!("/proc/{pid}/root")));
    }

    /// `path` (a path as the recorder sees it) as the test can read it.
    pub fn visible(&self, path: &Path) -> PathBuf {
        match &*self.view.lock().expect("lock") {
            Some(v) => v.join(path.strip_prefix("/").unwrap_or(path)),
            None => path.to_path_buf(),
        }
    }

    pub fn cast_paths(&self) -> Vec<PathBuf> {
        find_casts(&self.visible(&self.logs))
    }

    pub fn casts(&self) -> Vec<Cast> {
        self.cast_paths().iter().filter_map(|p| Cast::load(p).ok()).collect()
    }

    /// Waits for the cast whose session marker names `rec_pid` (the pane pid: the pane
    /// process execs zsh and then the recorder, keeping its pid).
    pub fn wait_cast_of(&self, rec_pid: u32, timeout: Duration) -> Result<PathBuf> {
        wait_until(&format!("a cast recorded by pid {rec_pid}"), timeout, || {
            self.casts()
                .into_iter()
                .find(|c| c.rec_pid() == Some(rec_pid))
                .map(|c| c.path)
        })
        .with_context(|| format!("casts present: {:?}", self.cast_paths()))
    }

    /// Waits for a cast (other than `known`) whose session marker satisfies `pred`.
    pub fn wait_new_cast(
        &self,
        known: &[PathBuf],
        timeout: Duration,
        pred: impl Fn(&Marker) -> bool,
    ) -> Result<PathBuf> {
        wait_until("a new cast", timeout, || {
            self.casts()
                .into_iter()
                .filter(|c| !known.contains(&c.path))
                .find(|c| c.session().as_ref().is_some_and(&pred))
                .map(|c| c.path)
        })
        .with_context(|| format!("casts present: {:?}", self.cast_paths()))
    }

    /// Opens a recorded shell in a new window of `t` (the harness server).
    pub fn start_recorded(&self, t: &Tmux, session: &str) -> Result<Recorded> {
        self.start_recorded_argv(t, session, &self.shell_argv())
    }

    pub fn start_recorded_argv(&self, t: &Tmux, session: &str, argv: &[String]) -> Result<Recorded> {
        let pane = if t.run(&["has-session", "-t", session]).is_ok() {
            t.new_window(session, argv)?
        } else {
            t.new_session(session, 120, 40, argv)?
        };
        Recorded::attach(self, pane)
    }

    /// Opens an unrecorded-or-recorded sandbox shell and waits for its prompt only.
    pub fn start_shell(&self, t: &Tmux, session: &str, argv: &[String]) -> Result<Pane> {
        let pane = if t.run(&["has-session", "-t", session]).is_ok() {
            t.new_window(session, argv)?
        } else {
            t.new_session(session, 120, 40, argv)?
        };
        pane.wait_prompt(Pane::startup_timeout())?;
        Ok(pane)
    }

    /// Processes that belong to this sandbox.
    pub fn processes(&self) -> Vec<u32> {
        procs::with_env(MARKER_VAR, &self.id)
    }

    /// Parsed registry files (`run/cinelog/*.json`).
    pub fn registry(&self) -> Vec<(PathBuf, Value)> {
        let Ok(rd) = fs::read_dir(self.registry_dir()) else {
            return Vec::new();
        };
        let mut v: Vec<(PathBuf, Value)> = rd
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x == "json"))
            .filter_map(|p| Some((p.clone(), serde_json::from_slice(&fs::read(&p).ok()?).ok()?)))
            .collect();
        v.sort_by(|a, b| a.0.cmp(&b.0));
        v
    }

    pub fn rec_log(&self) -> String {
        fs::read_to_string(self.state_dir().join("rec.log")).unwrap_or_default()
    }

    /// Kills the tmux servers and every remaining sandbox process.
    pub fn shutdown(&self) {
        for t in self.servers.lock().expect("lock").iter() {
            t.kill_server();
        }
        for _ in 0..3 {
            let pids = self.processes();
            if pids.is_empty() {
                break;
            }
            procs::terminate(&pids, Duration::from_millis(500));
        }
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        self.shutdown();
        // Scenarios make directories read-only; restore so the tree can be removed.
        restore_perms(&self.root);
        if self.own_sock_dir {
            let _ = fs::remove_dir_all(&self.sock_dir);
        }
        if let Some(td) = self.tempdir.take()
            && std::env::var("CINELOG_IT_KEEP").is_ok_and(|v| v == "1")
        {
            eprintln!("keeping sandbox {}", td.keep().display());
        }
    }
}

fn restore_perms(dir: &Path) {
    let Ok(meta) = fs::symlink_metadata(dir) else { return };
    if !meta.is_dir() {
        return;
    }
    let _ = fs::set_permissions(dir, fs::Permissions::from_mode(meta.permissions().mode() | 0o700));
    if let Ok(rd) = fs::read_dir(dir) {
        for e in rd.flatten() {
            restore_perms(&e.path());
        }
    }
}

/// A pane whose shell is being recorded, with its cast.
#[derive(Debug, Clone)]
pub struct Recorded {
    pub pane: Pane,
    pub cast_path: PathBuf,
}

/// Default wait for a marker to show up in a cast.
pub fn cast_timeout() -> Duration {
    scaled(10.0)
}

impl Recorded {
    /// Waits until the shell in `pane` is recorded (its cast exists) and shows a prompt.
    pub fn attach(sb: &Sandbox, pane: Pane) -> Result<Self> {
        let cast_path = sb
            .wait_cast_of(pane.pid, Pane::startup_timeout())
            .with_context(|| format!("screen:\n{}", pane.capture().unwrap_or_default()))?;
        pane.wait_prompt(Pane::startup_timeout())?;
        let r = Self { pane, cast_path };
        r.wait_cast("the hello marker", cast_timeout(), |c| c.first_marker("hello"))?;
        Ok(r)
    }

    /// Like [`Recorded::attach`] for a pane of a server the harness did not start; the cast
    /// is identified by the pane pid all the same.
    pub fn attach_pane(sb: &Sandbox, t: &Tmux, pane_id: &str) -> Result<Self> {
        Self::attach(sb, t.pane(pane_id)?)
    }

    pub fn cast(&self) -> Result<Cast> {
        Cast::load(&self.cast_path)
    }

    pub fn uuid(&self) -> Result<String> {
        self.cast()?.uuid().context("cast without session uuid")
    }

    pub fn wait_cast<T>(&self, what: &str, timeout: Duration, f: impl Fn(&Cast) -> Option<T>) -> Result<T> {
        wait_until(&format!("{what} in {}", self.cast_path.display()), timeout, || {
            Cast::load(&self.cast_path).ok().as_ref().and_then(&f)
        })
        .with_context(|| {
            format!(
                "markers: {:?}\nscreen:\n{}",
                self.cast()
                    .map(|c| c.markers().into_iter().map(|m| m.json.to_string()).collect::<Vec<_>>())
                    .unwrap_or_default(),
                self.pane.capture().unwrap_or_default()
            )
        })
    }

    pub fn wait_marker(&self, kind: &str, pred: impl Fn(&Marker) -> bool) -> Result<Marker> {
        self.wait_cast(&format!("a {kind:?} marker"), cast_timeout(), |c| {
            c.markers_of(kind).into_iter().find(|m| pred(m))
        })
    }

    /// Types `line`, then waits for its `cmd` marker (typed exactly `line`) and the
    /// matching `done`. Returns `(cmd, done)`.
    pub fn run(&self, line: &str) -> Result<(Marker, Marker)> {
        self.run_timeout(line, cast_timeout())
    }

    pub fn run_timeout(&self, line: &str, timeout: Duration) -> Result<(Marker, Marker)> {
        let before = self
            .cast()
            .map(|c| {
                c.markers_of("cmd")
                    .iter()
                    .filter(|m| m.str("typed") == Some(line))
                    .count()
            })
            .unwrap_or(0);
        self.pane.type_line(line)?;
        self.wait_done(line, before, timeout)
    }

    /// Waits for the `before`-th (0-based) `cmd` typed `typed` and its `done`.
    pub fn wait_done(&self, typed: &str, before: usize, timeout: Duration) -> Result<(Marker, Marker)> {
        self.wait_cast(&format!("cmd+done for {typed:?}"), timeout, |c| {
            let cmd = c
                .markers_of("cmd")
                .into_iter()
                .filter(|m| m.str("typed") == Some(typed))
                .nth(before)?;
            let done = c.done_for(cmd.u64("seq")?)?;
            Some((cmd, done))
        })
    }

    /// Waits until the recorder wrote its exit event and is gone.
    pub fn wait_exit(&self, timeout: Duration) -> Result<String> {
        let x = self.wait_cast("the final x event", timeout, |c| c.exit_status().map(str::to_owned))?;
        if !procs::wait_gone(self.pane.pid, timeout) {
            bail!("recorder {} still running after its x event", self.pane.pid);
        }
        Ok(x)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn quiet() -> SandboxOptions {
        SandboxOptions {
            flavour: None,
            ..SandboxOptions::default()
        }
    }

    #[test]
    fn layout_config_and_env() {
        let sb = Sandbox::with(SandboxOptions {
            record: vec!["nested = \"record\"".into()],
            shell: vec![("banners".into(), "true".into())],
            ..quiet()
        })
        .unwrap();
        assert!(sb.home.join(".zshrc").is_file());
        let cfg = fs::read_to_string(sb.config_file()).unwrap();
        assert!(cfg.contains("nested = \"record\""));
        assert!(cfg.contains("banners = true"));
        assert!(cfg.contains("motd = false"));
        assert!(cfg.contains("autostart = \"off\""));
        assert!(cfg.contains(&*sb.logs.to_string_lossy()));
        let mode = fs::metadata(sb.config_file()).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let env = sb.env();
        assert!(env.iter().any(|(k, v)| k == MARKER_VAR && *v == sb.id));
        assert!(env.iter().all(|(k, _)| !k.starts_with("CINELOG_")));
        let argv = sb.shell_argv();
        assert_eq!(&argv[..2], ["env", "-i"]);
        assert_eq!(argv.last().unwrap(), "-i");
        assert!(sb.sock_dir.as_os_str().len() + SOCKET_SUFFIX_LEN < 108);
    }

    #[test]
    fn baseline_zshrc_differs_only_by_the_plugin() {
        let sb = Sandbox::with(quiet()).unwrap();
        let with = fs::read_to_string(sb.home.join(".zshrc")).unwrap();
        let without = fs::read_to_string(sb.baseline_zdotdir().unwrap().join(".zshrc")).unwrap();
        assert!(with.starts_with(&without));
        let extra = &with[without.len()..];
        assert!(
            extra.starts_with("source ") && extra.contains("cinelog.plugin.zsh"),
            "{extra:?}"
        );
        assert!(!without.contains("cinelog.plugin.zsh"));
    }

    /// Regression: a system-wide `cinelog` (e.g. left by a team install) must not be found
    /// by sandboxed shells, or "missing binary" scenarios record after all.
    #[test]
    fn foreign_cinelog_is_not_on_the_sandbox_path() {
        let sb = Sandbox::with(quiet()).unwrap();
        assert_eq!(sb.resolve("cinelog"), None, "PATH {}", sb.path);
        assert!(sb.resolve("sh").is_some());
    }

    #[test]
    fn a_path_dir_with_a_foreign_cinelog_is_shadowed() {
        let d = crate::util::scratch_dir().unwrap();
        let (sys, other) = (d.path().join("sys"), d.path().join("other"));
        for (dir, names) in [(&sys, &["cinelog", "zsh", "tmux"][..]), (&other, &["python3"][..])] {
            mkdir(dir, 0o755).unwrap();
            for name in names {
                fs::write(dir.join(name), "#!/bin/sh\n").unwrap();
                fs::set_permissions(dir.join(name), fs::Permissions::from_mode(0o755)).unwrap();
            }
        }
        let own = d.path().join("own");
        let path = isolated_path(&own, &[sys.clone(), other.clone()], &d.path().join("shadow")).unwrap();
        let entries: Vec<&str> = path.split(':').collect();
        assert_eq!(entries[0], own.to_str().unwrap());
        assert_eq!(
            entries[2],
            other.to_str().unwrap(),
            "dirs without cinelog are used as they are"
        );
        let shadow = Path::new(entries[1]);
        assert_ne!(shadow, sys);
        let mut names: Vec<_> = fs::read_dir(shadow)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        assert_eq!(names, ["tmux", "zsh"]);
        assert!(crate::util::is_executable(&shadow.join("zsh")));
        let found = entries
            .iter()
            .map(|e| Path::new(e).join("cinelog"))
            .find(|p| p.exists());
        assert_eq!(found, None);
    }

    #[test]
    fn drop_restores_permissions_and_removes_the_tree() {
        let sb = Sandbox::with(quiet()).unwrap();
        let root = sb.root.clone();
        let ro = sb.logs.join("ro");
        mkdir(&ro, 0o500).unwrap();
        fs::set_permissions(&sb.logs, fs::Permissions::from_mode(0o500)).unwrap();
        drop(sb);
        assert!(!root.exists());
    }

    #[test]
    fn tmpfs_wrapper_runs_as_our_uid_with_a_private_mount() {
        if crate::gate::check(&[crate::Need::UserNs]).is_err() {
            eprintln!("SKIP: user namespaces unavailable");
            return;
        }
        let sb = Sandbox::with(quiet()).unwrap();
        let mnt = sb.root.join("mnt");
        mkdir(&mnt, 0o700).unwrap();
        let probe = format!(
            "id -u > {out}/uid; df -k {mnt} | tail -1 > {out}/df; ls {mnt} > {out}/ls",
            out = sb.work.display(),
            mnt = mnt.display()
        );
        let argv = sb.tmpfs_argv(&mnt, "1m", 4096, vec!["sh".into(), "-c".into(), probe]);
        let st = std::process::Command::new(&argv[0]).args(&argv[1..]).status().unwrap();
        assert!(st.success());
        let uid = fs::read_to_string(sb.work.join("uid")).unwrap();
        assert_eq!(uid.trim(), nix::unistd::getuid().to_string());
        assert!(fs::read_to_string(sb.work.join("df")).unwrap().starts_with("tmpfs"));
        assert_eq!(fs::read_to_string(sb.work.join("ls")).unwrap().trim(), "filler");
        assert!(fs::read_dir(&mnt).unwrap().next().is_none(), "the mount is private");
    }

    /// Regression for I8: a cast on a namespace-private log tmpfs is invisible in the host
    /// view of the tree, so "no cast" checks must go through the pane's view.
    #[test]
    fn casts_on_a_private_tmpfs_are_seen_only_through_the_view() {
        if crate::gate::check(&[crate::Need::UserNs]).is_err() {
            eprintln!("SKIP: user namespaces unavailable");
            return;
        }
        let sb = Sandbox::with(SandboxOptions {
            logs: Some("logs-tmpfs".into()),
            ..quiet()
        })
        .unwrap();
        let ready = sb.work.join("ready");
        let inner = format!(
            "echo '{{}}' > {logs}/hidden.cast && : > {ready} && exec sleep 30",
            logs = sb.logs.display(),
            ready = ready.display()
        );
        let argv = sb.tmpfs_argv(&sb.logs, "1m", 0, vec!["sh".into(), "-c".into(), inner]);
        let mut child = std::process::Command::new(&argv[0]).args(&argv[1..]).spawn().unwrap();
        let seen = crate::wait_until("the cast on the tmpfs", Duration::from_secs(10), || {
            ready.exists().then_some(())
        })
        .map(|()| {
            sb.view_through(child.id());
            (find_casts(&sb.root), sb.cast_paths())
        });
        let _ = child.kill();
        let _ = child.wait();
        let (host, view) = seen.unwrap();
        assert!(host.is_empty(), "host view: {host:?}");
        assert_eq!(
            view.iter()
                .map(|p| p.file_name().unwrap().to_owned())
                .collect::<Vec<_>>(),
            ["hidden.cast"]
        );
    }
}
