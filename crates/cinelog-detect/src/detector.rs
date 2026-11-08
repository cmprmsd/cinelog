//! [`ProcDetector`]: the [`Detector`] over any [`ProcFs`] and [`RegistryLookup`].

use std::collections::{HashMap, HashSet, VecDeque};
use std::io;
use std::path::{Path, PathBuf};

use cinelog_core::config::HidepidFallback;
use cinelog_core::registry::{ProcessProbe, RegistryEntry};
use cinelog_proto::{Attribution, Scope};

use crate::classify::{
    RECORDER_COMM, classify_holder, is_fallback_comm, is_multiplexer_server, login_via_comm, strip_deleted,
};
use crate::{
    CheckRequest, Detector, Foreground, FoundVia, Hidepid, Holder, HolderClass, NestedClaim, Pid, Principal,
    PrincipalRequest, ProcFs, ProcStat, Reason, RegistryLookup, StartDecision, TtyIdentity,
};

/// Ancestor walks stop after this many steps (pid loops in a racy or forged tree).
const MAX_DEPTH: usize = 256;

type UserNames = Box<dyn Fn(u32) -> Option<String> + Send + Sync>;

/// Registry lookup that never finds an entry (for `doctor` across uids and tests).
#[derive(Debug, Clone, Copy, Default)]
pub struct NoRegistry;

impl RegistryLookup for NoRegistry {
    fn entry_for_pts(&self, _pts: u32) -> Option<RegistryEntry> {
        None
    }
}

impl RegistryLookup for HashMap<u32, RegistryEntry> {
    fn entry_for_pts(&self, pts: u32) -> Option<RegistryEntry> {
        self.get(&pts).cloned()
    }
}

impl<T: RegistryLookup + ?Sized> RegistryLookup for &T {
    fn entry_for_pts(&self, pts: u32) -> Option<RegistryEntry> {
        (**self).entry_for_pts(pts)
    }
}

/// [`Detector`] over a [`ProcFs`].
pub struct ProcDetector<P: ProcFs, R: RegistryLookup> {
    proc: P,
    registry: R,
    self_exe: Option<PathBuf>,
    user_names: UserNames,
}

impl<P: ProcFs, R: RegistryLookup> std::fmt::Debug for ProcDetector<P, R> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProcDetector")
            .field("self_exe", &self.self_exe)
            .finish_non_exhaustive()
    }
}

fn system_user_name(uid: u32) -> Option<String> {
    nix::unistd::User::from_uid(nix::unistd::Uid::from_raw(uid))
        .ok()
        .flatten()
        .map(|u| u.name)
}

fn is_ptmx(target: &Path) -> bool {
    target.file_name().is_some_and(|n| n == "ptmx")
}

/// A resolved holder and, for a verified own recorder of the same uid, its registry entry.
struct Resolved {
    holder: Holder,
    entry: Option<RegistryEntry>,
}

/// How a master fd with the right tty-index relates to the devpts instance of our slave.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Instance {
    /// Same devpts instance as the slave.
    Same,
    /// The instance of the master or of the slave cannot be told.
    Unknown,
}

/// Outcome of looking for the holder: found, or nothing visible.
enum Lookup {
    Found(Box<Resolved>),
    NotFound,
    /// `/proc` itself is unusable; detection must not guess.
    Unavailable,
}

impl<P: ProcFs, R: RegistryLookup> ProcDetector<P, R> {
    /// The own exe is read from `/proc/self/exe`; user names come from the passwd database.
    pub fn new(proc: P, registry: R) -> Self {
        let self_exe = proc.exe(proc.self_pid()).ok().map(|p| strip_deleted(&p).to_path_buf());
        Self {
            proc,
            registry,
            self_exe,
            user_names: Box::new(system_user_name),
        }
    }

    /// Overrides the exe an own recorder must run (tests, or a binary started by path).
    pub fn with_self_exe(mut self, exe: impl Into<PathBuf>) -> Self {
        self.self_exe = Some(exe.into());
        self
    }

    /// Overrides the uid → user name lookup.
    pub fn with_user_names(mut self, f: impl Fn(u32) -> Option<String> + Send + Sync + 'static) -> Self {
        self.user_names = Box::new(f);
        self
    }

    pub fn proc(&self) -> &P {
        &self.proc
    }

