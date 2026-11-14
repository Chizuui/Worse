//! Applies the Phase 2 handling class to every Host message.

use std::sync::atomic::Ordering;
use std::sync::Arc;

use harness_host_protocol::{
    classify, parse_resource_usage_result, AnswerKind, HostEventKind, HostHandling, ResponseKind,
};
use serde_json::Value;

use crate::fallback;
use crate::shared::HostShared;

/// Routes one Host message according to [`classify`].
pub(crate) fn route_host_message(shared: &Arc<HostShared>, message: &Value) {
    let Some(kind) = message
        .get("type")
        .and_then(Value::as_str)
        .map(HostEventKind::from_wire)
    else {
        tracing::warn!("ignored a Host message without a type");
        return;
    };
    shared.sink().on_host_event(&kind, message);
    match classify(&kind) {
        HostHandling::StartupState => on_startup_state(shared, message),
        HostHandling::Log => log_host_message(message),
        HostHandling::Answer(answer) => answer_request(shared, answer, message),
        HostHandling::Response(ResponseKind::ResourceUsageSnapshot) => {
            on_resource_usage(shared, message);
        }
        HostHandling::Observe => tracing::debug!(kind = kind.as_wire(), "observed Host event"),
        HostHandling::Drop => {
            let dropped = shared.drop_count.fetch_add(1, Ordering::Relaxed) + 1;
            tracing::debug!(kind = kind.as_wire(), dropped, "dropped Host event");
        }
        HostHandling::Unsupported(reason) => {
            let count = shared.unsupported_count.fetch_add(1, Ordering::Relaxed) + 1;
            if shared.first_unsupported(kind.as_wire()) {
                tracing::warn!(
                    kind = kind.as_wire(),
                    reason,
                    count,
                    "unsupported Host event"
                );
            }
        }
    }
}

fn on_startup_state(shared: &HostShared, message: &Value) {
    let Some(state) = message.get("state") else {
        tracing::warn!("database-startup-state without a state object");
        return;
    };
    let accepted = shared.startup().accept(state);
    if accepted {
        shared.sink().on_startup_state(state);
    } else {
        tracing::debug!("ignored a stale or foreign database-startup-state");
    }
}

/// Resolves the shell's own `resource-usage-snapshot-request`; never replies.
fn on_resource_usage(shared: &HostShared, message: &Value) {
    match parse_resource_usage_result(message) {
        Ok((request_id, snapshot)) => {
            if !shared.usage().resolve(&request_id, snapshot) {
                tracing::debug!("ignored a stale resource-usage-snapshot-result");
            }
        }
        Err(error) => tracing::warn!(%error, "invalid resource-usage-snapshot-result"),
    }
}

fn log_host_message(message: &Value) {
    let text = message.get("message").and_then(Value::as_str).unwrap_or("");
    let source = message
        .get("source")
        .and_then(Value::as_str)
        .unwrap_or("host");
    match message.get("level").and_then(Value::as_str) {
        Some("error") => tracing::error!(target: "harness_host", source, "{text}"),
        Some("warn") => tracing::warn!(target: "harness_host", source, "{text}"),
        _ => tracing::info!(target: "harness_host", source, "{text}"),
    }
}

/// Sends the fallback reply without blocking the reader on stdin.
fn answer_request(shared: &Arc<HostShared>, answer: AnswerKind, message: &Value) {
    let sequence = shared.claim_sequence.fetch_add(1, Ordering::Relaxed);
    let Some(reply) = fallback::build(answer, message, &shared.claim_prefix, sequence) else {
        tracing::warn!(
            ?answer,
            "Host request without a correlation id; no reply sent"
        );
        return;
    };
    let shared = Arc::clone(shared);
    // 回复在独立任务中写入 stdin：读循环若在写入时阻塞，而 sidecar 同时阻塞在写 stdout，会互相等待。
    tokio::spawn(async move {
        if let Err(error) = shared.send_host(reply).await {
            tracing::warn!(?answer, %error, "failed to send a Host fallback reply");
        }
    });
}
