//! Agents overview, memory documents, projects and archive (API 0.4.0, spec `phase-9-parity.md` §9.1.5a).

use serde::{Deserialize, Serialize};

use crate::types::{
    AgentId, AgentRuntimeState, MemoryScope, QuotaState, SessionId, SessionSummary,
};

/// Login presence of an agent CLI (never read from secrets; existence only).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum AuthPresence {
    Present,
    Absent,
    Unknown,
}

/// One agent card of the settings Agents page.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentOverview {
    pub id: AgentId,
    pub display_name: String,
    /// Launch command resolved on PATH (the CLI is never run for this).
    pub installed: bool,
    pub command_path: Option<String>,
    /// How to install, when not installed.
    pub install_hint: Option<String>,
    /// `agentInfo.version` of the last ACP `initialize` (ACP adapter version); `None` until known.
    pub version: Option<String>,
    pub auth: AuthPresence,
    /// How to log in (no secrets), for the CLIs Harness knows.
    pub login_hint: Option<String>,
    pub runtime: AgentRuntimeState,
    pub quota: QuotaState,
    /// The CLI is installed but its ACP adapter is not (API 0.9.0, §9.10).
    #[serde(default)]
    pub adapter_missing: bool,
    /// npm package of the agent's ACP adapter, when Harness knows one.
    #[serde(default)]
    pub adapter_package: Option<String>,
}

/// State of one row of the first-run setup (API 0.9.0, phase-9-parity.md §9.10).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SetupState {
    /// CLI and adapter are installed.
    Ready,
    /// The CLI is installed, its ACP adapter is not.
    AdapterMissing,
    /// The CLI itself is not installed (Harness never installs CLIs).
    CliMissing,
}

/// One row of [`SetupStatus`]: one launch program (accounts share the row of their CLI).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SetupAgent {
    pub agent: AgentId,
    pub display_name: String,
    /// CLI command name (`claude`, `codex`, `agy`).
    pub cli: String,
    pub cli_path: Option<String>,
    /// npm package of the adapter; `None` for CLIs that speak ACP themselves.
    pub package: Option<String>,
    pub state: SetupState,
}

/// Result of [`crate::HarnessCore::setup_status`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SetupStatus {
    /// Resolved npm command; `None` when npm (Node.js) is missing.
    pub npm_path: Option<String>,
    /// Plain-text steps to get npm when it is missing.
    pub npm_steps: String,
    pub agents: Vec<SetupAgent>,
    #[serde(default)]
    pub gh: GhSetup,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GhSetup {
    pub path: Option<String>,
    pub bundled: bool,
}

/// Result of [`crate::HarnessCore::install_adapter`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AdapterInstallResult {
    pub agent: AgentId,
    pub ok: bool,
    pub message: String,
}

/// Result of [`crate::HarnessCore::test_agent`] (spawn + ACP `initialize`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentTestResult {
    pub agent: AgentId,
    pub ok: bool,
    pub latency_ms: u64,
    pub agent_name: Option<String>,
    pub version: Option<String>,
    pub error: Option<String>,
}

/// One MEMORY.md file (settings Memory page tree).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MemoryFileInfo {
    pub scope: MemoryScope,
    pub path: String,
    pub exists: bool,
    pub bytes: u64,
    /// Unix milliseconds.
    pub modified_ms: Option<i64>,
}

/// A MEMORY.md file with its content.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MemoryDoc {
    pub scope: MemoryScope,
    pub path: String,
    pub content: String,
    pub exists: bool,
}

/// A planned MEMORY.md replacement; apply with [`crate::HarnessCore::memory_apply`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MemoryPlan {
    pub scope: MemoryScope,
    pub path: String,
    /// Fingerprint of the file the plan was made against (conflict check).
    pub base: String,
    pub content: String,
    /// Unified diff (3 lines of context); empty when `no_op`.
    pub diff: String,
    pub no_op: bool,
}

/// `list_projects` request.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ListProjects {
    /// Case-insensitive match on the workspace path.
    pub query: Option<String>,
    /// `true` lists removed projects (restore list) instead of the active ones.
    #[serde(default)]
    pub removed: bool,
}

/// One project (workspace) known to the hub.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectInfo {
    pub workspace_key: String,
    pub workspace_path: String,
    /// Sessions not archived.
    pub sessions: u32,
    pub archived: u32,
    /// Newest session `updated_at` (unix ms).
    pub last_used: Option<i64>,
    /// No session updated in the last 30 days.
    pub unused: bool,
    /// Unix ms when removed (removed list only).
    pub removed_at: Option<i64>,
}

