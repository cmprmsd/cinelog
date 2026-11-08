//! The UI bus behind `GET /api/events` (plan §7.4; docs/API.md §9.2): an in-process
//! broadcast of [`BusEvent`]s. Contracts, complete.
//!
//! Publishers: the live watcher (`live`, M4-A: session start/update/end, index and import
//! progress), the annotation handlers (`api::annotations`, M4-C: `annotation`, `tag`) and
//! `POST /api/forget` (`forgotten`). The only subscriber is the `/events` SSE handler.

use tokio::sync::{broadcast, watch};

use crate::api::types::BusEvent;

/// Events a subscriber may fall behind before it gets `RecvError::Lagged` and must resync
/// (the `/events` handler then sends a fresh `hello`).
pub const BUS_CAPACITY: usize = 1024;

/// Cheap to clone; every clone publishes to the same subscribers.
#[derive(Debug, Clone)]
pub struct Bus {
    tx: broadcast::Sender<BusEvent>,
    closing: watch::Sender<bool>,
}

impl Default for Bus {
    fn default() -> Self {
        Self::new()
    }
}

impl Bus {
    pub fn new() -> Self {
        Self {
            tx: broadcast::channel(BUS_CAPACITY).0,
            closing: watch::channel(false).0,
        }
    }

    /// Sends `event` to every current subscriber (dropped when there is none). Callable
    /// from any thread, including `spawn_blocking` handlers.
    pub fn publish(&self, event: BusEvent) {
        let _ = self.tx.send(event);
    }

    /// Every event published from now on.
    pub fn subscribe(&self) -> broadcast::Receiver<BusEvent> {
        self.tx.subscribe()
    }

    pub fn subscribers(&self) -> usize {
        self.tx.receiver_count()
    }

    /// The server is shutting down: every SSE stream (`/events`, `/live`) ends, so the
    /// graceful shutdown does not wait for them.
    pub fn close(&self) {
        self.closing.send_replace(true);
    }

    pub fn is_closed(&self) -> bool {
        *self.closing.borrow()
    }

    /// Resolves once [`Self::close`] was called (at once if it already was).
    pub async fn closed(&self) {
        let mut rx = self.closing.subscribe();
        let _ = rx.wait_for(|closing| *closing).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::types::{LegacyProgress, Tag};
    use tokio::sync::broadcast::error::RecvError;

    fn tag(n: usize) -> BusEvent {
        BusEvent::Tag(Tag {
            name: format!("t{n}"),
            color: None,
            count: 0,
        })
    }

    #[tokio::test]
    async fn publish_subscribe_and_lag() {
        let bus = Bus::new();
        bus.publish(tag(0));
        let mut rx = bus.subscribe();
        assert_eq!(bus.subscribers(), 1);
        let from_thread = bus.clone();
        std::thread::spawn(move || from_thread.publish(BusEvent::ImportProgress(LegacyProgress::default())))
            .join()
            .unwrap();
        assert_eq!(
            rx.recv().await.unwrap(),
            BusEvent::ImportProgress(LegacyProgress::default()),
            "events before subscribing are not replayed"
        );
        for n in 0..=BUS_CAPACITY {
            bus.publish(tag(n));
        }
        assert!(matches!(rx.recv().await, Err(RecvError::Lagged(1))));
        assert_eq!(rx.recv().await.unwrap(), tag(1));
    }

    #[tokio::test]
    async fn close_wakes_waiters() {
        let bus = Bus::new();
        let waiter = tokio::spawn({
            let bus = bus.clone();
            async move { bus.closed().await }
        });
        assert!(!bus.is_closed());
        bus.close();
        waiter.await.unwrap();
        assert!(bus.is_closed());
        bus.closed().await;
    }
}
