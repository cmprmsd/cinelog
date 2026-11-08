//! Runs every `expect` line of `tests/fixtures/proc/*.txt` against a [`ProcDetector`] over
//! the materialized tree. See `cinelog_detect::fixture` for the tree syntax; expectations:
//!
//! ```text
//! expect check tty=pts:N|vt:PATH|none start=P euid=U [fallback=start|skip] => <rec --check line>
//! expect holder [euid=U] pts=N start=P => none | [pid=] [via=] [class=] [uid=] [comm=]
//! expect principal euid=U start=P [client=P] [claimed=S] => [user=] [sudo_user=] [attribution=] [login_via=] [claimed=]
//! expect foreground fg=G shell=S [mux=a|b] => shell | job leader=C | multiplexer pid=P [comm=C] | recorder pid=P start=T
//! expect nested fg=G shell=S pid=P start=T => true|false
//! expect relay fg=G [relays=a|b] => true|false
//! expect valid pts=N => true|false
//! ```
//!
//! `-` stands for `None` on the right-hand side.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use cinelog_core::config::{HidepidFallback, RecordConfig};
use cinelog_detect::fixture::Fixture;
use cinelog_detect::{
    CheckRequest, Detector, Foreground, FoundVia, Holder, HolderClass, NestedClaim, PrincipalRequest, StartDecision,
    TtyIdentity,
};
use cinelog_proto::Attribution;

fn fixture_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/proc")
}

/// `k=v` tokens (values may be double-quoted) plus bare words.
fn kv(s: &str) -> (Vec<String>, HashMap<String, String>) {
    let mut words = Vec::new();
    let mut map = HashMap::new();
    let mut toks = Vec::new();
    let (mut cur, mut quoted, mut any) = (String::new(), false, false);
    for c in s.chars() {
        match c {
            '"' => {
                quoted = !quoted;
                any = true;
            }
            c if c.is_whitespace() && !quoted => {
                if any {
                    toks.push(std::mem::take(&mut cur));
                    any = false;
                }
            }
            c => {
                cur.push(c);
                any = true;
            }
        }
    }
    if any {
        toks.push(cur);
    }
    for t in toks {
        match t.split_once('=') {
            Some((k, v)) => {
                map.insert(k.to_owned(), v.to_owned());
            }
            None => words.push(t),
        }
    }
    (words, map)
}

fn num<T: std::str::FromStr>(m: &HashMap<String, String>, k: &str) -> T {
    m.get(k)
        .unwrap_or_else(|| panic!("missing {k}"))
        .parse()
        .unwrap_or_else(|_| panic!("{k} is not a number"))
}

fn opt(v: &str) -> Option<String> {
    (v != "-").then(|| v.to_owned())
}

fn list(m: &HashMap<String, String>, k: &str, default: &[String]) -> Vec<String> {
    m.get(k)
        .map_or_else(|| default.to_vec(), |v| v.split('|').map(str::to_owned).collect())
}

fn class_name(c: &HolderClass) -> &'static str {
    match c {
        HolderClass::OwnRecorder { same_uid: true } => "own-same",
        HolderClass::OwnRecorder { same_uid: false } => "own-other",
        HolderClass::PrivRelay => "privrelay",
        HolderClass::Multiplexer => "multiplexer",
        HolderClass::Remote => "remote",
        HolderClass::Other => "other",
    }
}

fn via_name(v: FoundVia) -> &'static str {
    match v {
        FoundVia::Ancestor => "ancestor",
        FoundVia::Scan => "scan",
        FoundVia::CommFallback => "commfallback",
    }
}

fn attribution_name(a: Attribution) -> &'static str {
    match a {
        Attribution::Ancestor => "ancestor",
        Attribution::Loginuid => "loginuid",
        Attribution::Inherited => "inherited",
        Attribution::None => "none",
        Attribution::Legacy => "legacy",
    }
}

fn check_holder(got: Option<Holder>, want: &str) -> Result<(), String> {
    let (words, m) = kv(want);
    match got {
        None if words == ["none"] => Ok(()),
        None => Err("no holder".into()),
        Some(h) if words == ["none"] => Err(format!("unexpected holder {h:?}")),
        Some(h) => {
            let actual = HashMap::from([
                ("pid", h.pid.to_string()),
                ("uid", h.uid.to_string()),
                ("comm", h.comm.clone()),
                ("class", class_name(&h.class).to_owned()),
                ("via", via_name(h.via).to_owned()),
            ]);
            for (k, v) in &m {
                let a = actual
                    .get(k.as_str())
                    .ok_or_else(|| format!("unknown holder key {k}"))?;
                if a != v {
                    return Err(format!("{k}: got {a:?}, want {v:?} in {h:?}"));
                }
            }
            Ok(())
        }
    }
}

fn check_bool(got: bool, want: &str) -> Result<(), String> {
    let want: bool = want.trim().parse().map_err(|_| format!("not a bool: {want:?}"))?;
    if got == want { Ok(()) } else { Err(format!("got {got}")) }
}

