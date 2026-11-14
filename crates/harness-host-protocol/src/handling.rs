//! Phase 2 handling class for every `HostResponseTypes` value.
//!
//! The mapping follows the catalogue table in
//! `Worse/docs/specs/harness/phase-2-host-sidecar.md` section 8.1:
//!
//! - `database-startup-state` is the only tracked message (`StartupState`).
//! - `log` is the Host log channel (`Log`).
//! - Host lifecycle and bookkeeping events are observed, logged, and exposed
//!   to the sink without a reply (`Observe`).
//! - Telemetry, single-window broadcasts, schedulers, and remote-workspace
//!   traffic are discarded (`Drop`).
//! - Requests that would otherwise wait forever receive a typed fallback
//!   (`Answer`).
//! - Replies to shell-initiated requests are correlated by `requestId`
//!   (`Response`); `resource-usage-snapshot-result` answers the shell's own
//!   `resource-usage-snapshot-request`, so the shell never replies to it.
//! - Remote and project-workspace messages cannot occur in Phase 2 and are
//!   counted once per kind (`Unsupported`).
//!
//! Claim token bookkeeping for `broadcast-claim-commit` and
//! `broadcast-claim-release` stays in the dispatcher; this crate only reports
//! that both are observed rather than dropped.

use crate::HostEventKind;

/// Prefix of the private shell <-> host-node web remote-control channel
/// (`harness-web-remote-call` / `-result` / `-event`). The sidecar intercepts
/// these messages; they never reach the `ZCode` Host. See
/// `Worse/docs/specs/harness/phase-4-web-remote-control.md`.
pub const WEB_REMOTE_MESSAGE_PREFIX: &str = "harness-web-remote-";

/// How the Phase 2 dispatcher handles a Host message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostHandling {
    /// Cache the state and forward it to the webview.
    StartupState,
    /// Log the payload; the shell owns no state and sends no reply.
    Log,
    /// Reply with the documented Phase 2 fallback.
    Answer(AnswerKind),
    /// Resolve the matching shell-initiated request; no reply.
    Response(ResponseKind),
    /// Log and expose the message to the sink; no reply, no state.
    Observe,
    /// Discard the payload and count it for diagnostics.
    Drop,
    /// Not available in Phase 2; count and warn once per kind.
    Unsupported(&'static str),
}

/// The fallback reply selected by [`classify`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnswerKind {
    /// `broadcast-claim-request` answered with an acquired claim token.
    BroadcastClaim,
    /// `task-run-lease-acquire` answered with `acquired: false`.
    TaskRunLease,
    /// `task-owner-command-request` answered with `ok: false`.
    TaskOwnerCommand,
    /// `feedback-log-archive-request` answered as unavailable.
    FeedbackArchive,
    /// `browser-execute-request` answered as unavailable.
    BrowserExecute,
    /// `local-media-preview-path-authorize-request` answered as unavailable.
    MediaPreviewAuthorize,
    /// `plugin-sandbox-register-request` answered as unavailable.
    PluginSandboxRegister,
    /// `provider-provisioning-lane-request` answered with lane `none`.
    ProviderProvisioningLane,
}

/// A Host reply to a request the shell originated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResponseKind {
    /// `resource-usage-snapshot-result` for `resource-usage-snapshot-request`.
    ResourceUsageSnapshot,
}

