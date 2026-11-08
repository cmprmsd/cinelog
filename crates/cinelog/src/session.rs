//! The recording of the calling shell, found through the controlling tty (PROTOCOL §6.5).

use std::fmt;

use cinelog_core::registry::{ProcessProbe, Registry, RegistryEntry, RegistryError, TtyKey};
use cinelog_detect::{Detector, Pid};

/// A validated registry entry for the controlling tty.
#[derive(Debug, Clone)]
pub struct Current {
    pub key: TtyKey,
    pub entry: RegistryEntry,
}

#[derive(Debug)]
pub enum NotRecorded {
    NoTty,
    NoEntry(TtyKey),
    Stale(TtyKey),
    /// The entry's pid is alive but another process holds the master of that pts.
    WrongHolder {
        key: TtyKey,
        holder: Pid,
    },
    Registry(RegistryError),
}

impl fmt::Display for NotRecorded {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoTty => f.write_str("no controlling terminal"),
            Self::NoEntry(k) => write!(f, "{k} is not recorded"),
            Self::Stale(k) => write!(f, "the registry entry for {k} is stale (recorder gone)"),
            Self::WrongHolder { key, holder } => {
                write!(
                    f,
                    "the registry entry for {key} does not match its PTY holder (pid {holder})"
                )
            }
            Self::Registry(e) => e.fmt(f),
        }
    }
}

/// Looks up and validates the entry for `key`: pid, starttime and boot id must match a
/// running process, and, when a holder of that pts is visible, it must be that pid.
pub fn lookup(
    registry: &Registry,
    key: Option<TtyKey>,
    probe: &dyn ProcessProbe,
    detector: &dyn Detector,
    start_pid: Pid,
) -> Result<Current, NotRecorded> {
    let key = key.ok_or(NotRecorded::NoTty)?;
    let entry = registry
        .read(&key)
        .map_err(NotRecorded::Registry)?
        .ok_or_else(|| NotRecorded::NoEntry(key.clone()))?;
    if !entry.is_alive(probe) {
        return Err(NotRecorded::Stale(key));
    }
    if let TtyKey::Pts(n) = key
        && let Some(h) = detector.holder(n, start_pid)
        && h.pid != entry.pid
    {
        return Err(NotRecorded::WrongHolder { key, holder: h.pid });
    }
    Ok(Current { key, entry })
}

/// The current session of this process (`start_pid` = our parent, the shell).
pub fn current(ctx: &crate::ctx::Ctx) -> Result<Current, NotRecorded> {
    lookup(
        &ctx.registry(),
        cinelog_core::tty::controlling_tty(),
        &cinelog_core::registry::SystemProbe,
        &ctx.detector(),
        nix::unistd::getppid().as_raw().unsigned_abs(),
    )
}

#[cfg(test)]
mod tests {
    use cinelog_detect::{FoundVia, Holder, HolderClass, StartDecision};

    use super::*;
    use crate::check::tests::{FakeDetector, FakeProbe, entry};

    fn det(holder: Option<Pid>) -> FakeDetector {
        let mut d = FakeDetector::new(StartDecision::Recorded { uuid: String::new() });
        d.holder = holder.map(|pid| Holder {
            pid,
            uid: 1000,
            comm: "cinelog".into(),
            class: HolderClass::OwnRecorder { same_uid: true },
            via: FoundVia::Ancestor,
        });
        d
    }

    #[test]
    fn validation() {
        let dir = tempfile::tempdir().unwrap();
        let reg = Registry::new(dir.path());
        let key = Some(TtyKey::Pts(9));
        assert!(matches!(
            lookup(&reg, None, &FakeProbe, &det(None), 1),
            Err(NotRecorded::NoTty)
        ));
        assert!(matches!(
            lookup(&reg, key.clone(), &FakeProbe, &det(None), 1),
            Err(NotRecorded::NoEntry(_))
        ));
        reg.write(&TtyKey::Pts(9), &entry(777, "u-1")).unwrap();
        let cur = lookup(&reg, key.clone(), &FakeProbe, &det(Some(777)), 1).unwrap();
        assert_eq!(cur.entry.uuid, "u-1");
        assert!(lookup(&reg, key.clone(), &FakeProbe, &det(None), 1).is_ok());
        let err = lookup(&reg, key.clone(), &FakeProbe, &det(Some(5)), 1).unwrap_err();
        assert!(err.to_string().contains("pid 5"), "{err}");
        reg.write(&TtyKey::Pts(9), &entry(778, "u-1")).unwrap();
        assert!(matches!(
            lookup(&reg, key, &FakeProbe, &det(None), 1),
            Err(NotRecorded::Stale(_))
        ));
    }
}
