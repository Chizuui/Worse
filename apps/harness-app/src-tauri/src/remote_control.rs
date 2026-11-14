//! Phase 4 web remote control: the shell side of the host-node channel.
//!
//! The vendored `ZCode` web remote-control manager (relay connection, pairing,
//! bridge forwarding, phone Host attachments) runs inside the host-node
//! sidecar; see `Worse/docs/specs/harness/phase-4-web-remote-control.md`.
//! This module only forwards allowlisted calls to it, correlates the replies,
//! and re-emits its events to the webview. It holds no task, queue, snapshot,
//! or relay state: the pending map lives only while one call is in flight.

#![allow(clippy::needless_pass_by_value)]

#[cfg(any(target_os = "android", target_os = "ios"))]
use harness_protocol::CommandError;

#[cfg(not(any(target_os = "android", target_os = "ios")))]
// 通配导入：tauri::command 生成的隐藏宏（__cmd__*）也要一并导出，generate_handler! 才能找到。
pub(crate) use desktop::*;

/// Mobile shells are remote clients only: they own no Host and therefore no
/// remote-control manager. Pairing on mobile uses the web remote page.
#[cfg(any(target_os = "android", target_os = "ios"))]
#[tauri::command]
#[allow(clippy::unused_async)]
pub(crate) async fn remote_control_call(method: String) -> Result<(), CommandError> {
    Err(crate::error::unsupported(format!(
        "web remote control `{method}` needs a desktop Host; this device is a remote client"
    )))
}

#[cfg(not(any(target_os = "android", target_os = "ios")))]
mod desktop {
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Mutex, MutexGuard, PoisonError};
    use std::time::Duration;

    use harness_host_control::HostControlError;
    use harness_protocol::{CommandError, ErrorCode};
    use serde_json::{json, Value};
    use tauri::{AppHandle, Emitter, Manager, State};
    use tokio::sync::oneshot;

    use crate::state::AppState;

    const CALL: &str = "harness-web-remote-call";
    const RESULT: &str = "harness-web-remote-result";
    const EVENT: &str = "harness-web-remote-event";

    /// Upper bound for one call. `start` registers the device on the relay and
    /// may wait for the network; the manager applies its own shorter timeouts.
    const CALL_TIMEOUT: Duration = Duration::from_secs(90);

    /// Methods the renderer may invoke. Diagnostics such as
    /// `getAttachmentStats` stay reserved for the gate.
    const METHODS: &[&str] = &[
        "start",
        "stop",
        "getStatus",
        "notifyAuthStateChanged",
        "syncWorkspaces",
        "reconnectResult",
        "rendererActionResult",
        // Phase 4 fix B: Main platform requests of the desktop renderer (temp text
        // attachments, MCP user directory). The sidecar restricts `params.method`.
        "platformRequest",
    ];

    /// Sidecar event name → Tauri event name.
    const EVENTS: &[(&str, &str)] = &[
        ("status", "harness://web-remote/status"),
        ("reconnectRequest", "harness://web-remote/reconnect-request"),
        (
            "rendererActionRequest",
            "harness://web-remote/renderer-action-request",
        ),
        ("attachment", "harness://web-remote/attachment"),
    ];

    type Reply = Result<Value, String>;

    /// In-flight call correlation only; entries are removed on reply,
    /// send failure, or timeout.
    #[derive(Default)]
    pub(crate) struct WebRemoteCalls {
        next: AtomicU64,
        pending: Mutex<HashMap<String, oneshot::Sender<Reply>>>,
    }

    impl WebRemoteCalls {
        fn pending(&self) -> MutexGuard<'_, HashMap<String, oneshot::Sender<Reply>>> {
            self.pending.lock().unwrap_or_else(PoisonError::into_inner)
        }

        fn begin(&self) -> (String, oneshot::Receiver<Reply>) {
            let id = format!("shell-{}", self.next.fetch_add(1, Ordering::Relaxed) + 1);
            let (sender, receiver) = oneshot::channel();
            self.pending().insert(id.clone(), sender);
            (id, receiver)
        }

        fn abandon(&self, id: &str) {
            self.pending().remove(id);
        }

        fn settle(&self, message: &Value) {
            let Some(id) = message.get("id").and_then(Value::as_str) else {
                return;
            };
            let Some(sender) = self.pending().remove(id) else {
                // 调用已超时被放弃，迟到的结果直接丢弃。
                return;
            };
            let reply = if message.get("ok").and_then(Value::as_bool) == Some(true) {
                Ok(message.get("value").cloned().unwrap_or(Value::Null))
            } else {
                Err(message
                    .get("error")
                    .and_then(Value::as_str)
                    .unwrap_or("web remote control call failed")
                    .to_owned())
            };
            // The caller may already have timed out; dropping the reply is fine.
            let _ignored = sender.send(reply);
        }
    }

    /// Routes one `harness-web-remote-*` Host message. Called by the Host sink
    /// for every Host message; other types are ignored.
    pub(crate) fn on_host_message(app: &AppHandle, message: &Value) {
        match message.get("type").and_then(Value::as_str) {
            Some(RESULT) => {
                if let Some(calls) = app.try_state::<WebRemoteCalls>() {
                    calls.settle(message);
                }
            }
            Some(EVENT) => {
                let event = message.get("event").and_then(Value::as_str);
                let Some((_, name)) = EVENTS.iter().find(|(wire, _)| Some(*wire) == event) else {
                    tracing::debug!(?event, "ignored unknown web remote-control event");
                    return;
                };
                let payload = message.get("payload").cloned().unwrap_or(Value::Null);
                if let Err(error) = app.emit(name, payload) {
                    tracing::warn!(%error, event = name, "failed to emit web remote-control event");
                }
            }
            _ => {}
        }
    }

    /// Forwards one allowlisted call to the sidecar's remote-control owner.
    ///
    /// Fails closed: without a running Host the call errors instead of
    /// spawning one, and unknown methods are rejected before sending.
    #[tauri::command]
    pub(crate) async fn remote_control_call(
        app: AppHandle,
        state: State<'_, AppState>,
        calls: State<'_, WebRemoteCalls>,
        method: String,
        params: Option<Value>,
    ) -> Result<Value, CommandError> {
        if !METHODS.contains(&method.as_str()) {
            return Err(crate::error::invalid_input(format!(
                "unknown web remote-control method `{method}`"
            )));
        }
        let controller = state.host().running_controller().ok_or_else(|| {
            crate::error::host(ErrorCode::HostNotStarted, "the local Host is not running")
        })?;
        let (id, receiver) = calls.begin();
        let message = json!({
            "type": CALL,
            "id": id,
            "method": method,
            "params": params.unwrap_or_else(|| json!({})),
            "appVersion": app.package_info().version.to_string(),
        });
        if let Err(error) = controller.send_web_remote(message).await {
            calls.abandon(&id);
            let code = if matches!(error, HostControlError::NotRunning) {
                ErrorCode::HostNotStarted
            } else {
                ErrorCode::HostProtocolError
            };
            return Err(crate::error::host(code, error.to_string()));
        }
        match tokio::time::timeout(CALL_TIMEOUT, receiver).await {
            Ok(Ok(Ok(value))) => Ok(value),
            Ok(Ok(Err(message))) => Err(crate::error::internal(message)),
            Ok(Err(_dropped)) => Err(crate::error::host(
                ErrorCode::HostNotStarted,
                "the local Host stopped during the call",
            )),
            Err(_elapsed) => {
                calls.abandon(&id);
                Err(crate::error::host(
                    ErrorCode::HostTimeout,
                    format!("web remote-control `{method}` timed out"),
                ))
            }
        }
    }
}
