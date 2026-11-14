//! Plain data types of the hub API.

use serde::{Deserialize, Serialize};

macro_rules! string_id {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Default, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(pub String);

        impl $name {
            /// Wraps a raw id.
            #[must_use]
            pub fn new(value: impl Into<String>) -> Self {
                Self(value.into())
            }

            /// The raw id.
            #[must_use]
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl From<&str> for $name {
            fn from(value: &str) -> Self {
                Self(value.to_owned())
            }
        }
    };
}

string_id!(
    /// Harness session id (the conversation; owned by Harness, not by any CLI).
    SessionId
);
string_id!(
    /// One user turn inside a session.
    TurnId
);
string_id!(
    /// Agent id: `claude-code`, `codex`, `agy`, or a configured extra agent (gates: `fake-a`).
    AgentId
);
string_id!(
    /// Hub-assigned id of one permission request.
    PermissionRequestId
);

/// Whether an agent can be started on this machine. Probing never runs the CLI.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "camelCase")]
pub enum Availability {
    /// The launch command resolved to an executable.
    #[serde(rename_all = "camelCase")]
    Available { command_path: String },
    /// The launch command is not on PATH. `hint` says how to install it.
    #[serde(rename_all = "camelCase")]
    NotInstalled { hint: String },
    /// The agent's CLI is installed but its ACP adapter (an npm package) is not (API 0.9.0,
    /// phase-9-parity.md §9.10). `cli_path` is the resolved CLI, `package` the npm package to install.
    #[serde(rename_all = "camelCase")]
    AdapterMissing {
        cli_path: String,
        package: String,
        hint: String,
    },
}

impl Availability {
    /// True for [`Availability::Available`].
    #[must_use]
    pub fn is_available(&self) -> bool {
        matches!(self, Self::Available { .. })
    }
}

/// Quota/credit state of an agent (global per agent, owned by the hub's quota registry).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "camelCase")]
pub enum QuotaState {
    /// Usable.
    Ok,
    /// Usage limit reached; skipped by failover until `resets_at` (unix seconds) or a manual swap.
    #[serde(rename_all = "camelCase")]
    Exhausted {
        resets_at: Option<i64>,
        reason: String,
    },
}

/// Process state of an agent CLI.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum AgentRuntimeState {
    /// No process.
    Stopped,
    /// Spawned, ACP `initialize` in flight.
    Starting,
    /// Serving at least one turn.
    Busy,
    /// Alive without turns; will be stopped after the idle timeout.
    Idle,
    /// Exited unexpectedly.
    Crashed,
}

/// One configured agent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentInfo {
    pub id: AgentId,
    pub display_name: String,
    pub availability: Availability,
    pub quota: QuotaState,
    pub runtime: AgentRuntimeState,
}

/// `create_session` request.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateSession {
    /// Absolute workspace path (cwd of agents, project MEMORY.md root).
    pub workspace_path: String,
    /// Optional identity; key = `workspace_identity.trim() || workspace_path`.
    pub workspace_identity: Option<String>,
    pub title: Option<String>,
    /// Initial sticky agent; `None` = first available in the fallback order.
    pub agent: Option<AgentId>,
    #[serde(default)]
    pub worktree: Option<WorktreeRequest>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorktreeRequest {
    pub branch: Option<String>,
    pub base: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorktreeInfo {
    pub path: String,
    pub branch: String,
    pub base: String,
    pub repo_root: String,
}

/// `list_sessions` request.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ListSessions {
    pub workspace_key: Option<String>,
    /// Case-insensitive match on title and transcript text.
    pub query: Option<String>,
    pub limit: Option<u32>,
}

/// Session list row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionSummary {
    pub id: SessionId,
    pub title: String,
    pub workspace_path: String,
    pub workspace_key: String,
    /// Sticky agent (single owner: the session actor).
    pub active_agent: AgentId,
    /// Unix milliseconds.
    pub created_at: i64,
    pub updated_at: i64,
    pub turn_count: u32,
    /// Distinct agents that served at least one turn (or attempt), in order of first use (API 0.2.0).
    #[serde(default)]
    pub agents_used: Vec<AgentId>,
    /// Pinned to the top of the sidebar (API 0.5.0, phase-9-parity.md §9.2.1).
    #[serde(default)]
    pub pinned: bool,
    /// Sidebar group; `None` = ungrouped.
    #[serde(default)]
    pub group_id: Option<String>,
    /// Sort key inside the sidebar section (larger = higher; `move_sessions` raises it).
    #[serde(default)]
    pub order: i64,
    /// Marked unread, or a turn finished after the session was last read.
    #[serde(default)]
    pub unread: bool,
    #[serde(default)]
    pub forked_from: Option<crate::chat::ForkOrigin>,
    /// `zcode:<id>` / `claude-code:<id>` for imported sessions.
    #[serde(default)]
    pub imported_from: Option<String>,
    /// The session that delegated this one (subagent session, API 0.8.0, phase-9-parity.md §9.8).
    #[serde(default)]
    pub parent_id: Option<SessionId>,
    #[serde(default)]
    pub worktree: Option<WorktreeInfo>,
}

