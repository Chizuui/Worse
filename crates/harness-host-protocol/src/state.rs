//! Lifecycle state of the Node Host sidecar owned by one window.

/// Where the Host sidecar is in its start, ready, and failure lifecycle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostState {
    /// No sidecar has been spawned yet, or it was shut down.
    Stopped,
    /// The sidecar process was spawned and has not reported `ready` yet.
    Starting {
        /// Operating-system pid when the spawn already produced one.
        pid: Option<u32>,
    },
    /// The sidecar reported `ready` and serves attachments on `ws_port`.
    Ready {
        /// Operating-system pid of the sidecar process.
        pid: u32,
        /// Loopback port of the attachment WebSocket server.
        ws_port: u16,
    },
    /// The sidecar could not start or exited unexpectedly.
    Failed {
        /// Stable machine-readable failure code.
        code: String,
        /// Human-readable failure message.
        message: String,
    },
}

impl HostState {
    /// Returns whether the sidecar is ready to accept attachments.
    #[must_use]
    pub fn is_ready(&self) -> bool {
        matches!(self, Self::Ready { .. })
    }
}
