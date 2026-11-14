//! Pause state machine of the outer recorder (plan §5.3, PROTOCOL.md §4.3–§4.6).
//!
//! Pure: it consumes observations (foreground classification, corroborated nested
//! recorders, user requests) and returns the `m` markers to write. A paused interval is
//! opened by the first active cause and closed when the last one goes away; each interval
//! leaves exactly one `pause` and one `resume` marker.

use cinelog_detect::Foreground;
use cinelog_proto::{
    CastMarker, Extra, MultiplexerInfo, MuxTarget, NestedInfo, NestedLink, NestedMode, PauseInfo, PauseReason,
    ResumeInfo, ResumeReason,
};

#[derive(Debug, Clone, PartialEq, Eq)]
struct ActiveNested {
    uuid: String,
    pid: u32,
    starttime: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ActiveMux {
    pid: u32,
    comm: String,
    pgrp: u32,
}

#[derive(Debug, Default)]
pub(crate) struct PauseMachine {
    user: Option<Option<String>>,
    nested: Option<ActiveNested>,
    mux: Option<ActiveMux>,
    /// Reason of the open paused interval.
    interval: Option<(PauseReason, Option<String>)>,
}

impl PauseMachine {
    pub(crate) fn is_paused(&self) -> bool {
        self.interval.is_some()
    }

    /// Registry `state_reason` while paused: the strongest cause still active (user >
    /// nested > multiplexer), which may differ from the one that opened the interval
    /// (`cinelog pause`, then `tmux a`, then `cinelog resume` stays paused for tmux).
    pub(crate) fn pause_reason(&self) -> Option<String> {
        let (reason, detail) = self.interval.as_ref()?;
        let (r, d) = if let Some(user) = &self.user {
            ("user", user.as_deref())
        } else if let Some(n) = &self.nested {
            ("nested", Some(n.uuid.as_str()))
        } else if let Some(m) = &self.mux {
            ("multiplexer", Some(m.comm.as_str()))
        } else {
            let r = match reason {
                PauseReason::Multiplexer => "multiplexer",
                PauseReason::Nested => "nested",
                PauseReason::User => "user",
            };
            (r, detail.as_deref())
        };
        Some(match d {
            Some(d) => format!("{r}: {d}"),
            None => r.to_owned(),
        })
    }

    /// The follow-up `multiplexer` marker for the client `pid`, once the tmux session it
    /// attached to is known; `None` when that client no longer holds the pause.
    pub(crate) fn on_mux_target(&self, pid: u32, target: MuxTarget) -> Option<CastMarker> {
        let m = self.mux.as_ref().filter(|m| m.pid == pid && self.interval.is_some())?;
        Some(CastMarker::Multiplexer(MultiplexerInfo {
            comm: m.comm.clone(),
            pid,
            pgrp: m.pgrp,
            tmux: Some(target),
            extra: Extra::new(),
        }))
    }

    fn any_cause(&self) -> bool {
        self.user.is_some() || self.nested.is_some() || self.mux.is_some()
    }

    fn open(&mut self, reason: PauseReason, detail: Option<String>, out: &mut Vec<CastMarker>) {
        if self.interval.is_none() {
            self.interval = Some((reason, detail.clone()));
            out.push(CastMarker::Pause(PauseInfo {
                reason,
                detail,
                extra: Extra::new(),
            }));
        }
    }

    fn close_if_idle(&mut self, reason: ResumeReason, alt_open: bool, out: &mut Vec<CastMarker>) {
        if self.interval.is_some() && !self.any_cause() {
            self.interval = None;
            out.push(CastMarker::Resume(ResumeInfo {
                reason,
                alt_fixup: alt_open,
                extra: Extra::new(),
            }));
        }
    }