    fn user_name(&self, uid: u32) -> String {
        (self.user_names)(uid).unwrap_or_else(|| uid.to_string())
    }

    fn own_euid(&self) -> u32 {
        self.proc
            .uids(self.proc.self_pid())
            .map(|u| u.effective)
            .unwrap_or_else(|_| nix::unistd::geteuid().as_raw())
    }

    /// Effective uid of `pid`, from status or the owner of its `/proc` directory.
    fn euid_of(&self, pid: Pid) -> Option<u32> {
        self.proc
            .uids(pid)
            .map(|u| u.effective)
            .or_else(|_| self.proc.owner_uid(pid))
            .ok()
    }

    /// Real uid of `pid`; the directory owner (effective uid) only if status is unreadable.
    fn ruid_of(&self, pid: Pid) -> Option<u32> {
        self.proc
            .uids(pid)
            .map(|u| u.real)
            .or_else(|_| self.proc.owner_uid(pid))
            .ok()
    }

    /// `pid` and its ancestors, nearest first, as far as their stat is readable.
    pub fn ancestry(&self, pid: Pid) -> Vec<ProcStat> {
        let mut out: Vec<ProcStat> = Vec::new();
        let mut cur = pid;
        while cur != 0 && out.len() < MAX_DEPTH && !out.iter().any(|s| s.pid == cur) {
            let Ok(st) = self.proc.stat(cur) else { break };
            cur = st.ppid;
            out.push(st);
        }
        out
    }

    /// Whether `pid` holds a master with tty-index `pts` in any devpts instance; `Err` if its
    /// fds are unreadable.
    pub fn holds_master(&self, pid: Pid, pts: u32) -> io::Result<bool> {
        Ok(self
            .proc
            .fds(pid)?
            .iter()
            .any(|fd| is_ptmx(&fd.target) && self.proc.fdinfo_tty_index(pid, fd.fd).ok().flatten() == Some(pts)))
    }

    /// Best match among `pid`'s masters with tty-index `pts`, given the devpts instance of
    /// the slave. Masters of another instance (a container's own devpts) never match.
    fn master_match(&self, pid: Pid, pts: u32, slave: Option<u64>) -> io::Result<Option<Instance>> {
        let mut best = None;
        for fd in self.proc.fds(pid)? {
            if !is_ptmx(&fd.target) || self.proc.fdinfo_tty_index(pid, fd.fd).ok().flatten() != Some(pts) {
                continue;
            }
            match (slave, self.proc.devpts_dev(pid, fd.fd).ok().flatten()) {
                (Some(s), Some(m)) if s == m => return Ok(Some(Instance::Same)),
                (Some(_), Some(_)) => {}
                _ => best = Some(Instance::Unknown),
            }
        }
        Ok(best)
    }

    /// devpts instance of `/dev/pts/<pts>` as held open by `chain` (the shell first), or by
    /// this process.
    fn slave_devpts(&self, pts: u32, chain: &[ProcStat]) -> Option<u64> {
        let slave = PathBuf::from(format!("/dev/pts/{pts}"));
        chain
            .iter()
            .map(|s| s.pid)
            .chain(std::iter::once(self.proc.self_pid()))
            .find_map(|pid| {
                self.proc
                    .fds(pid)
                    .ok()?
                    .iter()
                    .filter(|fd| fd.target == slave)
                    .find_map(|fd| self.proc.devpts_dev(pid, fd.fd).ok().flatten())
            })
    }

    /// Registry validation (§5.1): pid runs with that starttime, boot_id matches, and the pid
    /// holds the master of `pts`.
    pub fn entry_is_valid(&self, entry: &RegistryEntry, pts: u32) -> bool {
        Detector::boot_id(self).as_deref() == Some(entry.boot_id.as_str())
            && Detector::starttime(self, entry.pid) == Some(entry.starttime)
            && self.holds_master(entry.pid, pts).unwrap_or(false)
    }

    /// [`Detector::holder`] as seen by `euid` (same-uid vs. other-uid recorder), for
    /// `rec --check` on behalf of another process and for `doctor`.
    pub fn holder_as(&self, pts: u32, start_pid: Pid, euid: u32) -> Option<Holder> {
        match self.lookup(pts, start_pid, euid) {
            Lookup::Found(r) => Some(r.holder),
            Lookup::NotFound | Lookup::Unavailable => None,
        }
    }

