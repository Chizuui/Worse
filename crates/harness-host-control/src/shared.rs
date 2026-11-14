//! State shared between the controller and the sidecar dispatcher.
//!
//! Everything here is either process lifecycle, request correlation, or the
//! startup cache. No conversation, queue, or interaction state lives in Rust.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};

use harness_host_protocol::{HostState, ShellLine};
use harness_sidecar::SidecarStdin;
use serde_json::Value;
use tokio::sync::oneshot;
use uuid::Uuid;

use crate::error::HostControlError;
use crate::sink::HostSink;
use crate::startup::StartupCache;
use crate::usage::UsageCorrelator;

/// Outcome delivered to a pending `attach.open` request.
pub(crate) type AttachOutcome = Result<(), HostControlError>;

/// Locks a standard mutex, recovering the guard when a previous holder panicked.
pub(crate) fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Mutable state owned by the controller and observed by the dispatcher.
pub(crate) struct HostShared {
    stdin: SidecarStdin,
    sink: Arc<dyn HostSink>,
    state: Mutex<HostState>,
    startup: Mutex<StartupCache>,
    pending: Mutex<HashMap<String, oneshot::Sender<AttachOutcome>>>,
    usage: UsageCorrelator,
    unsupported_seen: Mutex<HashSet<String>>,
    /// Loopback port reported by `ready`; unset until the handshake completes.
    ws_port: OnceLock<u16>,
    /// Shell-local claim prefix; the Host never sees it as a `hostId`.
    pub(crate) claim_prefix: String,
    pub(crate) claim_sequence: AtomicU64,
    pub(crate) drop_count: AtomicU64,
    pub(crate) unsupported_count: AtomicU64,
    initialized: AtomicBool,
    shutting_down: AtomicBool,
}

impl HostShared {
    pub(crate) fn new(stdin: SidecarStdin, sink: Arc<dyn HostSink>, startup_id: String) -> Self {
        Self {
            stdin,
            sink,
            state: Mutex::new(HostState::Starting { pid: None }),
            startup: Mutex::new(StartupCache::new(startup_id)),
            pending: Mutex::new(HashMap::new()),
            usage: UsageCorrelator::default(),
            unsupported_seen: Mutex::new(HashSet::new()),
            ws_port: OnceLock::new(),
            claim_prefix: format!("harness-{}", Uuid::new_v4()),
            claim_sequence: AtomicU64::new(0),
            drop_count: AtomicU64::new(0),
            unsupported_count: AtomicU64::new(0),
            initialized: AtomicBool::new(false),
            shutting_down: AtomicBool::new(false),
        }
    }

    pub(crate) fn sink(&self) -> &dyn HostSink {
        self.sink.as_ref()
    }

    pub(crate) fn state(&self) -> HostState {
        lock(&self.state).clone()
    }

    /// Stores and publishes a lifecycle transition.
    pub(crate) fn set_state(&self, next: &HostState) {
        {
            *lock(&self.state) = next.clone();
        }
        self.sink.on_state(next);
    }

    pub(crate) fn set_ws_port(&self, port: u16) {
        // The handshake runs once; a second `ready` is rejected by the dispatcher.
        let _ = self.ws_port.set(port);
    }

    pub(crate) fn ws_port(&self) -> Option<u16> {
        self.ws_port.get().copied()
    }

    pub(crate) fn startup(&self) -> MutexGuard<'_, StartupCache> {
        lock(&self.startup)
    }

    pub(crate) fn usage(&self) -> &UsageCorrelator {
        &self.usage
    }

    pub(crate) fn is_initialized(&self) -> bool {
        self.initialized.load(Ordering::Acquire)
    }

    /// Marks the Host as initialized; returns whether it already was.
    pub(crate) fn mark_initialized(&self) -> bool {
        self.initialized.swap(true, Ordering::AcqRel)
    }

    pub(crate) fn is_shutting_down(&self) -> bool {
        self.shutting_down.load(Ordering::Acquire)
    }

    /// Enters the shutdown barrier; returns whether it was already entered.
    pub(crate) fn begin_shutdown(&self) -> bool {
        self.shutting_down.swap(true, Ordering::AcqRel)
    }

    /// Returns `true` the first time `kind` is reported as unsupported.
    pub(crate) fn first_unsupported(&self, kind: &str) -> bool {
        lock(&self.unsupported_seen).insert(kind.to_owned())
    }

    pub(crate) fn insert_pending(
        &self,
        request_id: String,
        sender: oneshot::Sender<AttachOutcome>,
    ) {
        lock(&self.pending).insert(request_id, sender);
    }

    pub(crate) fn take_pending(&self, request_id: &str) -> Option<oneshot::Sender<AttachOutcome>> {
        lock(&self.pending).remove(request_id)
    }

    /// Resolves every pending attachment request with `error` and drops the
    /// resource-usage waiter (its caller then sees [`HostControlError::NotRunning`]).
    pub(crate) fn fail_pending(&self, error: &HostControlError) {
        self.usage.clear();
        let senders = {
            let mut pending = lock(&self.pending);
            pending
                .drain()
                .map(|(_, sender)| sender)
                .collect::<Vec<_>>()
        };
        for sender in senders {
            let _ = sender.send(Err(error.clone()));
        }
    }

    /// Writes one shell line to the sidecar stdin.
    pub(crate) async fn send(&self, line: &ShellLine) -> Result<(), HostControlError> {
        let encoded = line
            .to_line()
            .map_err(|error| HostControlError::Protocol(error.to_string()))?;
        self.stdin
            .write_line(&encoded)
            .await
            .map_err(|error| HostControlError::Io(error.to_string()))
    }

    /// Wraps `message` in a `host` envelope and writes it to the sidecar.
    pub(crate) async fn send_host(&self, message: Value) -> Result<(), HostControlError> {
        self.send(&ShellLine::host(message)).await
    }
}
