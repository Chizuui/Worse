//! Hub event stream.

use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

use crate::chat::{ChangeDirection, ContextUsage, Failure, PlanEntry, QueuedTurn};
use crate::types::{
    AgentId, AgentRuntimeState, ErrorClass, MemoryScope, PermissionDecision, PermissionRequestId,
    PermissionRequestView, QuotaState, SessionId, SessionSummary, ToolCallView, TurnId,
};

/// One hub event. `seq` increases by one per event across the whole hub; a gap means events were lost
/// (see [`RecvError::Lagged`]) and the UI should re-open the session snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HubEvent {
    pub seq: u64,
    pub session_id: Option<SessionId>,
    pub kind: EventKind,
}

/// How a turn ended.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "camelCase")]
pub enum TurnOutcome {
    /// ACP stop reason (`end_turn`, `max_tokens`, `refusal`, ...).
    #[serde(rename_all = "camelCase")]
    Completed {
        stop_reason: String,
    },
    Cancelled,
    #[serde(rename_all = "camelCase")]
    Failed {
        error: String,
        class: ErrorClass,
    },
}

/// Why the active agent changed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "reason", rename_all = "camelCase")]
pub enum SwitchReason {
    /// Explicit agent on send or `set_active_agent`.
    Manual,
    /// The previous agent's usage limit was reached.
    #[serde(rename_all = "camelCase")]
    QuotaExhausted { resets_at: Option<i64> },
    /// The sticky agent is not installed / not usable (added in API 0.1.1).
    Unavailable,
}

/// Why a turn is paused.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "reason", rename_all = "camelCase")]
pub enum PauseReason {
    /// No available, non-exhausted agent. `agents` = `(agent, resets_at)`.
    AllAgentsExhausted { agents: Vec<(AgentId, Option<i64>)> },
}

/// Event payloads.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum EventKind {
    SessionCreated {
        summary: SessionSummary,
    },
    SessionUpdated {
        summary: SessionSummary,
    },
    #[serde(rename_all = "camelCase")]
    TurnQueued {
        turn_id: TurnId,
        seq: u32,
        user_text: String,
        position: u32,
        /// The agent started this turn on its own when a background task ended (§9.17): shown as one muted line
        /// instead of a user bubble.
        #[serde(default)]
        background: bool,
        /// Paths of the images attached to the turn (API 0.19.0).
        #[serde(default)]
        images: Vec<String>,
    },
    #[serde(rename_all = "camelCase")]
    TurnStarted {
        turn_id: TurnId,
        agent: AgentId,
        attempt: u32,
    },
    #[serde(rename_all = "camelCase")]
    MessageDelta {
        turn_id: TurnId,
        agent: AgentId,
        text: String,
    },
    #[serde(rename_all = "camelCase")]
    ThoughtDelta {
        turn_id: TurnId,
        agent: AgentId,
        text: String,
    },
    #[serde(rename_all = "camelCase")]
    ToolCall {
        turn_id: TurnId,
        agent: AgentId,
        tool: ToolCallView,
    },
    #[serde(rename_all = "camelCase")]
    ToolCallUpdated {
        turn_id: TurnId,
        agent: AgentId,
        tool: ToolCallView,
    },
    PermissionRequested {
        request: PermissionRequestView,
    },
    #[serde(rename_all = "camelCase")]
    PermissionResolved {
        turn_id: TurnId,
        request_id: PermissionRequestId,
        decision: PermissionDecision,
    },
    #[serde(rename_all = "camelCase")]
    TurnFinished {
        turn_id: TurnId,
        agent: Option<AgentId>,
        outcome: TurnOutcome,
    },
    #[serde(rename_all = "camelCase")]
    TurnPaused {
        turn_id: TurnId,
        reason: PauseReason,
    },
    /// The sticky agent changed. `message` is ready to show, e.g.
    /// `switched from Claude Code to Codex: Claude Code limit reached (resets at ...)`.
    AgentSwitched {
        from: Option<AgentId>,
        to: AgentId,
        reason: SwitchReason,
        message: String,
    },
    AgentRuntime {
        agent: AgentId,
        state: AgentRuntimeState,
        pid: Option<u32>,
    },
    AgentQuota {
        agent: AgentId,
        quota: QuotaState,
    },
    MemoryChanged {
        scope: MemoryScope,
    },
    /// Modes/models/config options of the session's agent changed (a set call or a new advertisement);
    /// re-read them with `list_modes` / `list_models` / `list_config_options` (API 0.3.0).
    SessionOptionsChanged {
        agent: AgentId,
    },
    /// Sessions were archived, unarchived or deleted, or projects removed/restored; re-read session, project and
    /// archive lists (global, API 0.4.0).
    SessionsChanged,
    /// Sidebar groups changed (global, API 0.5.0).
    GroupsChanged,
    /// Turns from `from_seq` on were removed (rewind / edit / retry); re-open the session (API 0.5.0).
    #[serde(rename_all = "camelCase")]
    TurnsRemoved {
        from_seq: u32,
    },
    /// Workspace files of a turn were restored (undo / reapply).
    #[serde(rename_all = "camelCase")]
    FilesRestored {
        turn_id: TurnId,
        direction: ChangeDirection,
    },
    /// ACP plan of the running turn (todo list).
    #[serde(rename_all = "camelCase")]
    PlanUpdated {
        turn_id: TurnId,
        agent: AgentId,
        entries: Vec<PlanEntry>,
    },
    /// Context window use of the session's agent.
    UsageUpdated {
        agent: AgentId,
        usage: ContextUsage,
    },
    /// The slash commands an agent advertised changed (global).
    CommandsChanged {
        agent: AgentId,
    },
    /// Scheduled prompts changed or ran (global).
    SchedulesChanged,
    BackgroundTasksChanged {
        tasks: Vec<crate::types::BackgroundTask>,
    },
    /// The session's queue after an edit, reorder, run-now or steer; replace semantics (API 0.5.1, §9.2.5).
    QueueChanged {
        queue: Vec<QueuedTurn>,
    },
    #[serde(rename_all = "camelCase")]
    SteerInjected {
        turn_id: TurnId,
        steered_turn_id: TurnId,
        text: String,
    },
    /// A typed failure of a turn, sent just before its `TurnFinished { Failed }` (API 0.5.1, §9.2.5).
    #[serde(rename_all = "camelCase")]
    TurnFailure {
        turn_id: TurnId,
        agent: AgentId,
        failure: Failure,
    },
    /// A subagent of this (parent) session started, changed activity or finished; upsert by
    /// `subagent.session_id` (API 0.8.0, phase-9-parity.md §9.8).
    SubagentUpdated {
        subagent: crate::types::SubagentView,
    },
    /// Progress of an ACP adapter install (global, API 0.9.0, phase-9-parity.md §9.10). `running` events carry
    /// one output `line`; the final `succeeded` / `failed` event carries `message`.
    AdapterInstall {
        agent: AgentId,
        package: String,
        status: AdapterInstallStatus,
        line: Option<String>,
        message: Option<String>,
    },
    #[serde(rename_all = "camelCase")]
    BoardChanged {
        workspace_key: String,
    },
    InboxChanged,
}

