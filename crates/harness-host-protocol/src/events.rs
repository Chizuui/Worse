//! The `HostResponseTypes` catalogue: every message the `ZCode` Host can send.

macro_rules! host_event_kinds {
    ($($variant:ident => $wire:literal),+ $(,)?) => {
        /// One `HostResponseTypes` value received from the `ZCode` Host.
        ///
        /// Unknown wire values are preserved so a newer Host does not break an
        /// older shell; [`HostEventKind::is_known`] distinguishes the two.
        #[derive(Debug, Clone, PartialEq, Eq, Hash)]
        pub enum HostEventKind {
            $(
                $variant,
            )+
            /// A wire value outside the catalogue.
            Unknown(String),
        }

        /// Every known `HostResponseTypes` wire value, in catalogue order.
        pub const HOST_EVENT_KINDS: [&str; 54] = [$($wire),+];

        impl HostEventKind {
            /// Maps a wire value to its catalogue entry.
            #[must_use]
            pub fn from_wire(value: &str) -> Self {
                match value {
                    $($wire => Self::$variant,)+
                    _ => Self::Unknown(value.to_owned()),
                }
            }

            /// Returns the wire value of this entry.
            #[must_use]
            pub fn as_wire(&self) -> &str {
                match self {
                    $(Self::$variant => $wire,)+
                    Self::Unknown(value) => value.as_str(),
                }
            }

            /// Returns whether the value belongs to the catalogue.
            #[must_use]
            pub fn is_known(&self) -> bool {
                !matches!(self, Self::Unknown(_))
            }
        }
    };
}