/// Lifecycle of a subagent (API 0.8.0, §9.8).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SubagentStatus {
    Running,
    NeedsApproval,
    Done,
    Failed,
    Cancelled,
    /// The hub stopped while it was running.
    Interrupted,
}

impl SubagentStatus {
    /// Store / wire spelling.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::NeedsApproval => "needsApproval",
            Self::Done => "done",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::Interrupted => "interrupted",
        }
    }

    /// Inverse of [`SubagentStatus::as_str`]; unknown = interrupted.
    #[must_use]
    pub fn parse(value: &str) -> Self {
        match value {
            "running" => Self::Running,
            "needsApproval" => Self::NeedsApproval,
            "done" => Self::Done,
            "failed" => Self::Failed,
            "cancelled" => Self::Cancelled,
            _ => Self::Interrupted,
        }
    }

    /// Still working (running or waiting for the user).
    #[must_use]
    pub fn is_active(self) -> bool {
        matches!(self, Self::Running | Self::NeedsApproval)
    }
}

/// One subagent session of a parent (API 0.8.0, §9.8).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SubagentView {
    pub session_id: SessionId,
    pub parent_id: SessionId,
    pub agent: AgentId,
    pub title: String,
    pub status: SubagentStatus,
    /// Last tool title or other short live activity ("" when none).
    pub activity: String,
    /// Unix milliseconds of the latest delegated turn.
    pub started_at: i64,
    pub finished_at: Option<i64>,
    /// First characters of the latest answer.
    pub result_preview: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BackgroundTask {
    pub id: String,
    pub agent: AgentId,
    pub name: String,
    pub task_type: String,
    pub description: String,
    pub state: String,
    pub summary: String,
    pub output_file: Option<String>,
    pub can_stop: bool,
    pub started_at: i64,
    pub finished_at: Option<i64>,
}

impl BackgroundTask {
    #[must_use]
    pub fn is_running(&self) -> bool {
        self.finished_at.is_none()
    }
}

/// Lifecycle of a turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum TurnStatus {
    Queued,
    Running,
    /// Every agent is exhausted; waits for a reset or a manual swap.
    Paused,
    Completed,
    Cancelled,
    Failed,
    /// Was queued/running when the hub stopped unexpectedly; not re-run.
    Interrupted,
}

/// Error classification of an agent failure.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "class", rename_all = "camelCase")]
pub enum ErrorClass {
    /// Usage limit / credits exhausted. Triggers failover.
    #[serde(rename_all = "camelCase")]
    QuotaExhausted { resets_at: Option<i64> },
    /// Short rate limit or network error; the user may resend.
    #[serde(rename_all = "camelCase")]
    Transient { retry_after_secs: Option<u64> },
    /// Anything else.
    Fatal,
}

/// One execution of a turn by one agent (a failover produces several).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnAttempt {
    pub attempt: u32,
    pub agent: AgentId,
    /// `running`, `completed`, `cancelled`, `failed`, `quotaExhausted`.
    pub status: String,
    pub partial_text: String,
    pub error: Option<String>,
    pub error_class: Option<ErrorClass>,
}

/// Tool call status (ACP `ToolCallStatus`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ToolCallStatus {
    Pending,
    InProgress,
    Completed,
    Failed,
}

/// Display copy of a tool call.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolCallView {
    pub tool_call_id: String,
    pub title: String,
    /// ACP `ToolKind` (`read`, `edit`, `execute`, ...).
    pub kind: Option<String>,
    pub status: Option<ToolCallStatus>,
    /// Raw input as JSON text (display only).
    pub raw_input: Option<String>,
    /// Text / diff / terminal content (API 0.5.0; text capped at 64 KB).
    #[serde(default)]
    pub content: Vec<crate::chat::ToolContent>,
    #[serde(default)]
    pub locations: Vec<crate::chat::ToolLocation>,
    /// Raw output as JSON text (capped at 64 KB).
    #[serde(default)]
    pub raw_output: Option<String>,
}

/// Kind of a permission option (ACP `PermissionOptionKind`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionOptionKind {
    AllowOnce,
    AllowAlways,
    RejectOnce,
    RejectAlways,
}

/// One answer the agent offers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PermissionOption {
    pub option_id: String,
    pub name: String,
    pub kind: PermissionOptionKind,
}

/// A pending permission request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PermissionRequestView {
    pub request_id: PermissionRequestId,
    pub turn_id: TurnId,
    pub agent: AgentId,
    pub tool: ToolCallView,
    pub options: Vec<PermissionOption>,
}

/// How a permission request ended.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "camelCase")]
pub enum PermissionDecision {
    #[serde(rename_all = "camelCase")]
    Selected {
        option_id: String,
        kind: PermissionOptionKind,
    },
    /// Stop, failover or agent exit.
    Cancelled,
}

