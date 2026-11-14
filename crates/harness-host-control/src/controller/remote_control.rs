//! Shell-initiated web remote-control calls (Phase 4).
//!
//! The vendored `ZCode` web remote-control manager runs inside the host-node
//! sidecar. The shell reaches it through `host` envelopes whose `type` starts
//! with [`WEB_REMOTE_MESSAGE_PREFIX`]; the sidecar intercepts them and never
//! forwards them to the `ZCode` Host. Replies come back as Host messages of the
//! same prefix and are routed to the sink (`HostSink::on_host_event`).

use harness_host_protocol::WEB_REMOTE_MESSAGE_PREFIX;
use serde_json::Value;

use super::HostController;
use crate::error::HostControlError;

impl HostController {
    /// Sends one `harness-web-remote-*` message to the sidecar.
    ///
    /// Only messages of the reserved prefix are accepted, so this cannot be
    /// used to inject arbitrary Host traffic.
    ///
    /// # Errors
    ///
    /// Returns [`HostControlError::Protocol`] when `message.type` lacks the
    /// prefix, [`HostControlError::NotRunning`] when the Host is not ready,
    /// and [`HostControlError::Io`] when stdin is closed.
    pub async fn send_web_remote(&self, message: Value) -> Result<(), HostControlError> {
        let kind = message
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if !kind.starts_with(WEB_REMOTE_MESSAGE_PREFIX) {
            return Err(HostControlError::Protocol(format!(
                "web remote-control message type must start with {WEB_REMOTE_MESSAGE_PREFIX}"
            )));
        }
        self.running_port()?;
        self.shared.send_host(message).await
    }
}
