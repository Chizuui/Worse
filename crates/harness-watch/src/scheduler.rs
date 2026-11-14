//! The single deadline thread shared by every subscription.
//!
//! Waits are event-driven (`recv_timeout` until the earliest deadline), never
//! sleep polling: on Windows a short `sleep` can last a whole 15.6 ms tick.

use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::sync::{mpsc, Weak};
use std::time::Instant;

use notify::RecommendedWatcher;

use crate::{flush_if_due, lock, HubInner};

pub(crate) enum SchedMsg {
    /// Re-check `id` at `deadline`.
    Schedule(String, Instant),
    /// Drop a failed subscription's watcher off the notify thread.
    Release(String, Option<RecommendedWatcher>),
    /// Hub dropped.
    Stop,
}

pub(crate) fn spawn(hub: Weak<HubInner>, rx: mpsc::Receiver<SchedMsg>) {
    let spawned = std::thread::Builder::new()
        .name("harness-watch".to_owned())
        .spawn(move || run(&hub, &rx));
    if let Err(error) = spawned {
        tracing::error!(%error, "failed to start watch scheduler thread");
    }
}

fn run(hub: &Weak<HubInner>, rx: &mpsc::Receiver<SchedMsg>) {
    let mut heap: BinaryHeap<Reverse<(Instant, String)>> = BinaryHeap::new();
    loop {
        let message = match heap.peek() {
            None => match rx.recv() {
                Ok(message) => Some(message),
                Err(_) => return,
            },
            Some(Reverse((deadline, _))) => {
                let wait = deadline.saturating_duration_since(Instant::now());
                match rx.recv_timeout(wait) {
                    Ok(message) => Some(message),
                    Err(mpsc::RecvTimeoutError::Timeout) => None,
                    Err(mpsc::RecvTimeoutError::Disconnected) => return,
                }
            }
        };
        match message {
            Some(SchedMsg::Stop) => return,
            Some(SchedMsg::Release(id, watcher)) => {
                drop(watcher);
                tracing::debug!(id, "released failed watcher");
            }
            Some(SchedMsg::Schedule(id, deadline)) => heap.push(Reverse((deadline, id))),
            None => {}
        }
        let Some(hub) = hub.upgrade() else { return };
        let now = Instant::now();
        while let Some(Reverse((deadline, _))) = heap.peek() {
            if *deadline > now {
                break;
            }
            let Some(Reverse((_, id))) = heap.pop() else {
                break;
            };
            let sub = lock(&hub.subs).get(&id).cloned();
            if let Some(sub) = sub {
                if let Some(next) = flush_if_due(&hub, &sub, now) {
                    heap.push(Reverse((next, id)));
                }
            }
        }
    }
}
