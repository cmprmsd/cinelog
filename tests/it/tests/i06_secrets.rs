//! I6: secrets typed at password prompts appear in no cast (keystroke policy §5.5):
//! `read -s` and getpass (rule a: `!ECHO && ICANON`), a raw-mode prompt as seen through a
//! relay and a real `ssh -tt` to localhost (rule b: prompt heuristic), and, root-gated, a
//! real sudo password prompt.

use std::fs;
use std::path::Path;

use cinelog_it::secret::{files_below, leaked, random_secret};
use cinelog_it::sshd::Sshd;
use cinelog_it::util::sh_quote;
use cinelog_it::{Cast, Need, Pane, Recorded, Sandbox, need, scaled};

fn prompt_lines(pane: &Pane) -> usize {
    let screen = pane.capture().unwrap_or_default();
    screen.lines().filter(|l| l.trim_end().ends_with("assword:")).count()
}

/// Starts `line`, waits for a *new* password prompt, types `secret`, and waits for
/// `got-<len>`. (Typing before the prompt is type-ahead, a documented gap of §5.5.)
fn answer_prompt(r: &Recorded, line: &str, secret: &str) {
    let seen = prompt_lines(&r.pane);
    r.pane.type_line(line).unwrap();
    cinelog_it::wait_until("a new password prompt", scaled(15.0), || {
        (prompt_lines(&r.pane) > seen).then_some(())
    })
    .unwrap_or_else(|e| panic!("{e:#}\n{}", r.pane.capture().unwrap_or_default()));
    r.pane.send_text(secret).unwrap();
    r.pane.send_keys(&["Enter"]).unwrap();
    let got = format!("got-{}", secret.len());
    r.pane.wait_text(&got, scaled(10.0)).unwrap();
    r.wait_done(line, 0, scaled(10.0)).unwrap();
}

/// Fails if any 4-character fragment of `secret` is in the input or output stream of a
/// cast (each concatenated over all its events, so reads split anywhere are covered), or
/// in any file of the sandbox. Generated secrets cannot occur there by chance.
fn assert_secret_nowhere(sb: &Sandbox, secret: &str) {
    let casts = sb.casts();
    assert!(!casts.is_empty(), "the shell was recorded");
    for c in &casts {
        for (stream, text) in [("input", c.input()), ("output", c.output())] {
            let hits = leaked(secret, 4, &text, "");
            assert!(hits.is_empty(), "{stream} of {} leaks {hits:?}", c.path.display());
        }
    }
    for (path, bytes) in files_below(&sb.root) {
        let hits = leaked(secret, 4, &String::from_utf8_lossy(&bytes), "");
        assert!(hits.is_empty(), "{} leaks {hits:?}", path.display());
    }
}

/// Not vacuous: ordinary keystrokes of the same shell are recorded.
fn assert_input_recorded(r: &Recorded) {
    let c = r.cast().unwrap();
    assert!(
        c.input().contains("echo ordinary"),
        "input recording is on: {:?}",
        c.input()
    );
}

/// Raw-mode prompt that does not echo, like the local side of `ssh -t` to a remote prompt:
/// the local termios is `!ECHO && !ICANON`, so only the prompt heuristic can protect it.
const RAW_PROMPT: &str = r#"
import os, sys, termios, tty
fd = sys.stdin.fileno()
old = termios.tcgetattr(fd)
tty.setraw(fd)
buf = b""
try:
    os.write(1, b"Password: ")
    while True:
        c = os.read(fd, 1)
        if c in (b"\r", b"\n", b""):
            break
        buf += c
finally:
    termios.tcsetattr(fd, termios.TCSADRAIN, old)
print("\ngot-%d" % len(buf))
"#;