    fn exe_is_cinelog(&self, pid: Pid) -> bool {
        match self.proc.exe(pid) {
            Ok(exe) => {
                let exe = strip_deleted(&exe);
                self.self_exe.as_deref() == Some(exe)
                    || exe
                        .file_name()
                        .and_then(|n| n.to_str())
                        .is_some_and(|n| n.starts_with(RECORDER_COMM))
            }
            // Another uid's recorder: exe is unreadable, comm alone has to do.
            Err(_) => true,
        }
    }

    fn resolve(&self, st: &ProcStat, via: FoundVia, pts: u32, euid: u32) -> Resolved {
        let uid = self.euid_of(st.pid).unwrap_or(u32::MAX);
        let mut entry = None;
        let class = if st.comm == RECORDER_COMM && self.exe_is_cinelog(st.pid) {
            if uid != euid {
                Some(HolderClass::OwnRecorder { same_uid: false })
            } else {
                entry = self.registry.entry_for_pts(pts).filter(|e| {
                    e.pid == st.pid
                        && e.starttime == st.starttime
                        && Detector::boot_id(self).as_deref() == Some(e.boot_id.as_str())
                });
                entry.is_some().then_some(HolderClass::OwnRecorder { same_uid: true })
            }
        } else {
            None
        };
        let class = class.unwrap_or_else(|| {
            let cmdline = self.proc.cmdline(st.pid).unwrap_or_default();
            classify_holder(&st.comm, &cmdline).0
        });
        Resolved {
            holder: Holder {
                pid: st.pid,
                uid,
                comm: st.comm.clone(),
                class,
                via,
            },
            entry,
        }
    }

    fn lookup(&self, pts: u32, start_pid: Pid, euid: u32) -> Lookup {
        let chain = self.ancestry(start_pid);
        let slave = self.slave_devpts(pts, &chain);
        let mut denied = Vec::new();
        for st in &chain {
            match self.master_match(st.pid, pts, slave) {
                Ok(Some(_)) => return Lookup::Found(Box::new(self.resolve(st, FoundVia::Ancestor, pts, euid))),
                Ok(None) => {}
                Err(_) => denied.push(st),
            }
        }

        let pids = match self.proc.pids() {
            Ok(p) => p,
            Err(_) if chain.is_empty() => return Lookup::Unavailable,
            Err(_) => Vec::new(),
        };
        let in_chain: HashSet<Pid> = chain.iter().map(|s| s.pid).collect();
        let mut found: Vec<(Instance, ProcStat)> = pids
            .into_iter()
            .filter(|p| !in_chain.contains(p))
            .filter_map(|p| Some((self.master_match(p, pts, slave).ok()??, self.proc.stat(p).ok()?)))
            .collect();
        // Several holders left (instances we cannot tell apart): a confirmed instance first,
        // then a holder of our own uid, then the lowest pid, so the result is deterministic.
        found.sort_by_key(|(inst, s)| (*inst, self.euid_of(s.pid) != Some(euid), s.pid));
        let scan_hit = found.first().map(|(inst, st)| (*inst, st));

        // Only ancestors whose fds we could not read can be the invisible holder.
        let fallback = denied.into_iter().find(|s| is_fallback_comm(&s.comm));
        let found = |st: &ProcStat, via| Lookup::Found(Box::new(self.resolve(st, via, pts, euid)));
        match (scan_hit, fallback) {
            (Some((Instance::Same, st)), _) => found(st, FoundVia::Scan),
            // Our slave's instance is known but this holder's is not: it may well belong to
            // another devpts, so the denied ancestor is the better guess.
            (Some((Instance::Unknown, _)), Some(st)) if slave.is_some() => found(st, FoundVia::CommFallback),
            (Some((_, st)), _) => found(st, FoundVia::Scan),
            (None, Some(st)) => found(st, FoundVia::CommFallback),
            (None, None) => Lookup::NotFound,
        }
    }

    fn reason_of(&self, holder: &Holder) -> Reason {
        match holder.class {
            HolderClass::OwnRecorder { same_uid: false } => Reason::NestedUid,
            _ => {
                let cmdline = self.proc.cmdline(holder.pid).unwrap_or_default();
                classify_holder(&holder.comm, &cmdline).1
            }
        }
    }