    /// One foreground observation. `alt_open`: the recorded stream is on the alternate
    /// screen while the terminal is not (a fix-up must follow a resume).
    pub(crate) fn on_foreground(&mut self, fg: &Foreground, alt_open: bool) -> Vec<CastMarker> {
        let mut out = Vec::new();
        match fg {
            Foreground::Multiplexer { pid, comm, pgrp } => {
                let now = ActiveMux {
                    pid: *pid,
                    comm: comm.clone(),
                    pgrp: *pgrp,
                };
                if self.mux.as_ref() != Some(&now) {
                    if self.interval.is_none() {
                        out.push(CastMarker::Multiplexer(MultiplexerInfo {
                            comm: comm.clone(),
                            pid: *pid,
                            pgrp: *pgrp,
                            tmux: None,
                            extra: Extra::new(),
                        }));
                    }
                    self.mux = Some(now);
                    self.open(PauseReason::Multiplexer, Some(comm.clone()), &mut out);
                }
            }
            Foreground::Shell => {
                // Back at the prompt: no multiplexer and no nested recorder can still be in
                // front, whatever markers were lost (a crashed inner recorder).
                self.mux = None;
                self.nested = None;
                self.close_if_idle(ResumeReason::Foreground, alt_open, &mut out);
            }
            Foreground::Job { .. } | Foreground::Recorder { .. } => {
                self.mux = None;
                self.close_if_idle(ResumeReason::Foreground, alt_open, &mut out);
            }
        }
        out
    }

    /// A corroborated `nested-start`.
    pub(crate) fn on_nested_start(&mut self, info: &NestedInfo, mode: NestedMode) -> Vec<CastMarker> {
        let mut out = Vec::new();
        if mode == NestedMode::Ignore {
            return out;
        }
        out.push(CastMarker::Nested(NestedLink {
            child_uuid: info.uuid.clone(),
            scope: info.scope,
            user: info.user.clone(),
            sudo_user: info.sudo_user.clone(),
            url: info.url.clone(),
            pid: info.pid,
            starttime: info.starttime,
            mode,
            extra: Extra::new(),
        }));
        if mode == NestedMode::Pause {
            self.nested = Some(ActiveNested {
                uuid: info.uuid.clone(),
                pid: info.pid,
                starttime: info.starttime,
            });
            self.open(PauseReason::Nested, Some(info.uuid.clone()), &mut out);
        }
        out
    }

    /// `nested-end` of the active nested recorder (others are ignored).
    pub(crate) fn on_nested_end(&mut self, info: &NestedInfo, alt_open: bool) -> Vec<CastMarker> {
        let mut out = Vec::new();
        let matches = self
            .nested
            .as_ref()
            .is_some_and(|n| n.uuid == info.uuid && n.pid == info.pid && n.starttime == info.starttime);
        if matches {
            self.nested = None;
            self.close_if_idle(ResumeReason::NestedEnd, alt_open, &mut out);
        }
        out
    }

    pub(crate) fn on_user_pause(&mut self, reason: Option<String>) -> Vec<CastMarker> {
        let mut out = Vec::new();
        self.user = Some(reason.clone());
        self.open(PauseReason::User, reason, &mut out);
        out
    }