#[test]
fn i6_local_password_prompts_are_not_recorded() {
    need!(Need::Zsh, Need::Tmux, Need::Python, Need::Plugin, Need::Recorder);
    let sb = Sandbox::new().unwrap();
    let t = sb.tmux().unwrap();
    let r = sb.start_recorded(&t, "main").unwrap();
    r.run("echo ordinary keystrokes").unwrap();
    let raw = sb.work.join("raw_prompt.py");
    fs::write(&raw, RAW_PROMPT).unwrap();
    let secrets: Vec<String> = (0..3).map(|_| random_secret()).collect();
    answer_prompt(&r, "read -rs 'pw?Password: '; print; print got-${#pw}", &secrets[0]);
    answer_prompt(
        &r,
        "python3 -c 'import getpass; print(\"got-%d\" % len(getpass.getpass()))'",
        &secrets[1],
    );
    answer_prompt(
        &r,
        &format!("python3 {}", sh_quote(&raw.to_string_lossy())),
        &secrets[2],
    );
    r.run("echo after the prompts").unwrap();
    assert_input_recorded(&r);
    for s in &secrets {
        assert_secret_nowhere(&sb, s);
    }
}

#[test]
fn i6_remote_prompt_over_ssh_is_not_recorded() {
    need!(Need::Zsh, Need::Tmux, Need::Plugin, Need::Recorder, Need::Sshd);
    let sb = Sandbox::new().unwrap();
    let sshd = Sshd::start(&sb.root.join("sshd"), &sb.env()).unwrap();
    let remote = sb.work.join("remote_prompt.sh");
    fs::write(&remote, "read -rsp 'Password: ' x; echo; echo got-${#x}\n").unwrap();
    let t = sb.tmux().unwrap();
    let r = sb.start_recorded(&t, "main").unwrap();
    r.run("echo ordinary keystrokes").unwrap();
    let s = random_secret();
    let line = sshd.ssh_command(sb.user(), &format!("bash {}", sh_quote(&remote.to_string_lossy())));
    answer_prompt(&r, &line, &s);
    assert_input_recorded(&r);
    assert_secret_nowhere(&sb, &s);
}

/// A login token printed once (`cinelog token new`; `install` prints its token the same
/// way) reaches the terminal but not the cast: the recording pauses around exactly that
/// line, behind a visible pause marker, and records again right after it. Before there is
/// a token, the MOTD says how to get one.
#[test]
fn i6_printed_login_token_is_not_recorded() {
    need!(Need::Zsh, Need::Tmux, Need::Plugin, Need::Recorder);
    let sb = Sandbox::new().unwrap();
    let t = sb.tmux().unwrap();
    let r = sb.start_recorded(&t, "main").unwrap();
    // A new user learns from the MOTD how to get a token (PROTOCOL.md §6.6 `web`).
    let hint = "cinelog: web UI not set up yet - run: cinelog install (prints your login token)";
    let motd = r.pane.capture().unwrap_or_default();
    assert!(motd.contains(hint), "{motd}");
    r.run("echo ordinary keystrokes").unwrap();
    r.run("cinelog token new").unwrap();
    r.run("echo after-the-token").unwrap();
    let next = sb.start_recorded(&t, "main").unwrap();
    let motd = next.pane.capture().unwrap_or_default();
    assert!(!motd.contains(hint) && motd.contains("cinelog: session "), "{motd}");
    let screen = r.pane.capture().unwrap_or_default();
    let token = screen
        .lines()
        .find_map(|l| l.split("(shown once): ").nth(1))
        .map(str::trim)
        .unwrap_or_else(|| panic!("no token on screen:\n{screen}"))
        .to_owned();
    assert!(token.len() >= 32, "{token:?}");
    let c = r.cast().unwrap();
    let out = c.output();
    let hits = leaked(&token, 8, &out, "");
    assert!(hits.is_empty(), "output of {} leaks {hits:?}", c.path.display());
    assert!(out.contains("after-the-token"), "recording resumed: {out:?}");
    let pauses = c.markers_of("pause");
    assert!(
        pauses
            .iter()
            .any(|m| m.str("reason") == Some("user") && m.str("detail") == Some("secret output")),
        "a visible pause marks the gap: {:?}",
        c.markers().iter().map(|m| m.json.to_string()).collect::<Vec<_>>()
    );
    assert!(
        c.pause_intervals().iter().all(|(_, end)| end.is_some()),
        "no pause stays open"
    );
}