/// Status of an [`EventKind::AdapterInstall`] event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum AdapterInstallStatus {
    Running,
    Succeeded,
    Failed,
}

/// Receive error of [`EventStream`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum RecvError {
    /// The subscriber fell behind and `n` events were dropped; re-open the session snapshot.
    #[error("lagged by {0} events")]
    Lagged(u64),
    /// The hub has shut down.
    #[error("event stream closed")]
    Closed,
}

/// Subscription to hub events.
#[derive(Debug)]
pub struct EventStream {
    inner: broadcast::Receiver<HubEvent>,
}

impl EventStream {
    /// Wraps a broadcast receiver (used by hub implementations).
    #[must_use]
    pub fn new(inner: broadcast::Receiver<HubEvent>) -> Self {
        Self { inner }
    }

    /// Waits for the next event.
    ///
    /// # Errors
    ///
    /// [`RecvError::Lagged`] when events were dropped, [`RecvError::Closed`] after shutdown.
    pub async fn recv(&mut self) -> Result<HubEvent, RecvError> {
        self.inner.recv().await.map_err(|error| match error {
            broadcast::error::RecvError::Lagged(n) => RecvError::Lagged(n),
            broadcast::error::RecvError::Closed => RecvError::Closed,
        })
    }

    /// Non-blocking receive; `Ok(None)` when no event is ready.
    ///
    /// # Errors
    ///
    /// Same as [`EventStream::recv`].
    pub fn try_recv(&mut self) -> Result<Option<HubEvent>, RecvError> {
        match self.inner.try_recv() {
            Ok(event) => Ok(Some(event)),
            Err(broadcast::error::TryRecvError::Empty) => Ok(None),
            Err(broadcast::error::TryRecvError::Lagged(n)) => Err(RecvError::Lagged(n)),
            Err(broadcast::error::TryRecvError::Closed) => Err(RecvError::Closed),
        }
    }
}