    pub(crate) fn on_user_resume(&mut self, alt_open: bool) -> Vec<CastMarker> {
        let mut out = Vec::new();
        self.user = None;
        self.close_if_idle(ResumeReason::User, alt_open, &mut out);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cinelog_proto::Scope;

    fn mux(pid: u32) -> Foreground {
        Foreground::Multiplexer {
            pid,
            comm: "tmux: client".into(),
            pgrp: pid,
        }
    }

    fn job(pgrp: u32, comm: &str) -> Foreground {
        Foreground::Job {
            pgrp,
            leader_comm: comm.into(),
        }
    }

    fn nested(uuid: &str, pid: u32) -> NestedInfo {
        NestedInfo {
            uuid: uuid.into(),
            pid,
            starttime: 99,
            scope: Scope::Team,
            user: "root".into(),
            sudo_user: Some("alice".into()),
            url: None,
            exit: None,
            extra: Extra::new(),
        }
    }

    /// Compact rendering of markers for table-driven tests.
    fn kinds(ms: &[CastMarker]) -> Vec<String> {
        ms.iter()
            .map(|m| match m {
                CastMarker::Pause(p) => format!("pause:{:?}", p.reason).to_lowercase(),
                CastMarker::Resume(r) => {
                    format!("resume:{:?}{}", r.reason, if r.alt_fixup { "+alt" } else { "" }).to_lowercase()
                }
                CastMarker::Nested(n) => format!("nested:{:?}", n.mode).to_lowercase(),
                other => other.kind().to_owned(),
            })
            .collect()
    }

    enum Ev {
        Fg(Foreground),
        Start(NestedInfo, NestedMode),
        End(NestedInfo),
        UserPause,
        UserResume,
    }

    fn run(evs: Vec<(Ev, &[&str])>) {
        let mut m = PauseMachine::default();
        for (i, (ev, want)) in evs.into_iter().enumerate() {
            let got = match ev {
                Ev::Fg(fg) => m.on_foreground(&fg, false),
                Ev::Start(info, mode) => m.on_nested_start(&info, mode),
                Ev::End(info) => m.on_nested_end(&info, false),
                Ev::UserPause => m.on_user_pause(Some("lunch".into())),
                Ev::UserResume => m.on_user_resume(false),
            };
            assert_eq!(kinds(&got), want, "step {i}");
        }
    }

    #[test]
    fn tmux_attach_from_the_prompt() {
        run(vec![
            (Ev::Fg(Foreground::Shell), &[]),
            (Ev::Fg(mux(10)), &["multiplexer", "pause:multiplexer"]),
            (Ev::Fg(mux(10)), &[]),
            (Ev::Fg(Foreground::Shell), &["resume:foreground"]),
            (Ev::Fg(Foreground::Shell), &[]),
        ]);
    }

    #[test]
    fn the_attached_session_is_linked_while_that_client_holds_the_pause() {
        let target = || MuxTarget {
            server_pid: 7,
            server_start: 9,
            session_id: "$1".into(),
            session_name: "ops".into(),
            extra: Extra::new(),
        };
        let mut m = PauseMachine::default();
        assert_eq!(m.on_mux_target(10, target()), None, "not paused");
        m.on_foreground(&mux(10), false);
        assert_eq!(m.on_mux_target(11, target()), None, "another client");
        let Some(CastMarker::Multiplexer(info)) = m.on_mux_target(10, target()) else {
            panic!("a multiplexer marker");
        };
        assert_eq!(
            (info.pid, info.comm.as_str(), info.tmux),
            (10, "tmux: client", Some(target()))
        );
        m.on_foreground(&Foreground::Shell, false);
        assert_eq!(m.on_mux_target(10, target()), None, "resumed");
    }

    #[test]
    fn sh_c_sleep_then_tmux_keeps_pgrp_and_changes_comm() {
        // `sh -c 'sleep 1; tmux a'`: a plain job first, the client appears later.
        run(vec![
            (Ev::Fg(job(20, "sh")), &[]),
            (Ev::Fg(mux(21)), &["multiplexer", "pause:multiplexer"]),
            (Ev::Fg(job(20, "sh")), &["resume:foreground"]),
            (Ev::Fg(Foreground::Shell), &[]),
        ]);
    }

    #[test]
    fn exec_tmux_is_seen_in_the_shell_pgrp() {
        // `exec tmux`: the detector reports the shell pgrp's leader as a multiplexer.
        run(vec![
            (Ev::Fg(mux(5)), &["multiplexer", "pause:multiplexer"]),
            (Ev::Fg(mux(5)), &[]),
        ]);
    }

    #[test]
    fn sudo_with_nested_recorder() {
        let n = nested("u-1", 30);
        run(vec![
            (Ev::Fg(job(29, "sudo")), &[]),
            (
                Ev::Start(n.clone(), NestedMode::Pause),
                &["nested:pause", "pause:nested"],
            ),
            (
                Ev::Fg(Foreground::Recorder {
                    pid: 30,
                    starttime: 99,
                    pgrp: 29,
                }),
                &[],
            ),
            (Ev::Fg(job(29, "sudo")), &[]),
            (Ev::End(nested("other", 31)), &[]),
            (Ev::End(n), &["resume:nestedend"]),
            (Ev::Fg(Foreground::Shell), &[]),
        ]);
    }

    #[test]
    fn crash_without_nested_end_resumes_at_the_prompt() {
        run(vec![
            (
                Ev::Start(nested("u-2", 40), NestedMode::Pause),
                &["nested:pause", "pause:nested"],
            ),
            (Ev::Fg(job(39, "sudo")), &[]),
            (Ev::Fg(Foreground::Shell), &["resume:foreground"]),
        ]);
    }

    #[test]
    fn nested_record_and_ignore_modes() {
        run(vec![
            (Ev::Start(nested("u-3", 50), NestedMode::Record), &["nested:record"]),
            (Ev::Start(nested("u-4", 51), NestedMode::Ignore), &[]),
            (Ev::Fg(Foreground::Shell), &[]),
        ]);
    }

    #[test]
    fn user_pause_survives_foreground_changes() {
        run(vec![
            (Ev::UserPause, &["pause:user"]),
            (Ev::Fg(mux(60)), &[]),
            (Ev::Fg(Foreground::Shell), &[]),
            (Ev::UserPause, &[]),
            (Ev::UserResume, &["resume:user"]),
            (Ev::UserResume, &[]),
        ]);
    }

    #[test]
    fn overlapping_causes_close_with_the_last_one() {
        run(vec![
            (Ev::Fg(mux(70)), &["multiplexer", "pause:multiplexer"]),
            (Ev::UserPause, &[]),
            (Ev::Fg(Foreground::Shell), &[]),
            (Ev::UserResume, &["resume:user"]),
        ]);
    }

    #[test]
    fn pause_reason_follows_the_causes_still_active() {
        let mut m = PauseMachine::default();
        m.on_user_pause(Some("lunch".into()));
        assert_eq!(m.pause_reason().as_deref(), Some("user: lunch"));
        // tmux attached during the user pause: no marker (§4.3), but the cause is tracked.
        assert!(m.on_foreground(&mux(80), false).is_empty());
        assert_eq!(m.pause_reason().as_deref(), Some("user: lunch"), "user wins");
        assert!(m.on_user_resume(false).is_empty(), "still paused for tmux");
        assert_eq!(m.pause_reason().as_deref(), Some("multiplexer: tmux: client"));
        m.on_nested_start(&nested("u-9", 81), NestedMode::Pause);
        assert_eq!(m.pause_reason().as_deref(), Some("nested: u-9"));
        m.on_nested_end(&nested("u-9", 81), false);
        assert_eq!(m.pause_reason().as_deref(), Some("multiplexer: tmux: client"));
        assert_eq!(
            kinds(&m.on_foreground(&Foreground::Shell, false)),
            ["resume:foreground"]
        );
        assert_eq!(m.pause_reason(), None);
        m.on_user_pause(None);
        assert_eq!(m.pause_reason().as_deref(), Some("user"));
    }

    #[test]
    fn alt_fixup_is_flagged_on_resume() {
        let mut m = PauseMachine::default();
        m.on_foreground(&mux(1), false);
        assert!(m.is_paused());
        assert_eq!(m.pause_reason().as_deref(), Some("multiplexer: tmux: client"));
        let out = m.on_foreground(&Foreground::Shell, true);
        assert_eq!(kinds(&out), ["resume:foreground+alt"]);
        assert!(!m.is_paused() && m.pause_reason().is_none());
    }
}
