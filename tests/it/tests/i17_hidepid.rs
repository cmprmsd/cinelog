//! I17: detection under `hidepid=2` and with devpts instances (plan §5.2 step 3, §12.2).
//!
//! The plan asks for a privileged `unshare -m` job; [`cinelog_it::procns`] builds the same
//! situation without root (a user namespace with a subordinate uid as the "other" owner of
//! the PTY master, a private pid namespace with its own `/proc`, devpts instances).

use cinelog_it::procns::{Holder, Layout};
use cinelog_it::{Need, Sandbox, SandboxOptions, need};

fn sandbox(fallback: &str) -> Sandbox {
    Sandbox::with(SandboxOptions {
        record: vec![format!("hidepid_fallback = \"{fallback}\"")],
        ..Default::default()
    })
    .unwrap()
}

fn layout(hidepid: bool, decoy: bool, holder: Holder, comm: &str) -> Layout {
    Layout {
        hidepid,
        decoy,
        holder,
        holder_comm: comm.into(),
    }
}

/// `cinelog rec --check` on the layout's PTY.
fn check(sb: &Sandbox, l: &Layout) -> String {
    let bin = sb.bin.as_ref().unwrap().to_string_lossy().into_owned();
    let o = sb.in_procns(l, &[bin, "rec".into(), "--check".into()]).unwrap();
    assert_eq!(o.tty, "/dev/pts/0", "{l:?}: a fresh devpts instance");
    if l.decoy {
        assert_eq!(
            o.decoy_tty.as_deref(),
            Some("/dev/pts/0"),
            "{l:?}: the decoy has the same index"
        );
    }
    o.output.trim().to_owned()
}

#[test]
fn hidepid_fallback_follows_the_config() {
    need!(Need::ProcNs, Need::Recorder);
    let invisible = layout(true, false, Holder::Denied, "python3");
    for (fallback, want) in [("start", "start personal hidepid"), ("skip", "skip hidepid")] {
        let sb = sandbox(fallback);
        assert_eq!(check(&sb, &invisible), want, "hidepid_fallback = {fallback}");
        // The same invisible holder behind a decoy in another devpts instance: the decoy is
        // never taken for the holder.
        let behind_decoy = Layout {
            decoy: true,
            ..invisible.clone()
        };
        assert_eq!(check(&sb, &behind_decoy), want, "{fallback}, with a decoy");
    }
}

#[test]
fn hidepid_only_matters_when_the_holder_is_invisible() {
    need!(Need::ProcNs, Need::Recorder);
    for fallback in ["start", "skip"] {
        let sb = sandbox(fallback);
        // hidepid=2, but the holder runs with our credentials: the ancestor walk finds it.
        assert_eq!(
            check(&sb, &layout(true, false, Holder::Readable, "script")),
            "start personal script",
            "{fallback}: visible holder under hidepid"
        );
        // No hidepid: an unreadable holder that is not a known relay is an unknown terminal,
        // never a hidepid decision.
        assert_eq!(
            check(&sb, &layout(false, false, Holder::Denied, "python3")),
            "start personal unknown",
            "{fallback}: unreadable holder without hidepid"
        );
    }
}

#[test]
fn unreadable_holder_is_classified_by_comm() {
    need!(Need::ProcNs, Need::Recorder);
    let sb = sandbox("skip");
    // An unreadable ancestor named like a known holder (the root-owned master case).
    assert_eq!(
        check(&sb, &layout(false, false, Holder::Denied, "tmux: server")),
        "start personal tmux-server"
    );
}

#[test]
fn devpts_index_collision_prefers_the_nearest_holder() {
    need!(Need::ProcNs, Need::Recorder);
    let sb = sandbox("skip");
    // The decoy (comm `script`) holds tty-index 0 of instance A, our pts is 0 of instance B.
    // Readable holder: the ancestor walk wins.
    assert_eq!(
        check(&sb, &layout(false, true, Holder::Readable, "screen")),
        "start personal screen"
    );
    // Unreadable holder: the full scan finds only the decoy, which belongs to another
    // instance, so the comm fallback on the unreadable ancestor decides.
    assert_eq!(
        check(&sb, &layout(false, true, Holder::Denied, "tmux: server")),
        "start personal tmux-server"
    );
}

/// The recorder itself acts on the decision: `start` records, `skip` falls back to the
/// unrecorded shell with `CINELOG_FAILED` naming the reason.
#[test]
fn recorder_honours_the_hidepid_decision() {
    need!(Need::ProcNs, Need::Recorder);
    let invisible = layout(true, false, Holder::Denied, "python3");
    for (fallback, want_failed, want_cast) in [("start", "", true), ("skip", "hidepid", false)] {
        let sb = sandbox(fallback);
        let bin = sb.bin.as_ref().unwrap().to_string_lossy().into_owned();
        let program = [
            bin,
            "rec".into(),
            "--".into(),
            "/bin/sh".into(),
            "-c".into(),
            "printf '<%s>\\n' \"$CINELOG_FAILED\"".into(),
        ];
        let o = sb.in_procns(&invisible, &program).unwrap();
        assert!(
            o.output.contains(&format!("<{want_failed}>")),
            "{fallback}: {:?}\nrec.log: {}",
            o.output,
            sb.rec_log()
        );
        let casts = sb.casts();
        assert_eq!(!casts.is_empty(), want_cast, "{fallback}: casts {:?}", sb.cast_paths());
        if want_cast {
            let session = casts[0].session().expect("session marker");
            assert_eq!(session.u64("uid"), Some(u64::from(nix::unistd::getuid().as_raw())));
            assert_eq!(casts[0].exit_status(), Some("0"));
        } else {
            assert!(
                sb.rec_log().contains("hidepid"),
                "the reason is logged: {}",
                sb.rec_log()
            );
        }
    }
}
