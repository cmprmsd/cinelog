//! A private, unprivileged OpenSSH server on 127.0.0.1 for the "remote password prompt"
//! case of I6. It accepts only a key generated for the sandbox, runs commands through the
//! user's login shell with `ZDOTDIR` pointing at an empty sandbox dir, never reads the real
//! `~/.ssh`, and is killed on drop.

use std::fs;
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use anyhow::{Context, Result, bail};

use crate::util::{wait_until, which};

pub struct Sshd {
    child: Child,
    pub port: u16,
    pub dir: PathBuf,
    pub client_key: PathBuf,
}

fn sshd_path() -> Option<PathBuf> {
    which("sshd").or_else(|| {
        ["/usr/sbin/sshd", "/usr/bin/sshd"]
            .iter()
            .map(PathBuf::from)
            .find(|p| p.is_file())
    })
}

fn keygen(path: &Path) -> Result<()> {
    let st = Command::new("ssh-keygen")
        .args(["-q", "-t", "ed25519", "-N", "", "-f"])
        .arg(path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .status()?;
    if !st.success() {
        bail!("ssh-keygen failed");
    }
    Ok(())
}

impl Sshd {
    /// Starts sshd in `<dir>` with the sandbox environment marker so cleanup can find it.
    pub fn start(dir: &Path, env: &[(String, String)]) -> Result<Self> {
        let sshd = sshd_path().context("sshd not found")?;
        fs::create_dir_all(dir)?;
        let zdot = dir.join("zdotdir");
        fs::create_dir_all(&zdot)?;
        fs::write(zdot.join(".zshenv"), "")?;
        let (host_key, client_key) = (dir.join("host_key"), dir.join("client_key"));
        keygen(&host_key)?;
        keygen(&client_key)?;
        fs::copy(dir.join("client_key.pub"), dir.join("authorized_keys"))?;
        let mut last_err = None;
        for _ in 0..3 {
            let port = TcpListener::bind("127.0.0.1:0")?.local_addr()?.port();
            let config = dir.join("sshd_config");
            fs::write(
                &config,
                format!(
                    "ListenAddress 127.0.0.1:{port}\nHostKey {hk}\nAuthorizedKeysFile {ak}\nPidFile {pid}\n\
                     StrictModes no\nUsePAM no\nPasswordAuthentication no\nKbdInteractiveAuthentication no\n\
                     PubkeyAuthentication yes\nPermitUserRC no\nPermitUserEnvironment no\nPrintMotd no\n\
                     PrintLastLog no\nSetEnv ZDOTDIR={zdot}\n",
                    hk = host_key.display(),
                    ak = dir.join("authorized_keys").display(),
                    pid = dir.join("sshd.pid").display(),
                    zdot = zdot.display(),
                ),
            )?;
            let child = Command::new(&sshd)
                .arg("-f")
                .arg(&config)
                .args(["-D", "-e"])
                .env_clear()
                .envs(env.iter().map(|(k, v)| (k, v)))
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(fs::File::create(dir.join("sshd.log"))?)
                .spawn()?;
            let mut s = Self {
                child,
                port,
                dir: dir.to_path_buf(),
                client_key: client_key.clone(),
            };
            let up = wait_until("sshd to listen", Duration::from_secs(5), || {
                if s.child.try_wait().ok().flatten().is_some() {
                    return Some(false);
                }
                TcpStream::connect(("127.0.0.1", port)).is_ok().then_some(true)
            });
            match up {
                Ok(true) => return Ok(s),
                _ => last_err = Some(fs::read_to_string(dir.join("sshd.log")).unwrap_or_default()),
            }
        }
        bail!("sshd did not start: {}", last_err.unwrap_or_default())
    }

    /// `ssh -tt …` command line (as typed into a sandbox shell) running `remote` on localhost.
    pub fn ssh_command(&self, user: &str, remote: &str) -> String {
        format!(
            "ssh -tt -F /dev/null -p {port} -i {key} -o IdentitiesOnly=yes -o BatchMode=yes \
             -o StrictHostKeyChecking=no -o UserKnownHostsFile={kh} -o LogLevel=ERROR {user}@127.0.0.1 {remote}",
            port = self.port,
            key = self.client_key.display(),
            kh = self.dir.join("known_hosts").display(),
        )
    }
}

impl Drop for Sshd {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Whether a private sshd can run here (used by the `Sshd` prerequisite).
pub fn probe() -> Result<(), String> {
    if sshd_path().is_none() || which("ssh").is_none() || which("ssh-keygen").is_none() {
        return Err("OpenSSH server/client not installed".into());
    }
    let dir = crate::util::scratch_dir().map_err(|e| format!("{e:#}"))?;
    let sshd = Sshd::start(dir.path(), &[("PATH".into(), "/usr/bin:/bin".into())]).map_err(|e| format!("{e:#}"))?;
    let user = nix::unistd::User::from_uid(nix::unistd::getuid())
        .ok()
        .flatten()
        .map(|u| u.name)
        .ok_or("no passwd entry")?;
    let out = Command::new("ssh")
        .args(["-F", "/dev/null", "-p", &sshd.port.to_string(), "-i"])
        .arg(&sshd.client_key)
        .args([
            "-o",
            "IdentitiesOnly=yes",
            "-o",
            "BatchMode=yes",
            "-o",
            "StrictHostKeyChecking=no",
        ])
        .arg("-o")
        .arg(format!(
            "UserKnownHostsFile={}",
            dir.path().join("known_hosts").display()
        ))
        .arg(format!("{user}@127.0.0.1"))
        .arg("echo clit-ssh-ok")
        .env_clear()
        .env("HOME", dir.path())
        .env("PATH", "/usr/bin:/bin")
        .stdin(Stdio::null())
        .output()
        .map_err(|e| e.to_string())?;
    if String::from_utf8_lossy(&out.stdout).contains("clit-ssh-ok") {
        Ok(())
    } else {
        Err(format!(
            "ssh to the private sshd failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ))
    }
}
