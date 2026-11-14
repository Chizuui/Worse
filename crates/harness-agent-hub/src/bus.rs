//! Event fan-out: one broadcast channel, hub-wide monotonically increasing `seq`.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use harness_core_api::{EventKind, EventStream, HubEvent, SessionId};
use tokio::sync::broadcast;

/// Cloneable event publisher.
#[derive(Debug, Clone)]
pub struct EventBus {
    sender: broadcast::Sender<HubEvent>,
    seq: Arc<AtomicU64>,
    // seq allocation and send share one critical section so subscribers never see seq out of order.
    order: Arc<Mutex<()>>,
}

impl EventBus {
    /// Bus with the given buffer capacity.
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        let (sender, _) = broadcast::channel(capacity.max(16));
        Self {
            sender,
            seq: Arc::new(AtomicU64::new(0)),
            order: Arc::new(Mutex::new(())),
        }
    }

    /// Publishes one event (no subscribers is fine).
    pub fn emit(&self, session_id: Option<&SessionId>, kind: EventKind) {
        let _guard = self.order.lock();
        let seq = self.seq.fetch_add(1, Ordering::SeqCst) + 1;
        let _ = self.sender.send(HubEvent {
            seq,
            session_id: session_id.cloned(),
            kind,
        });
    }

    /// `seq` of the last published event. Read by a session actor while it builds a snapshot: every event the actor
    /// emitted before has a `seq` at or below it.
    #[must_use]
    pub fn last_seq(&self) -> u64 {
        let _guard = self.order.lock();
        self.seq.load(Ordering::SeqCst)
    }

    /// New subscription.
    #[must_use]
    pub fn subscribe(&self) -> EventStream {
        EventStream::new(self.sender.subscribe())
    }
}