/// Archive list order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ArchiveSort {
    #[default]
    ArchivedNewest,
    ArchivedOldest,
    Title,
}

/// `list_archived` request.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ListArchived {
    pub query: Option<String>,
    pub workspace_key: Option<String>,
    #[serde(default)]
    pub sort: ArchiveSort,
    #[serde(default)]
    pub offset: u32,
    /// Page size, at most 200 (default 50).
    pub limit: Option<u32>,
}

/// One archived session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ArchivedSession {
    pub summary: SessionSummary,
    /// Unix ms.
    pub archived_at: i64,
}

/// One page of the archive.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ArchivePage {
    pub rows: Vec<ArchivedSession>,
    pub total: u32,
    pub has_more: bool,
}

/// Which archived sessions an archive action targets.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SessionSelection {
    Sessions(Vec<SessionId>),
    /// Every archived session of this workspace key.
    Project(String),
}

/// `add_account` request (API 0.6.0, phase-9-parity.md §9.6).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AddAccount {
    /// Built-in agent the account is a profile of (only `claude-code` today).
    pub base: AgentId,
    /// User label, e.g. "Work".
    pub label: String,
}

/// One extra login profile of an agent CLI (phase-9-parity.md §9.6).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountInfo {
    /// Agent id of the account (`claude-code-<slug>`).
    pub id: AgentId,
    pub base: AgentId,
    pub label: String,
    /// Config dir of the profile (`CLAUDE_CONFIG_DIR`).
    pub profile_dir: String,
    /// True when the running hub has this account as an agent (false until the next start).
    pub loaded: bool,
    /// Login presence (file existence only).
    pub auth: AuthPresence,
}

/// One Antigravity (agy) login slot (API 0.18.0). Never carries credential bytes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgyAccountInfo {
    pub id: String,
    pub name: String,
    pub created_ms: i64,
    /// A saved login exists for this slot.
    pub signed_in: bool,
    /// This slot's login is the one agy uses now.
    pub active: bool,
    /// Hit its usage limit in this hub run (skipped by failover until `Use`).
    pub exhausted: bool,
}

/// Antigravity account list (API 0.18.0).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgyAccounts {
    /// False when the platform has no supported credential store.
    pub available: bool,
    pub accounts: Vec<AgyAccountInfo>,
    /// Fallback order of account ids.
    pub order: Vec<String>,
    /// agy currently has a login (for "Import current login").
    pub live_signed_in: bool,
    /// Account waiting for its sign-in console to finish.
    pub pending: Option<String>,
}

/// Antigravity account operation (API 0.18.0).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "camelCase")]
pub enum AgyAccountOp {
    List,
    Add {
        name: String,
    },
    Remove {
        id: String,
    },
    /// Saves the current login, signs agy out and marks `id` pending; the caller opens the login console.
    SignIn {
        id: String,
    },
    /// Stores the fresh login of the pending account (also done by `List`).
    FinishSignIn,
    /// Makes `id` the login agy uses.
    Use {
        id: String,
    },
    /// Creates an account from the current agy login.
    Import {
        name: String,
    },
    SetOrder {
        order: Vec<String>,
    },
}

/// `add_provider` request (API 0.7.0, phase-9-parity.md §9.7). `Debug` hides the key.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AddProvider {
    pub label: String,
    /// OpenAI-compatible base URL, e.g. `https://api.example.com/v1`.
    pub base_url: String,
    pub model: String,
    pub api_key: String,
}

impl std::fmt::Debug for AddProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AddProvider")
            .field("label", &self.label)
            .field("base_url", &self.base_url)
            .field("model", &self.model)
            .field("api_key", &"<redacted>")
            .finish()
    }
}

/// `update_provider` request: `None` keeps the stored value. `Debug` hides the key.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateProvider {
    pub id: AgentId,
    pub label: Option<String>,
    pub base_url: Option<String>,
    pub model: Option<String>,
    pub api_key: Option<String>,
}

impl std::fmt::Debug for UpdateProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UpdateProvider")
            .field("id", &self.id)
            .field("label", &self.label)
            .field("base_url", &self.base_url)
            .field("model", &self.model)
            .field("api_key", &self.api_key.as_ref().map(|_| "<redacted>"))
            .finish()
    }
}

/// One custom API provider (phase-9-parity.md §9.7). Never carries the key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderInfo {
    /// Agent id of the provider (`api-<slug>`).
    pub id: AgentId,
    pub label: String,
    pub base_url: String,
    pub model: String,
    pub has_key: bool,
    /// True when the running hub has this provider as an agent (false until the next start).
    pub loaded: bool,
}
