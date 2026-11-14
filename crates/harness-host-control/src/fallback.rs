//! Exact fallback replies for Host requests that Harness Phase 2 cannot serve.

use harness_host_protocol::AnswerKind;
use serde_json::{json, Value};

/// Builds the reply for one classified request, or `None` when the request
/// carries no correlation id and no reply can be addressed.
pub(crate) fn build(
    kind: AnswerKind,
    message: &Value,
    host_id: &str,
    claim_sequence: u64,
) -> Option<Value> {
    match kind {
        AnswerKind::BroadcastClaim => {
            let request_id = request_id(message)?;
            let claim_token = format!("{host_id}:{request_id}:{claim_sequence}");
            Some(json!({
                "type": "broadcast-claim-result",
                "requestId": request_id,
                "status": "acquired",
                "claimToken": claim_token,
            }))
        }
        AnswerKind::TaskRunLease => {
            let lease_request_id = message
                .get("request")
                .and_then(|request| request.get("leaseRequestId"))
                .and_then(Value::as_str)?;
            Some(json!({
                "type": "task-run-lease-result",
                "result": {
                    "leaseRequestId": lease_request_id,
                    "acquired": false,
                    "ownerHostId": host_id,
                    "reason": "owned_by_other_host",
                },
            }))
        }
        AnswerKind::TaskOwnerCommand => {
            let command_request_id = message
                .get("command")
                .and_then(|command| command.get("commandRequestId"))
                .and_then(Value::as_str)?;
            Some(json!({
                "type": "task-owner-command-result",
                "result": {
                    "commandRequestId": command_request_id,
                    "success": false,
                    "error": "unsupported",
                },
            }))
        }
        AnswerKind::FeedbackArchive => {
            let request_id = request_id(message)?;
            Some(json!({
                "type": "feedback-log-archive-result",
                "requestId": request_id,
                "ok": false,
                "error": "log archive is not available in Harness Phase 2",
            }))
        }
        AnswerKind::BrowserExecute => {
            let request_id = request_id(message)?;
            Some(json!({
                "type": "browser-execute-result",
                "requestId": request_id,
                "result": {
                    "ok": false,
                    "error": {
                        "code": "backend_unavailable",
                        "message": "browser executor is not available in Harness Phase 2",
                    },
                    "elapsedMs": 0,
                },
            }))
        }
        AnswerKind::MediaPreviewAuthorize => {
            let request_id = request_id(message)?;
            Some(json!({
                "type": "local-media-preview-path-authorize-result",
                "requestId": request_id,
                "ok": false,
                "error": "local media preview is not available in Harness Phase 2",
            }))
        }
        AnswerKind::PluginSandboxRegister => {
            let request_id = request_id(message)?;
            Some(json!({
                "type": "plugin-sandbox-register-result",
                "requestId": request_id,
                "ok": false,
                "error": "plugin UI sandbox is not available in Harness Phase 2",
            }))
        }
        AnswerKind::ProviderProvisioningLane => {
            let request_id = request_id(message)?;
            Some(json!({
                "type": "provider-provisioning-lane-result",
                "requestId": request_id,
                "status": { "lane": "none" },
            }))
        }
    }
}

fn request_id(message: &Value) -> Option<&str> {
    message.get("requestId").and_then(Value::as_str)
}

#[cfg(test)]
mod tests {
    use harness_host_protocol::AnswerKind;
    use serde_json::{json, Value};

    use super::build;

    fn message(request_id: &str) -> Value {
        json!({ "type": "request", "requestId": request_id })
    }

    #[test]
    fn media_preview_authorization_is_rejected() {
        let reply = build(
            AnswerKind::MediaPreviewAuthorize,
            &message("req-1"),
            "host-1",
            0,
        );
        assert_eq!(
            reply,
            Some(json!({
                "type": "local-media-preview-path-authorize-result",
                "requestId": "req-1",
                "ok": false,
                "error": "local media preview is not available in Harness Phase 2",
            }))
        );
    }

    #[test]
    fn browser_execution_reports_backend_unavailable() {
        let reply = build(AnswerKind::BrowserExecute, &message("req-2"), "host-1", 0);
        assert_eq!(
            reply,
            Some(json!({
                "type": "browser-execute-result",
                "requestId": "req-2",
                "result": {
                    "ok": false,
                    "error": {
                        "code": "backend_unavailable",
                        "message": "browser executor is not available in Harness Phase 2",
                    },
                    "elapsedMs": 0,
                },
            }))
        );
    }

    #[test]
    fn plugin_sandbox_and_feedback_are_rejected() {
        assert_eq!(
            build(
                AnswerKind::PluginSandboxRegister,
                &message("req-3"),
                "host-1",
                0,
            ),
            Some(json!({
                "type": "plugin-sandbox-register-result",
                "requestId": "req-3",
                "ok": false,
                "error": "plugin UI sandbox is not available in Harness Phase 2",
            }))
        );
        assert_eq!(
            build(AnswerKind::FeedbackArchive, &message("req-4"), "host-1", 0),
            Some(json!({
                "type": "feedback-log-archive-result",
                "requestId": "req-4",
                "ok": false,
                "error": "log archive is not available in Harness Phase 2",
            }))
        );
    }

    #[test]
    fn provisioning_lane_is_empty() {
        assert_eq!(
            build(
                AnswerKind::ProviderProvisioningLane,
                &message("req-5"),
                "host-1",
                0,
            ),
            Some(json!({
                "type": "provider-provisioning-lane-result",
                "requestId": "req-5",
                "status": { "lane": "none" },
            }))
        );
    }

    #[test]
    fn broadcast_claim_token_uses_the_shell_prefixed_sequence() {
        let reply = build(AnswerKind::BroadcastClaim, &message("req-6"), "host-1", 42);
        assert_eq!(
            reply,
            Some(json!({
                "type": "broadcast-claim-result",
                "requestId": "req-6",
                "status": "acquired",
                "claimToken": "host-1:req-6:42",
            }))
        );
    }

    #[test]
    fn task_lease_and_owner_command_use_their_nested_ids() {
        let lease = json!({
            "type": "task-run-lease-acquire",
            "request": { "leaseRequestId": "lease-1" },
        });
        assert_eq!(
            build(AnswerKind::TaskRunLease, &lease, "host-1", 0),
            Some(json!({
                "type": "task-run-lease-result",
                "result": {
                    "leaseRequestId": "lease-1",
                    "acquired": false,
                    "ownerHostId": "host-1",
                    "reason": "owned_by_other_host",
                },
            }))
        );

        let command = json!({
            "type": "task-owner-command-request",
            "command": { "commandRequestId": "cmd-1" },
        });
        assert_eq!(
            build(AnswerKind::TaskOwnerCommand, &command, "host-1", 0),
            Some(json!({
                "type": "task-owner-command-result",
                "result": {
                    "commandRequestId": "cmd-1",
                    "success": false,
                    "error": "unsupported",
                },
            }))
        );
    }

    #[test]
    fn requests_without_a_correlation_id_are_not_answered() {
        let unroutable = json!({ "type": "local-media-preview-path-authorize-request" });
        assert_eq!(
            build(AnswerKind::MediaPreviewAuthorize, &unroutable, "host-1", 0),
            None
        );
    }
}
