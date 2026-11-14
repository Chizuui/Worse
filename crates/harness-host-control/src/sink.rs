//! Notification surface between the controller and the Tauri shell.

use harness_host_protocol::{HostEventKind, HostState};
use serde_json::Value;

/// Receives lifecycle and Host event notifications.
///
/// Implementors must not block: the dispatcher calls these methods from the
/// sidecar reader task.
pub trait HostSink: Send + Sync + 'static {
    /// Reports a Host lifecycle transition.
    fn on_state(&self, state: &HostState);

    /// Reports the latest `database-startup-state` payload (`state` object).
    fn on_startup_state(&self, state: &Value);

    /// Reports every Host message before it is classified.
    fn on_host_event(&self, kind: &HostEventKind, message: &Value);
}