    /// stat of every visible process.
    fn snapshot(&self) -> Vec<ProcStat> {
        self.proc
            .pids()
            .unwrap_or_default()
            .into_iter()
            .filter_map(|p| self.proc.stat(p).ok())
            .collect()
    }

    /// Members of `pgrp` and all their descendants, breadth-first (nearest to the shell first).
    fn pgrp_tree(&self, pgrp: Pid) -> Vec<ProcStat> {
        let all = self.snapshot();
        let mut children: HashMap<Pid, Vec<usize>> = HashMap::new();
        for (i, s) in all.iter().enumerate() {
            children.entry(s.ppid).or_default().push(i);
        }
        let mut seen: HashSet<Pid> = HashSet::new();
        let mut queue: VecDeque<usize> = all
            .iter()
            .enumerate()
            .filter(|(_, s)| s.pgrp == pgrp)
            .map(|(i, _)| i)
            .collect();
        let mut out = Vec::new();
        while let Some(i) = queue.pop_front() {
            let s = &all[i];
            if !seen.insert(s.pid) {
                continue;
            }
            queue.extend(children.get(&s.pid).into_iter().flatten().copied());
            out.push(s.clone());
        }
        out
    }

    /// comm of the nearest login path in `chain`. A multiplexer server severs the path: the
    /// `systemd` above a daemonized tmux server says nothing about how root logged in.
    fn login_via(&self, chain: &[ProcStat]) -> String {
        chain
            .iter()
            .take_while(|s| !is_multiplexer_server(&s.comm))
            .find(|s| login_via_comm(&s.comm))
            .map_or_else(|| "unknown".to_owned(), |s| s.comm.clone())
    }

    /// First process with real uid ≠ 0 in `pid`'s ancestry (§5.6 rule 1). The second value
    /// reports whether the walk reached a multiplexer server first (a shared root tmux pane).
    fn first_non_root(&self, chain: &[ProcStat]) -> (Option<u32>, bool) {
        for (i, st) in chain.iter().enumerate() {
            if let Some(ruid) = self.ruid_of(st.pid)
                && ruid != 0
            {
                return (Some(ruid), false);
            }
            // The walk's own start (a recorder, or a screen client) is never the server.
            if i > 0 && is_multiplexer_server(&st.comm) {
                return (None, true);
            }
        }
        (None, false)
    }
}

impl<P: ProcFs, R: RegistryLookup> Detector for ProcDetector<P, R> {
    fn check_start(&self, req: &CheckRequest) -> StartDecision {
        let scope = Scope::for_euid(req.euid);
        let pts = match &req.tty {
            TtyIdentity::NotATty => {
                return StartDecision::Skip {
                    reason: Reason::NotATty,
                };
            }
            TtyIdentity::Other(_) => {
                return StartDecision::Start {
                    scope,
                    reason: Reason::Vt,
                };
            }
            TtyIdentity::Pts(n) => *n,
        };
        match self.lookup(pts, req.start_pid, req.euid) {
            Lookup::Found(r) => match r.entry {
                Some(e) => StartDecision::Recorded { uuid: e.uuid },
                None => StartDecision::Start {
                    scope,
                    reason: self.reason_of(&r.holder),
                },
            },
            Lookup::Unavailable => StartDecision::Skip { reason: Reason::Error },
            Lookup::NotFound => match self.proc.hidepid() {
                Ok(Hidepid::Off) => StartDecision::Start {
                    scope,
                    reason: Reason::Unknown,
                },
                // Unreadable mount table: treat like hidepid, the config decides.
                Ok(_) | Err(_) => match req.hidepid_fallback {
                    HidepidFallback::Start => StartDecision::Start {
                        scope,
                        reason: Reason::Hidepid,
                    },
                    HidepidFallback::Skip => StartDecision::Skip {
                        reason: Reason::HidepidSkip,
                    },
                },
            },
        }
    }

    fn holder(&self, pts: u32, start_pid: Pid) -> Option<Holder> {
        self.holder_as(pts, start_pid, self.own_euid())
    }