host_event_kinds! {
    ProjectWorkspacePrepared => "project-workspace-prepared",
    DatabaseStartupState => "database-startup-state",
    RemoteWorkspaceConnectionLog => "remote-workspace-connection-log",
    RemoteWorkspaceConnected => "remote-workspace-connected",
    RemoteWorkspaceContextBound => "remote-workspace-context-bound",
    RemoteWorkspaceContextBindFailed => "remote-workspace-context-bind-failed",
    RemoteWorkspaceReconnected => "remote-workspace-reconnected",
    RemoteWorkspaceConnectFailed => "remote-workspace-connect-failed",
    RemoteWorkspaceClosed => "remote-workspace-closed",
    Log => "log",
    AgentProcessSpawned => "agent-process-spawned",
    AgentProcessReady => "agent-process-ready",
    AgentProcessExited => "agent-process-exited",
    AgentProcessError => "agent-process-error",
    AgentProcessException => "agent-process-exception",
    AgentResourceSample => "agent-resource-sample",
    HostResourceSample => "host-resource-sample",
    McpTelemetry => "mcp-telemetry",
    McpResourceSamples => "mcp-resource-samples",
    ToolExecResource => "tool-exec-resource",
    SessionCreateTelemetry => "session-create-telemetry",
    RemoteConversationTelemetry => "remote-conversation-telemetry",
    PluginTelemetry => "plugin-telemetry",
    ResourceUsageSnapshotResult => "resource-usage-snapshot-result",
    NetworkCaptureBatch => "network-capture-batch",
    AgentRunningTaskCountChanged => "agent-running-task-count-changed",
    WorkspaceRunningTaskCountChanged => "workspace-running-task-count-changed",
    CuaOperationState => "cua-operation-state",
    RemoteWorkspaceAcquired => "remote-workspace-acquired",
    Broadcast => "broadcast",
    BroadcastClaimRequest => "broadcast-claim-request",
    BroadcastClaimCommit => "broadcast-claim-commit",
    BroadcastClaimRelease => "broadcast-claim-release",
    TaskRealtimePublish => "task-realtime-publish",
    TaskStreamOpPublish => "task-stream-op-publish",
    TaskRunLeaseAcquire => "task-run-lease-acquire",
    TaskRunLeaseRelease => "task-run-lease-release",
    TaskOwnerCommandRequest => "task-owner-command-request",
    TaskOwnerCommandResult => "task-owner-command-result",
    SessionMessageSendRequested => "session-message-send-requested",
    SessionRouteAnnounce => "session-route-announce",
    SessionMessageDeliverResult => "session-message-deliver-result",
    FeedbackLogArchiveRequest => "feedback-log-archive-request",
    CronRunResult => "cron-run-result",
    OffPeakRunResult => "off-peak-run-result",
    CronSchedulerWakeRequest => "cron-scheduler-wake-request",
    OffPeakSchedulerWakeRequest => "off-peak-scheduler-wake-request",
    BrowserExecuteRequest => "browser-execute-request",
    LocalMediaPreviewPathAuthorizeRequest => "local-media-preview-path-authorize-request",
    PluginSandboxRegisterRequest => "plugin-sandbox-register-request",
    NetworkTelemetryBatch => "network-telemetry-batch",
    ProviderProvisioningSourceChanged => "provider-provisioning-source-changed",
    ProviderProvisioningExecutionResult => "provider-provisioning-execution-result",
    ProviderProvisioningLaneRequest => "provider-provisioning-lane-request",
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    /// Catalogue order copied from `HostResponseTypes` in
    /// `packages/shared/src/channels.ts`.
    const EXPECTED_ORDER: [&str; 54] = [
        "project-workspace-prepared",
        "database-startup-state",
        "remote-workspace-connection-log",
        "remote-workspace-connected",
        "remote-workspace-context-bound",
        "remote-workspace-context-bind-failed",
        "remote-workspace-reconnected",
        "remote-workspace-connect-failed",
        "remote-workspace-closed",
        "log",
        "agent-process-spawned",
        "agent-process-ready",
        "agent-process-exited",
        "agent-process-error",
        "agent-process-exception",
        "agent-resource-sample",
        "host-resource-sample",
        "mcp-telemetry",
        "mcp-resource-samples",
        "tool-exec-resource",
        "session-create-telemetry",
        "remote-conversation-telemetry",
        "plugin-telemetry",
        "resource-usage-snapshot-result",
        "network-capture-batch",
        "agent-running-task-count-changed",
        "workspace-running-task-count-changed",
        "cua-operation-state",
        "remote-workspace-acquired",
        "broadcast",
        "broadcast-claim-request",
        "broadcast-claim-commit",
        "broadcast-claim-release",
        "task-realtime-publish",
        "task-stream-op-publish",
        "task-run-lease-acquire",
        "task-run-lease-release",
        "task-owner-command-request",
        "task-owner-command-result",
        "session-message-send-requested",
        "session-route-announce",
        "session-message-deliver-result",
        "feedback-log-archive-request",
        "cron-run-result",
        "off-peak-run-result",
        "cron-scheduler-wake-request",
        "off-peak-scheduler-wake-request",
        "browser-execute-request",
        "local-media-preview-path-authorize-request",
        "plugin-sandbox-register-request",
        "network-telemetry-batch",
        "provider-provisioning-source-changed",
        "provider-provisioning-execution-result",
        "provider-provisioning-lane-request",
    ];

    #[test]
    fn catalogue_matches_the_documented_order() {
        assert_eq!(HOST_EVENT_KINDS.len(), 54);
        assert_eq!(HOST_EVENT_KINDS, EXPECTED_ORDER);
    }

    #[test]
    fn catalogue_has_no_duplicates() {
        let unique: HashSet<&str> = HOST_EVENT_KINDS.into_iter().collect();
        assert_eq!(unique.len(), HOST_EVENT_KINDS.len());
    }

    #[test]
    fn every_wire_value_round_trips_through_the_enum() {
        for wire in HOST_EVENT_KINDS {
            let kind = HostEventKind::from_wire(wire);
            assert!(kind.is_known(), "{wire} should be known");
            assert_eq!(kind.as_wire(), wire);
        }
    }

    #[test]
    fn unknown_wire_values_are_preserved() {
        let kind = HostEventKind::from_wire("future-host-message");
        assert!(!kind.is_known());
        assert_eq!(kind.as_wire(), "future-host-message");
        assert_eq!(
            kind,
            HostEventKind::Unknown("future-host-message".to_owned())
        );
    }
}
