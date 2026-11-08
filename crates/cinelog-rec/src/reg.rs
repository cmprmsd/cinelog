//! The recorder's own registry entry (plan §5.1, PROTOCOL.md §6.5): written at start,
//! rewritten atomically on every state change (pause, degraded, rotation), removed on exit.

use std::path::PathBuf;
use std::sync::Mutex;

use cinelog_core::logging::RecLog;
use cinelog_core::registry::{RecState, Registry, RegistryEntry, TtyKey};

#[derive(Debug)]
struct Inner {
    entry: RegistryEntry,
    paused: Option<String>,
    degraded: Option<String>,
    warned: bool,
}

#[derive(Debug)]
pub(crate) struct RegistryHandle {
    registry: Registry,
    key: TtyKey,
    log: Option<RecLog>,
    inner: Mutex<Inner>,
}

impl RegistryHandle {
    pub(crate) fn new(registry: Registry, key: TtyKey, entry: RegistryEntry, log: Option<RecLog>) -> Self {
        Self {
            registry,
            key,
            log,
            inner: Mutex::new(Inner {
                entry,
                paused: None,
                degraded: None,
                warned: false,
            }),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Writes the current entry; the first failure is logged, later ones are silent.
    fn store(&self, inner: &mut Inner) {
        let (state, reason) = match (&inner.degraded, &inner.paused) {
            (Some(r), _) => (RecState::Degraded, Some(r.clone())),
            (None, Some(r)) => (RecState::Paused, Some(r.clone())),
            (None, None) => (RecState::Live, None),
        };
        inner.entry.state = state;
        inner.entry.state_reason = reason;
        if let Err(e) = self.registry.write(&self.key, &inner.entry)
            && !inner.warned
        {
            inner.warned = true;
            if let Some(log) = &self.log {
                let _ = log.append(&format!("registry: {e}"));
            }
        }
    }

    pub(crate) fn publish(&self) {
        let mut inner = self.lock();
        self.store(&mut inner);
    }

    pub(crate) fn set_paused(&self, reason: Option<String>) {
        let mut inner = self.lock();
        if inner.paused != reason {
            inner.paused = reason;
            self.store(&mut inner);
        }
    }

    pub(crate) fn set_degraded(&self, reason: Option<String>) {
        let mut inner = self.lock();
        if inner.degraded != reason {
            inner.degraded = reason;
            self.store(&mut inner);
        }
    }

    pub(crate) fn set_cast(&self, cast: PathBuf, part: u32) {
        let mut inner = self.lock();
        inner.entry.cast = cast;
        inner.entry.part = part;
        self.store(&mut inner);
    }

    pub(crate) fn remove(&self) {
        let pid = self.lock().entry.pid;
        let _ = self.registry.remove_if_owner(&self.key, pid);
    }

    #[cfg(test)]
    pub(crate) fn snapshot(&self) -> RegistryEntry {
        self.lock().entry.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cinelog_proto::{Nonce, Scope};

    #[test]
    fn state_precedence_and_rewrites() {
        let dir = tempfile::tempdir().unwrap();
        let reg = Registry::new(dir.path());
        let entry = RegistryEntry {
            format: 1,
            pid: std::process::id(),
            starttime: 1,
            boot_id: "b".into(),
            uuid: "u".into(),
            nonce: Nonce::from_bytes([7; 16]),
            scope: Scope::Personal,
            cast: "/a.cast".into(),
            pts: "/dev/pts/9".into(),
            host_tty: String::new(),
            shell_pid: 2,
            parent_uuid: None,
            state: RecState::Live,
            state_reason: None,
            part: 1,
            version: String::new(),
            opts: String::new(),
        };
        let h = RegistryHandle::new(reg.clone(), TtyKey::Pts(9), entry, None);
        h.publish();
        let read = || reg.read(&TtyKey::Pts(9)).unwrap().unwrap();
        assert_eq!(read().state, RecState::Live);
        h.set_paused(Some("user".into()));
        assert_eq!(
            (read().state, read().state_reason.as_deref()),
            (RecState::Paused, Some("user"))
        );
        h.set_degraded(Some("enospc".into()));
        assert_eq!(read().state, RecState::Degraded);
        h.set_paused(None);
        assert_eq!(read().state, RecState::Degraded, "degraded wins over paused");
        h.set_degraded(None);
        h.set_cast("/b_part2.cast".into(), 2);
        let e = read();
        assert_eq!(
            (e.state, e.part, e.cast),
            (RecState::Live, 2, PathBuf::from("/b_part2.cast"))
        );
        assert_eq!(h.snapshot().part, 2);
        h.remove();
        assert!(reg.read(&TtyKey::Pts(9)).unwrap().is_none());
    }
}
