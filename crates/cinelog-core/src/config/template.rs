//! The commented `config.toml` / `team.toml` that `cinelog install` writes (§10.5).
//!
//! Every key line has the form `# key = <default>`; explanations start with `##`. The
//! template parses to [`Config::default`](super::Config::default) both as written and with
//! every key line uncommented (both are unit-tested), so the comments cannot drift from the
//! defaults. [`render`] uncomments selected keys with new values (settings migration).

use cinelog_proto::Scope;

const PERSONAL_HEAD: &str = "\
## cinelog configuration (PERSONAL scope): $XDG_CONFIG_HOME/cinelog/config.toml
## Every key is optional; the commented values are the defaults.
## A broken file makes new shells start unrecorded (with one warning) until it is fixed;
## check it with `cinelog doctor`.

[paths]
# logs = \"~/logs\"
## Empty: $XDG_STATE_HOME/cinelog. DBs go to <state>/<host>/ and must not be in a synced
## (Syncthing) or network folder.
# state = \"\"
";

const TEAM_HEAD: &str = "\
## cinelog configuration (TEAM scope, root shells): /etc/cinelog/team.toml
## Every key is optional; the commented values are the defaults. Root ignores HOME, XDG_*
## and CINELOG_* entirely; the only kill switch is /etc/cinelog/disable.

[paths]
## Pinned: casts go to <logs>/<sudo_user>/YYYY-MM/. Never derived from $HOME.
# logs = \"/root/logs\"
";

const BODY: &str = "
[record]
# input = true
## Keystrokes are not recorded while one of these applies. Add \"relay\" to drop all input
## while sudo/ssh/… run in the foreground (loses every ssh keystroke).
# input_suppress = [\"password-prompt\", \"prompt-heuristic\", \"paused\", \"terminal-responses\"]
# relays = [\"sudo\", \"su\", \"doas\", \"ssh\", \"mosh-client\", \"docker\", \"podman\", \"kubectl\", \"tmux: client\"]
# strip_markers = true
## What the outer recorder does while a nested (sudo) recording runs:
##   pause  – write a link marker and pause
##   record – keep recording. WARNING: this copies root/team session content into your
##            user-readable personal cast.
##   ignore – keep recording, no marker
# nested = \"pause\"
# pause_on_multiplexer = [\"tmux: client\", \"screen\", \"zellij\"]
## start | skip, when the PTY holder is invisible (hidepid=2).
# hidepid_fallback = \"start\"
# inner_login = false
# mirror_cmdline = true
# rotate_mb = 512
# rotate_days = 7
# fsync_secs = 5

[shell]
## Passed to the recorded shell as CINELOG_OPTS.
# motd = true
# banners = true
# check_free_space = true
# external_ip = false
# title = true
# title_link = false

[server]
## auto (systemd unit, else autostart from the recorder) | systemd | shell | off
# autostart = \"auto\"
## Port `auto`: 10000 (team) or 10000 + uid (personal).
# bind = [\"0.0.0.0:auto\", \"[::]:auto\"]
## Extra names the server answers to. Recommended for the team server: a hostname of its
## own (e.g. \"cinelog-team.lan\"), so its cookie never reaches personal servers.
# hostnames = []
# tls_cert = \"\"
# tls_key = \"\"
# tls_auto_regen = true
# session_idle_hours = 12
# session_max_days = 7

[index]
# output = true
# gap_output = true
# chunk_kb = 16
# max_output_per_command_mb = 64
# alt_snapshots = true
# playback_segment_threshold_mb = 8
## Optional regexes masked at index time; off by default.
# redact = []

[retention]
# compress_after_days = 14
# zstd_level = 9
";

/// The template for `scope` with all keys at their defaults.
pub fn template(scope: Scope) -> String {
    let head = match scope {
        Scope::Personal => PERSONAL_HEAD,
        Scope::Team => TEAM_HEAD,
    };
    format!("{head}{BODY}")
}

/// One key to set: (`section`, `key`, value).
pub type Override<'a> = (&'a str, &'a str, toml::Value);

/// The template with the given keys uncommented and set. Keys that are not in the
/// template are appended to their section, so nothing is silently dropped.
pub fn render(scope: Scope, overrides: &[Override<'_>]) -> String {
    let mut pending: Vec<&Override<'_>> = overrides.iter().collect();
    let mut out = String::new();
    let mut section = "";
    let flush = |out: &mut String, section: &str, pending: &mut Vec<&Override<'_>>| {
        pending.retain(|(s, k, v)| {
            if *s == section {
                out.push_str(&format!("{k} = {v}\n"));
                false
            } else {
                true
            }
        });
    };
    let text = template(scope);
    for line in text.lines() {
        if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            flush(&mut out, section, &mut pending);
            section = name;
        } else if let Some((key, _)) = key_line(line)
            && let Some(i) = pending.iter().position(|(s, k, _)| *s == section && *k == key)
        {
            let (_, k, v) = pending.remove(i);
            out.push_str(&format!("{k} = {v}\n"));
            continue;
        }
        out.push_str(line);
        out.push('\n');
    }
    flush(&mut out, section, &mut pending);
    for (s, k, v) in pending {
        out.push_str(&format!("\n[{s}]\n{k} = {v}\n"));
    }
    out
}

/// `# key = value` → (`key`, `value`).
fn key_line(line: &str) -> Option<(&str, &str)> {
    let (key, value) = line.strip_prefix("# ")?.split_once(" = ")?;
    key.bytes()
        .all(|b| b.is_ascii_lowercase() || b == b'_')
        .then_some((key, value))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    fn uncomment_all(text: &str) -> String {
        text.lines()
            .map(|l| if key_line(l).is_some() { &l[2..] } else { l })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn personal_template_documents_the_defaults() {
        let t = template(Scope::Personal);
        assert_eq!(Config::from_toml_str(&t).unwrap(), Config::default());
        assert_eq!(Config::from_toml_str(&uncomment_all(&t)).unwrap(), Config::default());
        assert!(t.contains("WARNING: this copies root/team session content"));
    }

    #[test]
    fn team_template_pins_root_logs() {
        let t = template(Scope::Team);
        let c = Config::from_toml_str(&uncomment_all(&t)).unwrap();
        assert_eq!(c.paths.logs, "/root/logs");
        let mut expected = Config::default();
        expected.paths.logs = "/root/logs".into();
        assert_eq!(c, expected);
        assert_eq!(Config::from_toml_str(&t).unwrap(), Config::default());
    }

    #[test]
    fn render_sets_keys_in_place_and_appends_unknown_ones() {
        let t = render(
            Scope::Personal,
            &[
                ("shell", "motd", toml::Value::Boolean(false)),
                ("shell", "external_ip", toml::Value::Boolean(true)),
                ("shell", "future_key", toml::Value::Integer(3)),
            ],
        );
        let c = Config::from_toml_str(&t).unwrap();
        assert!(!c.shell.motd && c.shell.external_ip && c.shell.banners);
        assert!(t.contains("[shell]\n## Passed to the recorded shell as CINELOG_OPTS.\nmotd = false\n"));
        let v: toml::Table = t.parse().unwrap();
        assert_eq!(v["shell"]["future_key"].as_integer(), Some(3));
        assert_eq!(t.matches("[shell]").count(), 1);
    }
}