/// Root-gated: a real sudo prompt of a password-requiring user (`CINELOG_IT_PW_USER`,
/// `CINELOG_IT_PW_PASS`, provisioned by CI) in that user's own recorded shell.
///
/// The password is provisioned, not generated, so it may share short fragments with text
/// that is legitimately recorded (the prompt, hex ids). Fragments of 4 characters are
/// therefore checked in the input stream, minus what the test typed and the screen showed;
/// the output stream and the raw files are checked for 8-character fragments. The password
/// never appears on any command line (sudo logs those): files are read out as that user and
/// searched in-process.
#[test]
fn i6_real_sudo_prompt_is_not_recorded() {
    need!(
        Need::Zsh,
        Need::Tmux,
        Need::Plugin,
        Need::Recorder,
        Need::Root,
        Need::PwUser
    );
    let user = std::env::var("CINELOG_IT_PW_USER").unwrap();
    let pass = std::env::var("CINELOG_IT_PW_PASS").unwrap();
    let sb = Sandbox::new().unwrap();
    let pw = PwUserHome::create(&sb, &user).unwrap();
    let t = sb.tmux().unwrap();
    let pane = t.new_session("pw", 120, 40, &pw.argv).unwrap();
    pane.wait_prompt(Pane::startup_timeout()).unwrap();
    let mut typed = vec!["echo ordinary keystrokes".to_owned()];
    pane.run_sync(&typed[0], scaled(10.0)).unwrap();
    let sudo_prompts = |p: &Pane| {
        let screen = p.capture().unwrap_or_default();
        screen.lines().filter(|l| l.contains("[sudo] password for")).count()
    };
    // `&&`: the token only appears if sudo accepted the password.
    for (line, token) in [
        ("sudo true && print got-$((6*7))", "got-42"),
        ("sudo -k; sudo true && print got-$((6*8))", "got-48"),
    ] {
        let seen = sudo_prompts(&pane);
        pane.type_line(line).unwrap();
        typed.push(line.to_owned());
        cinelog_it::wait_until("a new sudo prompt", scaled(15.0), || {
            (sudo_prompts(&pane) > seen).then_some(())
        })
        .unwrap();
        pane.send_text(&pass).unwrap();
        pane.send_keys(&["Enter"]).unwrap();
        pane.wait_text(token, scaled(15.0))
            .unwrap_or_else(|e| panic!("sudo did not accept the password: {e:#}"));
        pane.sync(scaled(10.0)).unwrap();
    }
    let screen = pane.capture().unwrap_or_default();
    pane.type_line("exit").unwrap();

    let home = Path::new(&pw.dir);
    let read = |p: &Path| cinelog_it::sudo::read_as(Some(&user), p).unwrap();
    let casts: Vec<Cast> = cinelog_it::wait_until("the password user's finished cast", scaled(15.0), || {
        let paths = cinelog_it::sudo::files_as(Some(&user), &home.join("logs"), Some("cast")).ok()?;
        let casts: Vec<Cast> = paths.iter().filter_map(|p| Cast::parse(p, &read(p)).ok()).collect();
        casts.iter().any(|c| c.exit_status().is_some()).then_some(casts)
    })
    .unwrap();
    assert!(
        casts.iter().any(|c| c.input().contains("echo ordinary")),
        "the password user's input is recorded"
    );
    let known = format!("{}\n{screen}", typed.join("\n"));
    for c in &casts {
        let hits = leaked(&pass, 4, &c.input(), &known);
        assert!(
            hits.is_empty(),
            "input of {} leaks {} password fragment(s)",
            c.path.display(),
            hits.len()
        );
        let hits = leaked(&pass, 8, &c.output(), &known);
        assert!(
            hits.is_empty(),
            "output of {} leaks {} password fragment(s)",
            c.path.display(),
            hits.len()
        );
    }
    for sub in ["home", "logs", "run"] {
        for p in cinelog_it::sudo::files_as(Some(&user), &home.join(sub), None).unwrap() {
            let hits = leaked(&pass, 8, &String::from_utf8_lossy(&read(&p)), &known);
            assert!(
                hits.is_empty(),
                "{} leaks {} password fragment(s)",
                p.display(),
                hits.len()
            );
        }
    }
}

