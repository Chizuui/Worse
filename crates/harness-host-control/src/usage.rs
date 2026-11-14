//! Correlation of shell-initiated resource-usage snapshots.
//!
//! Mirrors `packages/desktop/src/main/resourceManagerHostSampling.ts`: at most
//! one request is in flight per Host; a caller that times out (or arrives while
//! a request is still in flight) gets the previous result instead of queuing
//! another request; a late result still refreshes that cache. Stale or foreign
//! `requestId`s are ignored.

use std::sync::Mutex;

use harness_host_protocol::ResourceUsageSnapshot;
use tokio::sync::oneshot;
use uuid::Uuid;

use crate::shared::lock;

/// Result of trying to start one snapshot request.
pub(crate) enum UsageBegin {
    /// A new request id was registered; send it, then await `receiver`.
    Started {
        request_id: String,
        receiver: oneshot::Receiver<ResourceUsageSnapshot>,
    },
    /// A request is already in flight; this is the previous result (or empty).
    InFlight(ResourceUsageSnapshot),
}

struct InFlight {
    request_id: String,
    waiter: Option<oneshot::Sender<ResourceUsageSnapshot>>,
}

#[derive(Default)]
struct UsageInner {
    in_flight: Option<InFlight>,
    last: Option<ResourceUsageSnapshot>,
}

/// Owns the in-flight request id and the last snapshot of one Host.
#[derive(Default)]
pub(crate) struct UsageCorrelator {
    inner: Mutex<UsageInner>,
}

impl UsageCorrelator {
    pub(crate) fn begin(&self) -> UsageBegin {
        let mut inner = lock(&self.inner);
        if inner.in_flight.is_some() {
            return UsageBegin::InFlight(inner.last.clone().unwrap_or_default());
        }
        let request_id = Uuid::new_v4().to_string();
        let (sender, receiver) = oneshot::channel();
        inner.in_flight = Some(InFlight {
            request_id: request_id.clone(),
            waiter: Some(sender),
        });
        UsageBegin::Started {
            request_id,
            receiver,
        }
    }

    /// Stores a Host result; returns `false` for a stale or foreign id.
    pub(crate) fn resolve(&self, request_id: &str, snapshot: ResourceUsageSnapshot) -> bool {
        let waiter = {
            let mut inner = lock(&self.inner);
            let matches = inner
                .in_flight
                .as_ref()
                .is_some_and(|flight| flight.request_id == request_id);
            if !matches {
                return false;
            }
            let waiter = inner.in_flight.take().and_then(|flight| flight.waiter);
            inner.last = Some(snapshot.clone());
            waiter
        };
        if let Some(waiter) = waiter {
            // 调用方可能已超时并丢弃接收端；结果已写入缓存，发送失败无需处理。
            let _ = waiter.send(snapshot);
        }
        true
    }

    /// Forgets a request whose delivery failed.
    pub(crate) fn abandon(&self, request_id: &str) {
        let mut inner = lock(&self.inner);
        if inner
            .in_flight
            .as_ref()
            .is_some_and(|flight| flight.request_id == request_id)
        {
            inner.in_flight = None;
        }
    }

    pub(crate) fn last_or_empty(&self) -> ResourceUsageSnapshot {
        lock(&self.inner).last.clone().unwrap_or_default()
    }

    /// Ends the sampling session: drops the cache and returns the in-flight id
    /// so the caller can tell the Host to cancel it.
    pub(crate) fn cancel(&self) -> Option<String> {
        let mut inner = lock(&self.inner);
        inner.last = None;
        inner.in_flight.take().map(|flight| flight.request_id)
    }

    /// Drops every waiter; used when the Host exits.
    pub(crate) fn clear(&self) {
        let mut inner = lock(&self.inner);
        inner.in_flight = None;
        inner.last = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot(sampled_at: u64) -> ResourceUsageSnapshot {
        ResourceUsageSnapshot {
            sampled_at,
            processes: Vec::new(),
        }
    }

    fn started(correlator: &UsageCorrelator) -> (String, oneshot::Receiver<ResourceUsageSnapshot>) {
        match correlator.begin() {
            UsageBegin::Started {
                request_id,
                receiver,
            } => (request_id, receiver),
            UsageBegin::InFlight(_) => panic!("expected a new request"),
        }
    }

    #[test]
    fn resolves_the_matching_request_only() {
        let correlator = UsageCorrelator::default();
        let (id, mut receiver) = started(&correlator);
        assert!(!correlator.resolve("foreign", snapshot(1)));
        assert!(correlator.resolve(&id, snapshot(2)));
        assert_eq!(receiver.try_recv().unwrap().sampled_at, 2);
        assert!(
            !correlator.resolve(&id, snapshot(3)),
            "a second result is stale"
        );
    }

    #[test]
    fn in_flight_callers_reuse_the_last_result() {
        let correlator = UsageCorrelator::default();
        let (first, _receiver) = started(&correlator);
        assert!(correlator.resolve(&first, snapshot(7)));
        let (_second, receiver) = started(&correlator);
        drop(receiver);
        match correlator.begin() {
            UsageBegin::InFlight(last) => assert_eq!(last.sampled_at, 7),
            UsageBegin::Started { .. } => panic!("must not queue a second request"),
        }
    }

    #[test]
    fn late_results_refresh_the_cache_after_a_timeout() {
        let correlator = UsageCorrelator::default();
        let (id, receiver) = started(&correlator);
        drop(receiver);
        assert!(correlator.resolve(&id, snapshot(9)));
        assert_eq!(correlator.last_or_empty().sampled_at, 9);
    }

    #[test]
    fn cancel_and_abandon_release_the_slot() {
        let correlator = UsageCorrelator::default();
        let (id, _receiver) = started(&correlator);
        assert_eq!(correlator.cancel().as_deref(), Some(id.as_str()));
        assert!(correlator.cancel().is_none());
        let (id, _receiver) = started(&correlator);
        correlator.abandon(&id);
        let (_id, _receiver) = started(&correlator);
        correlator.clear();
        assert_eq!(correlator.last_or_empty(), ResourceUsageSnapshot::default());
    }
}
