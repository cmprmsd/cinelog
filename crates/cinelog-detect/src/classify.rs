//! Holder classification by comm and cmdline (§5.2 step 4), never by exe alone: terminator's
//! exe is python3, and a renamed process keeps its exe.

use std::path::Path;

use crate::{HolderClass, Reason};

/// comm of cinelog recorders (set with `PR_SET_NAME`; argv may be mirrored, comm is not).
pub const RECORDER_COMM: &str = "cinelog";

/// Strips the ` (deleted)` suffix the kernel appends to the exe link of a replaced binary.
pub fn strip_deleted(exe: &Path) -> &Path {
    exe.to_str()
        .and_then(|s| s.strip_suffix(" (deleted)"))
        .map_or(exe, Path::new)
}

/// First word of argv0 without its directory or a login-shell `-`: `sshd: alice@pts/0` → `sshd`.
fn argv0_name(cmdline: &[String]) -> &str {
    let Some(a0) = cmdline.first() else { return "" };
    let word = a0.split([' ', ':']).next().unwrap_or("");
    let base = word.rsplit('/').next().unwrap_or(word);
    base.strip_prefix('-').unwrap_or(base)
}

fn classify_name(name: &str) -> Option<(HolderClass, Reason)> {
    Some(match name {
        "sudo" | "sudo-rs" => (HolderClass::PrivRelay, Reason::Sudo),
        "su" => (HolderClass::PrivRelay, Reason::Su),
        "doas" => (HolderClass::PrivRelay, Reason::Doas),
        "screen" | "SCREEN" => (HolderClass::Multiplexer, Reason::Screen),
        "zellij" => (HolderClass::Multiplexer, Reason::Zellij),
        "mosh-server" => (HolderClass::Remote, Reason::MoshServer),
        "dropbear" => (HolderClass::Remote, Reason::Sshd),
        "script" => (HolderClass::Other, Reason::Script),
        // A cinelog that failed the own-recorder checks is not trusted as one.
        RECORDER_COMM => (HolderClass::Other, Reason::Unknown),
        n if n.starts_with("tmux") => (HolderClass::Multiplexer, Reason::TmuxServer),
        n if n.starts_with("sshd") => (HolderClass::Remote, Reason::Sshd),
        _ => return None,
    })
}

/// Class and start reason of a holder that is not a verified own recorder.
pub fn classify_holder(comm: &str, cmdline: &[String]) -> (HolderClass, Reason) {
    classify_name(comm)
        .or_else(|| classify_name(argv0_name(cmdline)))
        .unwrap_or((HolderClass::Other, Reason::Terminal))
}

/// comms considered by the comm fallback when no holder is visible (§5.2 step 3).
pub fn is_fallback_comm(comm: &str) -> bool {
    matches!(
        comm,
        RECORDER_COMM
            | "sudo"
            | "sudo-rs"
            | "su"
            | "doas"
            | "tmux: server"
            | "screen"
            | "SCREEN"
            | "zellij"
            | "mosh-server"
    ) || comm.starts_with("sshd")
}

/// Holders whose children are daemonized panes (`tmux: server`, screen, zellij).
pub fn is_multiplexer_server(comm: &str) -> bool {
    matches!(comm, "tmux: server" | "screen" | "SCREEN" | "zellij")
}

/// comms that name the login path of an unattributed root session (§5.6 rule 3).
pub fn login_via_comm(comm: &str) -> bool {
    comm.starts_with("sshd") || matches!(comm, "login" | "systemd")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn holders_by_comm() {
        let c = |comm: &str| classify_holder(comm, &[]);
        assert_eq!(c("tmux: server"), (HolderClass::Multiplexer, Reason::TmuxServer));
        assert_eq!(c("sudo"), (HolderClass::PrivRelay, Reason::Sudo));
        assert_eq!(c("su"), (HolderClass::PrivRelay, Reason::Su));
        assert_eq!(c("doas"), (HolderClass::PrivRelay, Reason::Doas));
        assert_eq!(c("sshd-session"), (HolderClass::Remote, Reason::Sshd));
        assert_eq!(c("sshd"), (HolderClass::Remote, Reason::Sshd));
        assert_eq!(c("mosh-server"), (HolderClass::Remote, Reason::MoshServer));
        assert_eq!(c("screen"), (HolderClass::Multiplexer, Reason::Screen));
        assert_eq!(c("zellij"), (HolderClass::Multiplexer, Reason::Zellij));
        assert_eq!(c("script"), (HolderClass::Other, Reason::Script));
        assert_eq!(c("terminator"), (HolderClass::Other, Reason::Terminal));
        assert_eq!(c("cinelog"), (HolderClass::Other, Reason::Unknown));
        assert_eq!(c("subdued"), (HolderClass::Other, Reason::Terminal));
    }

    #[test]
    fn holders_by_cmdline() {
        assert_eq!(
            classify_holder("kworker", &args(&["sshd: alice@pts/0"])),
            (HolderClass::Remote, Reason::Sshd)
        );
        assert_eq!(
            classify_holder("x", &args(&["/usr/bin/script", "-q"])),
            (HolderClass::Other, Reason::Script)
        );
        assert_eq!(
            classify_holder("terminator", &args(&["/usr/bin/python3", "/usr/bin/terminator"])),
            (HolderClass::Other, Reason::Terminal)
        );
        assert_eq!(argv0_name(&args(&["-zsh"])), "zsh");
        assert_eq!(argv0_name(&[]), "");
    }

    #[test]
    fn deleted_suffix() {
        assert_eq!(
            strip_deleted(Path::new("/usr/local/bin/cinelog (deleted)")),
            Path::new("/usr/local/bin/cinelog")
        );
        assert_eq!(strip_deleted(Path::new("/usr/bin/zsh")), Path::new("/usr/bin/zsh"));
    }

    #[test]
    fn fallback_and_login_comms() {
        for c in [
            "cinelog",
            "sudo",
            "su",
            "doas",
            "sshd-session",
            "tmux: server",
            "screen",
            "zellij",
            "mosh-server",
        ] {
            assert!(is_fallback_comm(c), "{c}");
        }
        assert!(!is_fallback_comm("zsh"));
        assert!(!is_fallback_comm("tmux: client"));
        assert!(login_via_comm("sshd-session") && login_via_comm("login") && login_via_comm("systemd"));
        assert!(!login_via_comm("zsh"));
        assert!(is_multiplexer_server("tmux: server") && is_multiplexer_server("SCREEN"));
        assert!(!is_multiplexer_server("zsh") && !is_multiplexer_server("tmux: client"));
    }
}