/// Classifies one Host message for the Phase 2 dispatcher.
#[must_use]
pub fn classify(kind: &HostEventKind) -> HostHandling {
    match kind {
        HostEventKind::DatabaseStartupState => HostHandling::StartupState,
        HostEventKind::Log => HostHandling::Log,
        HostEventKind::AgentProcessSpawned
        | HostEventKind::AgentProcessReady
        | HostEventKind::AgentProcessExited
        | HostEventKind::AgentProcessError
        | HostEventKind::AgentProcessException
        | HostEventKind::AgentRunningTaskCountChanged
        | HostEventKind::WorkspaceRunningTaskCountChanged
        | HostEventKind::BroadcastClaimCommit
        | HostEventKind::BroadcastClaimRelease => HostHandling::Observe,
        HostEventKind::BroadcastClaimRequest => HostHandling::Answer(AnswerKind::BroadcastClaim),
        HostEventKind::TaskRunLeaseAcquire => HostHandling::Answer(AnswerKind::TaskRunLease),
        HostEventKind::TaskOwnerCommandRequest => {
            HostHandling::Answer(AnswerKind::TaskOwnerCommand)
        }
        HostEventKind::FeedbackLogArchiveRequest => {
            HostHandling::Answer(AnswerKind::FeedbackArchive)
        }
        HostEventKind::BrowserExecuteRequest => HostHandling::Answer(AnswerKind::BrowserExecute),
        HostEventKind::LocalMediaPreviewPathAuthorizeRequest => {
            HostHandling::Answer(AnswerKind::MediaPreviewAuthorize)
        }
        HostEventKind::PluginSandboxRegisterRequest => {
            HostHandling::Answer(AnswerKind::PluginSandboxRegister)
        }
        HostEventKind::ProviderProvisioningLaneRequest => {
            HostHandling::Answer(AnswerKind::ProviderProvisioningLane)
        }
        HostEventKind::ResourceUsageSnapshotResult => {
            HostHandling::Response(ResponseKind::ResourceUsageSnapshot)
        }
        HostEventKind::ProjectWorkspacePrepared => HostHandling::Unsupported(
            "project workspace preparation is not available in Harness Phase 2",
        ),
        HostEventKind::RemoteWorkspaceConnectionLog
        | HostEventKind::RemoteWorkspaceConnected
        | HostEventKind::RemoteWorkspaceContextBound
        | HostEventKind::RemoteWorkspaceContextBindFailed
        | HostEventKind::RemoteWorkspaceReconnected
        | HostEventKind::RemoteWorkspaceConnectFailed
        | HostEventKind::RemoteWorkspaceClosed
        | HostEventKind::RemoteWorkspaceAcquired => {
            HostHandling::Unsupported("remote workspaces are not available in Harness Phase 2")
        }
        // Phase 4：harness-web-remote-* 是 host-node 远控 owner 回给 shell 的私有通道（结果与事件），
        // 不属于 ZCode Host 目录；按 Observe 交给 sink 转发，避免误报 unsupported。
        HostEventKind::Unknown(kind) if kind.starts_with(WEB_REMOTE_MESSAGE_PREFIX) => {
            HostHandling::Observe
        }
        HostEventKind::Unknown(_) => {
            HostHandling::Unsupported("host message is not part of the Phase 2 catalogue")
        }
        _ => HostHandling::Drop,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::HOST_EVENT_KINDS;

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Expected {
        Startup,
        Log,
        Observe,
        Drop,
        Answer(AnswerKind),
        Response(ResponseKind),
        Unsupported,
    }

    fn expected_for(wire: &str) -> Expected {
        use Expected::{Answer, Drop, Log, Observe, Response, Startup, Unsupported};
        match wire {
            "database-startup-state" => Startup,
            "log" => Log,
            "agent-process-spawned"
            | "agent-process-ready"
            | "agent-process-exited"
            | "agent-process-error"
            | "agent-process-exception"
            | "agent-running-task-count-changed"
            | "workspace-running-task-count-changed"
            | "broadcast-claim-commit"
            | "broadcast-claim-release" => Observe,
            "broadcast-claim-request" => Answer(AnswerKind::BroadcastClaim),
            "task-run-lease-acquire" => Answer(AnswerKind::TaskRunLease),
            "task-owner-command-request" => Answer(AnswerKind::TaskOwnerCommand),
            "feedback-log-archive-request" => Answer(AnswerKind::FeedbackArchive),
            "browser-execute-request" => Answer(AnswerKind::BrowserExecute),
            "local-media-preview-path-authorize-request" => {
                Answer(AnswerKind::MediaPreviewAuthorize)
            }
            "plugin-sandbox-register-request" => Answer(AnswerKind::PluginSandboxRegister),
            "provider-provisioning-lane-request" => Answer(AnswerKind::ProviderProvisioningLane),
            "resource-usage-snapshot-result" => Response(ResponseKind::ResourceUsageSnapshot),
            "project-workspace-prepared"
            | "remote-workspace-connection-log"
            | "remote-workspace-connected"
            | "remote-workspace-context-bound"
            | "remote-workspace-context-bind-failed"
            | "remote-workspace-reconnected"
            | "remote-workspace-connect-failed"
            | "remote-workspace-closed"
            | "remote-workspace-acquired" => Unsupported,
            "agent-resource-sample"
            | "host-resource-sample"
            | "mcp-telemetry"
            | "mcp-resource-samples"
            | "tool-exec-resource"
            | "session-create-telemetry"
            | "remote-conversation-telemetry"
            | "plugin-telemetry"
            | "network-capture-batch"
            | "cua-operation-state"
            | "broadcast"
            | "task-realtime-publish"
            | "task-stream-op-publish"
            | "task-run-lease-release"
            | "task-owner-command-result"
            | "session-message-send-requested"
            | "session-route-announce"
            | "session-message-deliver-result"
            | "cron-run-result"
            | "off-peak-run-result"
            | "cron-scheduler-wake-request"
            | "off-peak-scheduler-wake-request"
            | "network-telemetry-batch"
            | "provider-provisioning-source-changed"
            | "provider-provisioning-execution-result" => Drop,
            other => panic!("unclassified wire value: {other}"),
        }
    }

    fn actual(handling: HostHandling) -> Expected {
        match handling {
            HostHandling::StartupState => Expected::Startup,
            HostHandling::Log => Expected::Log,
            HostHandling::Answer(kind) => Expected::Answer(kind),
            HostHandling::Response(kind) => Expected::Response(kind),
            HostHandling::Observe => Expected::Observe,
            HostHandling::Drop => Expected::Drop,
            HostHandling::Unsupported(_) => Expected::Unsupported,
        }
    }

    #[test]
    fn every_catalogue_entry_matches_the_phase_two_table() {
        for wire in HOST_EVENT_KINDS {
            let kind = HostEventKind::from_wire(wire);
            assert_eq!(actual(classify(&kind)), expected_for(wire), "{wire}");
        }
    }

    #[test]
    fn unsupported_entries_carry_a_reason() {
        for wire in HOST_EVENT_KINDS {
            if let HostHandling::Unsupported(reason) = classify(&HostEventKind::from_wire(wire)) {
                assert!(
                    !reason.is_empty(),
                    "{wire} must explain why it is unsupported"
                );
            }
        }
    }

    #[test]
    fn unknown_messages_are_unsupported() {
        let kind = HostEventKind::from_wire("future-host-message");
        assert!(matches!(classify(&kind), HostHandling::Unsupported(_)));
    }

    #[test]
    fn web_remote_channel_messages_are_observed() {
        for wire in ["harness-web-remote-result", "harness-web-remote-event"] {
            let kind = HostEventKind::from_wire(wire);
            assert_eq!(classify(&kind), HostHandling::Observe);
        }
    }
}