/// One turn of a session snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnView {
    pub id: TurnId,
    pub seq: u32,
    pub user_text: String,
    /// Agent that served (or is serving) the turn; `None` while queued.
    pub agent: Option<AgentId>,
    pub assistant_text: String,
    pub status: TurnStatus,
    pub stop_reason: Option<String>,
    pub error: Option<String>,
    pub attempts: Vec<TurnAttempt>,
    pub tool_calls: Vec<ToolCallView>,
    /// `(request title, decision)` per answered permission request.
    pub permissions: Vec<(String, PermissionDecision)>,
    pub created_at: i64,
    pub finished_at: Option<i64>,
    /// Text and tool calls in live order; text segments concatenate to `assistant_text` (API 0.2.0).
    #[serde(default)]
    pub segments: Vec<TurnSegment>,
    /// Attachments and mentions of the user turn (API 0.5.0).
    #[serde(default)]
    pub context: Vec<crate::chat::ContextItem>,
    /// Latest ACP plan of the turn.
    #[serde(default)]
    pub plan: Vec<crate::chat::PlanEntry>,
    /// Token counts from the prompt result.
    #[serde(default)]
    pub usage: Option<crate::chat::TokenUsage>,
    /// Latest context window use reported during the turn.
    #[serde(default)]
    pub context_usage: Option<crate::chat::ContextUsage>,
    /// Set when a scheduled prompt sent the turn.
    #[serde(default)]
    pub origin: Option<crate::chat::TurnOrigin>,
}

/// One piece of a turn's transcript in display order (spec §15.1 request 4).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum TurnSegment {
    /// Assistant text.
    Text {
        text: String,
    },
    /// A tool call; details in `TurnView.tool_calls` with the same id.
    #[serde(rename_all = "camelCase")]
    Tool {
        tool_call_id: String,
    },
    Steer {
        text: String,
    },
}

/// Changes an agent made to the workspace during one turn (spec §15.2).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnDiff {
    pub turn_id: TurnId,
    /// `false` when no snapshot exists (not a git repository, git missing, snapshots disabled, pruned).
    pub available: bool,
    /// Why the diff is unavailable.
    pub reason: Option<String>,
    pub files: Vec<crate::diff::FileDiff>,
    /// The diff output hit the size cap; `files` is incomplete.
    pub truncated: bool,
    /// The turn's changes were undone (API 0.5.0).
    #[serde(default)]
    pub undone: bool,
}

/// `open_session` result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionDetail {
    pub summary: SessionSummary,
    pub turns: Vec<TurnView>,
    /// Ids of queued turns in admission order (the running turn is not included).
    pub queue: Vec<TurnId>,
    pub pending_permissions: Vec<PermissionRequestView>,
    /// Latest non-empty plan of the session (todo panel, API 0.5.0).
    #[serde(default)]
    pub plan: Vec<crate::chat::PlanEntry>,
    /// Latest context window use.
    #[serde(default)]
    pub context_usage: Option<crate::chat::ContextUsage>,
    /// `HubEvent.seq` at snapshot time (API 0.5.2). The session's events with `seq` at or below it are already in
    /// this snapshot; a client that subscribed before opening drops them instead of applying them twice.
    #[serde(default)]
    pub event_seq: u64,
    /// Subagent sessions this session delegated to, newest first (API 0.8.0, §9.8).
    #[serde(default)]
    pub subagents: Vec<SubagentView>,
    #[serde(default)]
    pub cost: Option<SessionCost>,
    #[serde(default)]
    pub background_tasks: Vec<BackgroundTask>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionCost {
    pub own: crate::chat::Cost,
    pub subagents: Option<crate::chat::Cost>,
    pub total: crate::chat::Cost,
}

/// `send_message` request.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SendMessage {
    pub session_id: SessionId,
    pub text: String,
    /// Manual swap: this agent serves the turn and becomes sticky. `None` = sticky agent.
    pub agent: Option<AgentId>,
    /// Idempotency key: a repeated send with the same key returns the first acceptance.
    pub client_message_id: Option<String>,
    /// Attachments and mentions (API 0.5.0, limits in `chat::limits`).
    #[serde(default)]
    pub context: Vec<crate::chat::ContextItem>,
}

/// `send_message` result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SendAccepted {
    pub turn_id: TurnId,
    pub seq: u32,
    /// 0 = starts now; n = n turns ahead (running one included).
    pub position: u32,
}

/// Which MEMORY.md.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "scope", rename_all = "camelCase")]
pub enum MemoryScope {
    /// `<HARNESS_HOME>/MEMORY.md`.
    Global,
    /// `<workspace_path>/.harness/MEMORY.md`.
    #[serde(rename_all = "camelCase")]
    Project { workspace_path: String },
}

/// Memory write mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum MemoryWriteMode {
    Replace,
    Append,
}
