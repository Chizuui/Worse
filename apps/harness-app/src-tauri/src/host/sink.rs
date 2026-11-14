//! Forwards Host lifecycle changes to the webview as Tauri events.

use harness_host_control::{HostControlError, HostSink};
use harness_host_protocol::{HostEventKind, HostState};
use harness_protocol::{CommandError, ErrorCode, HostStateDto, HostStatus};
use serde_json::Value;
use tauri::{AppHandle, Emitter};

/// Tauri event carrying the latest `database-startup-state` object.
pub(crate) const HOST_DATABASE_STARTUP_EVENT: &str = "harness://host/database-startup";

/// Tauri event carrying the local Host [`HostStatus`].
pub(crate) const HOST_STATE_EVENT: &str = "harness://host/state";

/// Emits events only; the controller owns the startup cache.
pub(crate) struct ShellHostSink {
    app: AppHandle,
}

impl ShellHostSink {
    pub(crate) fn new(app: AppHandle) -> Self {
        Self { app }
    }
}

impl HostSink for ShellHostSink {
    fn on_state(&self, state: &HostState) {
        emit_status(&self.app, &status_from_state(state));
    }

    fn on_startup_state(&self, state: &Value) {
        emit_database_startup(&self.app, state);
    }

    fn on_host_event(&self, kind: &HostEventKind, message: &Value) {
        // 载荷可能含用户数据，只记录事件类型。
        tracing::trace!(kind = kind.as_wire(), "local Host event");
        // Phase 4：host-node 远控 owner 的结果/事件经 Host 消息通道回到 shell，交给 remote_control 转发。
        if kind
            .as_wire()
            .starts_with(harness_host_protocol::WEB_REMOTE_MESSAGE_PREFIX)
        {
            crate::remote_control::on_host_message(&self.app, message);
            // Phase 5：插件沙箱 owner 的 fetch/dispose 结果复用同一前缀。
            crate::plugin_sandbox::on_host_message(&self.app, message);
        }
    }
}

pub(crate) fn emit_status(app: &AppHandle, status: &HostStatus) {
    if let Err(error) = app.emit(HOST_STATE_EVENT, status) {
        tracing::warn!(%error, "failed to emit the local Host state");
    }
}

pub(crate) fn emit_database_startup(app: &AppHandle, state: &Value) {
    if let Err(error) = app.emit(HOST_DATABASE_STARTUP_EVENT, state) {
        tracing::warn!(%error, "failed to emit the database startup state");
    }
}

pub(crate) fn status(state: HostStateDto) -> HostStatus {
    HostStatus {
        state,
        error_code: None,
        error_message: None,
    }
}

pub(crate) fn status_from_state(state: &HostState) -> HostStatus {
    match state {
        HostState::Stopped => status(HostStateDto::Stopped),
        HostState::Starting { .. } => status(HostStateDto::Starting),
        HostState::Ready { .. } => status(HostStateDto::Ready),
        HostState::Failed { code, message } => HostStatus {
            state: HostStateDto::Failed,
            error_code: Some(code.clone()),
            error_message: Some(message.clone()),
        },
    }
}

/// Maps a controller failure to the typed command error.
pub(crate) fn control_error(error: &HostControlError) -> CommandError {
    let code = match error {
        HostControlError::Spawn(_) | HostControlError::AlreadyRunning => ErrorCode::HostSpawnFailed,
        HostControlError::Protocol(_) => ErrorCode::HostProtocolError,
        HostControlError::Timeout(_) => ErrorCode::HostTimeout,
        HostControlError::NotRunning => ErrorCode::HostNotStarted,
        HostControlError::Io(_) | HostControlError::Shutdown(_) => ErrorCode::Internal,
    };
    crate::error::host(code, error.to_string())
}