fn run_one(fx: &Fixture, line: &str) -> Result<(), String> {
    let det = fx.detector();
    let defaults = RecordConfig::default();
    let (lhs, rhs) = line.split_once("=>").ok_or("missing =>")?;
    let (words, m) = kv(lhs);
    let rhs = rhs.trim();
    match words.first().map(String::as_str) {
        Some("check") => {
            let tty = match m.get("tty").map(String::as_str) {
                Some("none") => TtyIdentity::NotATty,
                Some(t) if t.starts_with("pts:") => TtyIdentity::Pts(t[4..].parse().map_err(|_| "bad pts")?),
                Some(t) if t.starts_with("vt:") => TtyIdentity::Other(PathBuf::from(&t[3..])),
                _ => return Err("bad tty".into()),
            };
            let hidepid_fallback = match m.get("fallback").map(String::as_str) {
                Some("skip") => HidepidFallback::Skip,
                _ => HidepidFallback::Start,
            };
            let req = CheckRequest {
                tty,
                euid: num(&m, "euid"),
                start_pid: num(&m, "start"),
                hidepid_fallback,
            };
            let want: StartDecision = rhs.parse().map_err(|e| format!("{e}"))?;
            let got = det.check_start(&req);
            if got == want {
                Ok(())
            } else {
                Err(format!("got `{got}`"))
            }
        }
        Some("holder") => {
            let (pts, start) = (num(&m, "pts"), num(&m, "start"));
            let got = match m.get("euid") {
                Some(_) => det.holder_as(pts, start, num(&m, "euid")),
                None => det.holder(pts, start),
            };
            check_holder(got, rhs)
        }
        Some("principal") => {
            let req = PrincipalRequest {
                euid: num(&m, "euid"),
                start_pid: num(&m, "start"),
                tmux_client_pid: m.get("client").map(|_| num(&m, "client")),
                claimed_user: m.get("claimed").cloned(),
            };
            let p = det.principal(&req);
            let (_, want) = kv(rhs);
            for (k, v) in &want {
                let (got, want) = match k.as_str() {
                    "user" => (Some(p.user.clone()), opt(v)),
                    "sudo_user" => (p.sudo_user.clone(), opt(v)),
                    "claimed" => (p.claimed_user.clone(), opt(v)),
                    "login_via" => (p.login_via.clone(), opt(v)),
                    "attribution" => (Some(attribution_name(p.attribution).to_owned()), opt(v)),
                    _ => return Err(format!("unknown principal key {k}")),
                };
                if got != want {
                    return Err(format!("{k}: got {got:?}, want {want:?} in {p:?}"));
                }
            }
            if p.scope != cinelog_proto::Scope::for_euid(req.euid) {
                return Err(format!("scope {:?}", p.scope));
            }
            Ok(())
        }
        Some("foreground") => {
            let mux = list(&m, "mux", &defaults.pause_on_multiplexer);
            let got = det.foreground(num(&m, "fg"), num(&m, "shell"), &mux);
            let (w, r) = kv(rhs);
            let ok = match (w.first().map(String::as_str), &got) {
                (Some("shell"), Foreground::Shell) => true,
                (Some("job"), Foreground::Job { leader_comm, pgrp }) => {
                    r.get("leader").is_none_or(|l| l == leader_comm) && *pgrp == num::<u32>(&m, "fg")
                }
                (Some("multiplexer"), Foreground::Multiplexer { pid, comm, .. }) => {
                    r.get("pid").is_none_or(|p| *p == pid.to_string()) && r.get("comm").is_none_or(|c| c == comm)
                }
                (Some("recorder"), Foreground::Recorder { pid, starttime, .. }) => {
                    r.get("pid").is_none_or(|p| *p == pid.to_string())
                        && r.get("start").is_none_or(|s| *s == starttime.to_string())
                }
                _ => false,
            };
            if ok { Ok(()) } else { Err(format!("got {got:?}")) }
        }
        Some("nested") => {
            let claim = NestedClaim {
                pid: num(&m, "pid"),
                starttime: num(&m, "start"),
            };
            check_bool(det.corroborate_nested(num(&m, "fg"), num(&m, "shell"), claim), rhs)
        }
        Some("relay") => {
            let relays = list(&m, "relays", &defaults.relays);
            check_bool(det.relay_in_foreground(num(&m, "fg"), &relays), rhs)
        }
        Some("valid") => {
            let pts = num(&m, "pts");
            let got = fx.registry.get(&pts).is_some_and(|e| det.entry_is_valid(e, pts));
            check_bool(got, rhs)
        }
        other => Err(format!("unknown expectation {other:?}")),
    }
}

#[test]
fn all_fixtures() {
    let mut files: Vec<PathBuf> = std::fs::read_dir(fixture_dir())
        .expect("fixture dir")
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|x| x == "txt"))
        .collect();
    files.sort();
    assert!(files.len() >= 10, "fixtures missing: {files:?}");

    let mut failures = Vec::new();
    let mut total = 0;
    for file in &files {
        let name = file.file_name().unwrap().to_string_lossy().into_owned();
        let spec = std::fs::read_to_string(file).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let fx = Fixture::materialize(&spec, dir.path()).unwrap_or_else(|e| panic!("{name}: {e}"));
        assert!(!fx.expectations.is_empty(), "{name} has no expectations");
        for (line, exp) in &fx.expectations {
            total += 1;
            if let Err(e) = run_one(&fx, exp) {
                failures.push(format!("{name}:{line}: `{exp}`: {e}"));
            }
        }
    }
    assert!(
        failures.is_empty(),
        "{} of {total} expectations failed:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

#[test]
fn unavailable_proc_skips_with_error() {
    let dir = tempfile::tempdir().unwrap();
    let det = cinelog_detect::ProcDetector::new(
        cinelog_detect::RealProcFs::at(dir.path().join("missing")),
        cinelog_detect::NoRegistry,
    );
    let req = CheckRequest {
        tty: TtyIdentity::Pts(3),
        euid: 1000,
        start_pid: 42,
        hidepid_fallback: HidepidFallback::Start,
    };
    assert_eq!(
        det.check_start(&req),
        StartDecision::Skip {
            reason: cinelog_detect::Reason::Error
        }
    );
    assert_eq!(det.holder(3, 42), None);
    assert_eq!(det.boot_id(), None);
}
