//! Events from the I/O threads to the writer thread (plan §5.1, §5.8).
//!
//! The channel itself is unbounded, but terminal data is admitted against a byte budget
//! (16 MiB). A sender never blocks: when the budget is exhausted the data is dropped and
//! counted, and the next event that gets through is preceded by a `gap`.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use cinelog_proto::CastMarker;
use crossbeam_channel::{Receiver, Sender};

/// Bytes of `o`/`i` payload that may wait for the writer.
pub(crate) const QUEUE_BUDGET: usize = 16 << 20;

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Payload {
    Output(Vec<u8>),
    Input(Vec<u8>),
    Marker(Box<CastMarker>),
    Resize {
        cols: u16,
        rows: u16,
    },
    /// Shell exit status and the session counters; the writer closes the cast after it.
    Exit {
        status: i32,
        bad_markers: u64,
        dropped_bytes: u64,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Msg {
    /// `t` is `CLOCK_BOOTTIME` in µs; the writer makes it relative to the current part.
    Event { t: u64, payload: Payload },
    /// Data was dropped between `from` and `to` (boottime µs).
    Gap { dropped_bytes: u64, from: u64, to: u64 },
    /// `cinelog resume` while degraded: open the next part.
    ResumeDegraded { t: u64 },
}

#[derive(Debug, Default)]
struct DropState {
    bytes: u64,
    from: u64,
    to: u64,
}

#[derive(Debug)]
struct Shared {
    queued: AtomicUsize,
    budget: usize,
    dropped_total: AtomicU64,
    drops: std::sync::Mutex<Option<DropState>>,
}

/// Sending side. Senders are serialised by the recorder's control lock, so a `gap` is
/// always written right before the event that ends it.
#[derive(Debug, Clone)]
pub(crate) struct EventTx {
    tx: Sender<Msg>,
    shared: Arc<Shared>,
}

/// Receiving side, held by the writer.
#[derive(Debug)]
pub(crate) struct EventRx {
    rx: Receiver<Msg>,
    shared: Arc<Shared>,
}

pub(crate) fn channel(budget: usize) -> (EventTx, EventRx) {
    let (tx, rx) = crossbeam_channel::unbounded();
    let shared = Arc::new(Shared {
        queued: AtomicUsize::new(0),
        budget,
        dropped_total: AtomicU64::new(0),
        drops: std::sync::Mutex::new(None),
    });
    (
        EventTx {
            tx,
            shared: Arc::clone(&shared),
        },
        EventRx { rx, shared },
    )
}

fn data_len(p: &Payload) -> usize {
    match p {
        Payload::Output(d) | Payload::Input(d) => d.len(),
        _ => 0,
    }
}

impl EventTx {
    /// Sends terminal data (`o`/`i`) if it fits the budget; returns whether it was queued.
    pub(crate) fn data(&self, t: u64, payload: Payload) -> bool {
        let len = data_len(&payload);
        let before = self.shared.queued.fetch_add(len, Ordering::AcqRel);
        if before + len > self.shared.budget {
            self.shared.queued.fetch_sub(len, Ordering::AcqRel);
            self.shared.dropped_total.fetch_add(len as u64, Ordering::Relaxed);
            let mut drops = self.shared.drops.lock().unwrap_or_else(|e| e.into_inner());
            let d = drops.get_or_insert(DropState {
                bytes: 0,
                from: t,
                to: t,
            });
            d.bytes += len as u64;
            d.to = t;
            return false;
        }
        self.flush_gap();
        let _ = self.tx.send(Msg::Event { t, payload });
        true
    }

    /// Sends an event that is never dropped (markers, resizes, exit).
    pub(crate) fn always(&self, t: u64, payload: Payload) {
        debug_assert_eq!(data_len(&payload), 0, "terminal data must go through data()");
        self.flush_gap();
        let _ = self.tx.send(Msg::Event { t, payload });
    }

    pub(crate) fn marker(&self, t: u64, m: CastMarker) {
        self.always(t, Payload::Marker(Box::new(m)));
    }

    pub(crate) fn control(&self, msg: Msg) {
        let _ = self.tx.send(msg);
    }

    /// Writes a pending `gap` now (also called at shutdown).
    pub(crate) fn flush_gap(&self) {
        let pending = {
            let mut drops = self.shared.drops.lock().unwrap_or_else(|e| e.into_inner());
            drops.take()
        };
        if let Some(d) = pending {
            let _ = self.tx.send(Msg::Gap {
                dropped_bytes: d.bytes,
                from: d.from,
                to: d.to,
            });
        }
    }

    pub(crate) fn dropped_total(&self) -> u64 {
        self.shared.dropped_total.load(Ordering::Relaxed)
    }
}

impl EventRx {
    /// Blocks up to `timeout`; `Err(true)` when all senders are gone.
    pub(crate) fn recv_timeout(&self, timeout: std::time::Duration) -> Result<Msg, bool> {
        match self.rx.recv_timeout(timeout) {
            Ok(m) => Ok(self.release(m)),
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => Err(false),
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => Err(true),
        }
    }

    /// Non-blocking; `Err(true)` when all senders are gone and the queue is empty.
    pub(crate) fn try_recv(&self) -> Result<Msg, bool> {
        match self.rx.try_recv() {
            Ok(m) => Ok(self.release(m)),
            Err(crossbeam_channel::TryRecvError::Empty) => Err(false),
            Err(crossbeam_channel::TryRecvError::Disconnected) => Err(true),
        }
    }

    fn release(&self, m: Msg) -> Msg {
        if let Msg::Event { payload, .. } = &m {
            self.shared.queued.fetch_sub(data_len(payload), Ordering::AcqRel);
        }
        m
    }

    #[cfg(test)]
    pub(crate) fn dropped_total(&self) -> u64 {
        self.shared.dropped_total.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn budget_drops_and_reports_a_gap() {
        let (tx, rx) = channel(10);
        assert!(tx.data(1, Payload::Output(vec![b'a'; 6])));
        assert!(!tx.data(2, Payload::Output(vec![b'b'; 6])), "over budget");
        assert!(!tx.data(3, Payload::Input(vec![b'c'; 6])));
        // Markers are never dropped and carry the pending gap in front of them.
        tx.marker(
            4,
            CastMarker::Unknown {
                k: "x".into(),
                fields: Default::default(),
            },
        );
        assert!(matches!(rx.try_recv(), Ok(Msg::Event { t: 1, .. })));
        assert_eq!(
            rx.try_recv(),
            Ok(Msg::Gap {
                dropped_bytes: 12,
                from: 2,
                to: 3
            })
        );
        assert!(matches!(rx.try_recv(), Ok(Msg::Event { t: 4, .. })));
        // Budget is released on receive.
        assert!(tx.data(5, Payload::Output(vec![b'd'; 10])));
        assert_eq!(rx.dropped_total(), 12);
        drop(tx);
        assert!(matches!(rx.try_recv(), Ok(Msg::Event { t: 5, .. })));
        assert_eq!(rx.try_recv(), Err(true));
    }
}