/// A sandbox home owned by another user, created and removed through sudo.
struct PwUserHome {
    dir: String,
    argv: Vec<String>,
}

impl PwUserHome {
    fn create(sb: &Sandbox, user: &str) -> anyhow::Result<Self> {
        use cinelog_it::sudo::sudo;
        let dir = format!("/tmp/clit-pw-{}", sb.id);
        let src = format!("/tmp/clit-pwsrc-{}", sb.id);
        // World-readable staging copy of the binary and plugin for the other user.
        fs::create_dir_all(&src)?;
        fs::copy(sb.bin.as_ref().expect("binary"), format!("{src}/cinelog"))?;
        fs::copy(
            sb.plugin_dir.join("cinelog.plugin.zsh"),
            format!("{src}/cinelog.plugin.zsh"),
        )?;
        if sb.plugin_dir.join("shell").is_dir() {
            std::process::Command::new("cp")
                .args(["-r", &sb.plugin_dir.join("shell").to_string_lossy(), &src])
                .status()?;
        }
        std::process::Command::new("chmod")
            .args(["-R", "a+rX", &src])
            .status()?;
        let script = format!(
            "set -e; umask 077; d={d}; mkdir -p $d/home/.config/cinelog $d/home/.local/state $d/run $d/logs $d/plugin/bin; \
             cp -r {src}/. $d/plugin/; mv $d/plugin/cinelog $d/plugin/bin/cinelog; chmod 755 $d/plugin/bin/cinelog; \
             printf '%s\\n' \"PROMPT='clit%# '\" 'source {d}/plugin/cinelog.plugin.zsh' > $d/home/.zshrc; \
             printf '[paths]\\nlogs = \"{d}/logs\"\\n[shell]\\nmotd = false\\nbanners = false\\n[server]\\nautostart = \"off\"\\n' > $d/home/.config/cinelog/config.toml",
            d = dir,
            src = src
        );
        sudo(&["-u", user, "sh", "-c", &script])?;
        let env = [
            format!("HOME={dir}/home"),
            format!("ZDOTDIR={dir}/home"),
            format!("USER={user}"),
            format!("LOGNAME={user}"),
            format!("SHELL={}", sb.zsh.display()),
            "PATH=/usr/local/bin:/usr/bin:/bin".to_owned(),
            "TERM=xterm-256color".to_owned(),
            "LANG=C.UTF-8".to_owned(),
            format!("XDG_CONFIG_HOME={dir}/home/.config"),
            format!("XDG_STATE_HOME={dir}/home/.local/state"),
            format!("XDG_RUNTIME_DIR={dir}/run"),
            format!("{}={}", cinelog_it::sandbox::MARKER_VAR, sb.id),
        ];
        let mut argv: Vec<String> = ["sudo", "-n", "-u", user, "env", "-i"].map(String::from).to_vec();
        argv.extend(env);
        argv.extend([sb.zsh.to_string_lossy().into_owned(), "-i".to_owned()]);
        Ok(Self { dir, argv })
    }
}

impl Drop for PwUserHome {
    fn drop(&mut self) {
        let _ = cinelog_it::sudo::sudo(&["rm", "-rf", &self.dir]);
        let src = self.dir.replace("clit-pw-", "clit-pwsrc-");
        let _ = fs::remove_dir_all(src);
    }
}