    fn foreground(&self, fg_pgrp: Pid, shell_pgrp: Pid, multiplexers: &[String]) -> Foreground {
        let is_mux = |comm: &str| multiplexers.iter().any(|m| m == comm);
        if fg_pgrp == shell_pgrp {
            // `exec tmux` keeps the shell's pid and pgrp; only the leader's comm changes.
            return match self.proc.stat(fg_pgrp) {
                Ok(st) if is_mux(&st.comm) => Foreground::Multiplexer {
                    pid: st.pid,
                    comm: st.comm,
                    pgrp: fg_pgrp,
                },
                _ => Foreground::Shell,
            };
        }
        let tree = self.pgrp_tree(fg_pgrp);
        // A multiplexer anywhere in the job pauses, even below a (nested) recorder.
        if let Some(st) = tree.iter().find(|s| is_mux(&s.comm)) {
            return Foreground::Multiplexer {
                pid: st.pid,
                comm: st.comm.clone(),
                pgrp: fg_pgrp,
            };
        }
        if let Some(st) = tree.iter().find(|s| s.comm == RECORDER_COMM) {
            return Foreground::Recorder {
                pid: st.pid,
                starttime: st.starttime,
                pgrp: fg_pgrp,
            };
        }
        let leader_comm = tree
            .iter()
            .find(|s| s.pid == fg_pgrp)
            .or_else(|| tree.iter().find(|s| s.pgrp == fg_pgrp))
            .map(|s| s.comm.clone())
            .unwrap_or_default();
        Foreground::Job {
            pgrp: fg_pgrp,
            leader_comm,
        }
    }

    fn corroborate_nested(&self, fg_pgrp: Pid, shell_pgrp: Pid, claim: NestedClaim) -> bool {
        if fg_pgrp == shell_pgrp || claim.pid == 0 {
            return false;
        }
        let chain = self.ancestry(claim.pid);
        let Some(me) = chain.first() else { return false };
        if me.pid != claim.pid || me.comm != RECORDER_COMM || me.starttime != claim.starttime {
            return false;
        }
        // The claimant or one of its ancestors is a member of the fg pgrp, reached before the
        // walk leaves the shell's job (the shell itself is in shell_pgrp).
        for st in &chain {
            if st.pgrp == fg_pgrp {
                return true;
            }
            if st.pgrp == shell_pgrp {
                return false;
            }
        }
        false
    }

    fn principal(&self, req: &PrincipalRequest) -> Principal {
        let scope = Scope::for_euid(req.euid);
        let claimed_user = req.claimed_user.clone().filter(|u| !u.is_empty());
        if scope == Scope::Personal {
            return Principal {
                scope,
                user: self.user_name(req.euid),
                sudo_user: None,
                claimed_user,
                attribution: Attribution::None,
                login_via: None,
            };
        }

        let team = |user: String, attribution: Attribution, login_via: Option<String>| Principal {
            scope,
            user: user.clone(),
            sudo_user: Some(user),
            claimed_user: claimed_user.clone(),
            attribution,
            login_via,
        };
        let chain = self.ancestry(req.start_pid);
        let (found, via_tmux) = self.first_non_root(&chain);
        if let Some(uid) = found {
            return team(self.user_name(uid), Attribution::Ancestor, None);
        }
        let loginuid = self.proc.loginuid(req.start_pid).ok().flatten().filter(|&u| u != 0);
        if via_tmux {
            if let Some(client) = req.tmux_client_pid
                && let (Some(uid), _) = self.first_non_root(&self.ancestry(client))
            {
                return team(self.user_name(uid), Attribution::Ancestor, None);
            }
            if let Some(uid) = loginuid {
                return team(self.user_name(uid), Attribution::Inherited, None);
            }
        } else if let Some(uid) = loginuid {
            return team(self.user_name(uid), Attribution::Loginuid, None);
        }
        team("root".to_owned(), Attribution::None, Some(self.login_via(&chain)))
    }

    fn relay_in_foreground(&self, fg_pgrp: Pid, relays: &[String]) -> bool {
        self.snapshot()
            .iter()
            .any(|s| s.pgrp == fg_pgrp && relays.contains(&s.comm))
    }

    fn starttime(&self, pid: Pid) -> Option<u64> {
        self.proc.stat(pid).ok().map(|s| s.starttime)
    }

    fn boot_id(&self) -> Option<String> {
        self.proc.boot_id().ok()
    }
}

impl<P: ProcFs, R: RegistryLookup> ProcessProbe for ProcDetector<P, R> {
    fn starttime(&self, pid: u32) -> Option<u64> {
        Detector::starttime(self, pid)
    }

    fn boot_id(&self) -> Option<String> {
        Detector::boot_id(self)
    }
}
