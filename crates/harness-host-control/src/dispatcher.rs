//! Routes sidecar NDJSON lines after the `ready` handshake.

use std::sync::Arc;

use harness_host_protocol::{
    AttachActiveLine, AttachMessageKind, HostState, LogLevel, SidecarLine,
};
use harness_sidecar::SidecarStdout;
use tokio::task::JoinHandle;

use crate::error::HostControlError;
use crate::routing;
use crate::shared::HostShared;

/// Parses and routes one raw stdout line. Malformed lines are logged and
/// skipped; the dispatcher never panics on sidecar output.
pub(crate) fn dispatch_line(shared: &Arc<HostShared>, raw: &str) {
    match SidecarLine::parse(raw) {
        Ok(line) => dispatch(shared, line),
        // 只记录错误类别与长度，不回显原始行，避免把 Host 载荷写进日志。
        Err(error) => tracing::warn!(%error, bytes = raw.len(), "ignored a malformed sidecar line"),
    }
}

fn dispatch(shared: &Arc<HostShared>, line: SidecarLine) {
    match line {
        SidecarLine::Ready(_) => {
            tracing::warn!("ignored a duplicate sidecar ready line");
        }
        SidecarLine::AttachOpened(opened) => {
            if let Some(sender) = shared.take_pending(&opened.request_id) {
                let _ = sender.send(Ok(()));
            } else {
                tracing::warn!(
                    attachment = %opened.attachment_id,
                    "attach.opened without a pending request"
                );
            }
        }
        SidecarLine::AttachFailed(failed) => {
            let error = HostControlError::Protocol(format!(
                "attachment rejected ({}): {}",
                failed.code.as_str(),
                failed.message
            ));
            if let Some(sender) = shared.take_pending(&failed.request_id) {
                let _ = sender.send(Err(error));
            } else {
                tracing::warn!(%error, "attach.failed without a pending request");
            }
        }
        SidecarLine::AttachActive(active) => on_attach_active(shared, &active),
        SidecarLine::AttachClosed(closed) => {
            tracing::info!(
                attachment = %closed.attachment_id,
                reason = ?closed.reason,
                "Host attachment closed"
            );
        }
        SidecarLine::Host(host) => routing::route_host_message(shared, &host.message),
        SidecarLine::Fatal(fatal) => {
            tracing::error!(code = fatal.code.as_str(), message = %fatal.message, "Host sidecar fatal");
            shared.set_state(&HostState::Failed {
                code: fatal.code.as_str().to_owned(),
                message: fatal.message,
            });
        }
        SidecarLine::Log(log) => match log.level {
            LogLevel::Info => tracing::info!(target: "harness_host_sidecar", "{}", log.message),
            LogLevel::Warn => tracing::warn!(target: "harness_host_sidecar", "{}", log.message),
            LogLevel::Error => tracing::error!(target: "harness_host_sidecar", "{}", log.message),
        },
        SidecarLine::Pong(pong) => tracing::debug!(id = %pong.id, "sidecar pong"),
    }
}

fn on_attach_active(shared: &HostShared, active: &AttachActiveLine) {
    let was_initialized = shared.mark_initialized();
    if was_initialized && active.message_type == AttachMessageKind::InitLocal {
        tracing::warn!(
            attachment = %active.attachment_id,
            "init-local became active on an already initialized Host"
        );
    }
    tracing::info!(
        attachment = %active.attachment_id,
        message_type = ?active.message_type,
        "Host attachment active"
    );
}

/// Runs the post-handshake reader until the sidecar stdout closes.
pub(crate) fn spawn_reader(shared: Arc<HostShared>, mut stdout: SidecarStdout) -> JoinHandle<()> {
    tokio::spawn(async move {
        let reason = loop {
            match stdout.next_line().await {
                Ok(Some(line)) => dispatch_line(&shared, &line),
                Ok(None) => break "sidecar stdout closed".to_owned(),
                Err(error) => break format!("failed to read sidecar stdout: {error}"),
            }
        };
        on_stream_end(&shared, &reason);
    })
}

/// Settles pending work once the sidecar can no longer answer.
fn on_stream_end(shared: &HostShared, reason: &str) {
    shared.fail_pending(&HostControlError::NotRunning);
    let failed = shared.startup().transport_closed();
    if let Some(state) = failed {
        shared.sink().on_startup_state(&state);
    }
    if shared.is_shutting_down() {
        tracing::info!(reason, "Host sidecar stream ended during shutdown");
        return;
    }
    tracing::error!(reason, "Host sidecar exited unexpectedly");
    if !matches!(shared.state(), HostState::Failed { .. }) {
        shared.set_state(&HostState::Failed {
            code: "host_exited".to_owned(),
            message: reason.to_owned(),
        });
    }
}
