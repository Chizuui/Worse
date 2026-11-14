//! Chat and workspace parity types (API 0.5.0, spec `phase-9-parity.md` §9.2.1).

use serde::{Deserialize, Serialize};

use crate::types::{AgentId, SessionId, SessionSummary, TurnId};
use crate::workspace::FileHit;
use crate::{ApiResult, BoxFuture};

// ---- session organisation -------------------------------------------------------------------------

/// Where a forked session came from (TL-016).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ForkOrigin {
    pub session_id: SessionId,
    /// Last copied turn of the source.
    pub turn_seq: u32,
}

/// The seven sidebar group colours (GRP-002); the UI maps them to theme tokens.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum GroupColor {
    #[default]
    Gray,
    Red,
    Orange,
    Yellow,
    Green,
    Blue,
    Purple,
}

impl GroupColor {
    /// Stable storage name.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Gray => "gray",
            Self::Red => "red",
            Self::Orange => "orange",
            Self::Yellow => "yellow",
            Self::Green => "green",
            Self::Blue => "blue",
            Self::Purple => "purple",
        }
    }

    /// Parses a storage name (unknown → gray).
    #[must_use]
    pub fn parse(value: &str) -> Self {
        match value {
            "red" => Self::Red,
            "orange" => Self::Orange,
            "yellow" => Self::Yellow,
            "green" => Self::Green,
            "blue" => Self::Blue,
            "purple" => Self::Purple,
            _ => Self::Gray,
        }
    }

    /// All colours in picker order.
    pub const ALL: [Self; 7] = [
        Self::Gray,
        Self::Red,
        Self::Orange,
        Self::Yellow,
        Self::Green,
        Self::Blue,
        Self::Purple,
    ];
}

/// A sidebar group.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionGroup {
    pub id: String,
    pub name: String,
    pub color: GroupColor,
    pub position: i64,
}

/// `update_group` request.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateGroup {
    pub id: String,
    pub name: Option<String>,
    pub color: Option<GroupColor>,
}

// ---- turn actions ---------------------------------------------------------------------------------

/// `fork_session` request.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ForkSession {
    pub session_id: SessionId,
    /// Copy turns up to and including this one; `None` = all finished turns.
    pub up_to_turn: Option<TurnId>,
    pub title: Option<String>,
}

/// `rewind_session` request: removes `turn_id` and every later turn.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Rewind {
    pub session_id: SessionId,
    pub turn_id: TurnId,
    /// Also restore the workspace files the removed turns changed (safe files only).
    pub restore_files: bool,
}

/// `rewind_session` result.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RewindResult {
    pub removed_turns: u32,
    /// Text of the first removed turn (back into the composer).
    pub user_text: String,
    pub context: Vec<ContextItem>,
    /// Present when `restore_files` was requested.
    pub files: Option<FileRestoreReport>,
}

/// `edit_and_resend` request.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EditResend {
    pub session_id: SessionId,
    pub turn_id: TurnId,
    pub text: String,
    /// `None` keeps the old turn's context items.
    pub context: Option<Vec<ContextItem>>,
    pub agent: Option<AgentId>,
    pub restore_files: bool,
}

/// `retry_turn` request (last turn only).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Retry {
    pub session_id: SessionId,
    pub turn_id: TurnId,
    pub agent: Option<AgentId>,
    pub restore_files: bool,
}

/// Direction of a file change operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ChangeDirection {
    /// Back to the state before the turn.
    #[default]
    Undo,
    /// Forward to the state after the turn.
    Reapply,
}

/// What restoring does to one path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum FileAction {
    /// Write the target content.
    Restore,
    /// The path does not exist in the target: delete it.
    Delete,
}

/// One path of a [`FileChangePlan`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlannedFile {
    /// Workspace-relative path with `/`.
    pub path: String,
    pub action: FileAction,
    /// The file is still as the turn left it (CHG-003).
    pub safe: bool,
    /// Why it is unsafe.
    pub reason: Option<String>,
}

/// `plan_file_changes` result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileChangePlan {
    pub turn_id: TurnId,
    pub direction: ChangeDirection,
    pub available: bool,
    pub reason: Option<String>,
    pub files: Vec<PlannedFile>,
    /// Every planned file already equals the plan's target (undo: already undone; reapply: already applied).
    /// API 0.5.1, phase-9-parity.md §9.2.5.
    #[serde(default)]
    pub applied: bool,
}

/// `apply_file_changes` request.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ApplyFileChanges {
    pub session_id: SessionId,
    pub turn_id: TurnId,
    pub direction: ChangeDirection,
    /// `None` = every planned path.
    pub paths: Option<Vec<String>>,
    pub include_unsafe: bool,
}

/// What a restore did.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileRestoreReport {
    pub applied: Vec<String>,
    /// `(path, reason)`.
    pub skipped: Vec<(String, String)>,
}

// ---- find ------------------------------------------------------------------------------------------

/// `find_in_session` request.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FindInSession {
    pub session_id: SessionId,
    pub query: String,
    pub case_sensitive: bool,
    /// 0 → 200, max 500.
    pub limit: u32,
}

/// Field a [`FindHit`] is in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum FindField {
    User,
    Assistant,
    /// Tool call title.
    Tool,
}

/// One match.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FindHit {
    pub turn_id: TurnId,
    pub seq: u32,
    pub field: FindField,
    /// Tool call id for [`FindField::Tool`].
    pub tool_call_id: Option<String>,
    /// Start and length in chars of that field.
    pub start: u32,
    pub len: u32,
    /// Up to 40 chars around the match.
    pub snippet: String,
}

// ---- context items (attachments and mentions) -----------------------------------------------------

/// Something attached to a user turn (ATT-*, MEN-002/003).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum ContextItem {
    /// `@file`; absolute or workspace-relative path.
    File { path: String },
    /// `@folder`.
    Folder { path: String },
    /// An image file or pasted image data (base64). Stored items keep only `path`.
    #[serde(rename_all = "camelCase")]
    Image {
        path: Option<String>,
        data_base64: Option<String>,
        mime: String,
    },
    /// Pasted text.
    Text { title: Option<String>, text: String },
    /// `#session` mention.
    #[serde(rename_all = "camelCase")]
    Session { session_id: SessionId },
}

/// Limits of context items (spec §9.2.1).
pub mod limits {
    /// Items per turn.
    pub const MAX_ITEMS: usize = 10;
    /// One image.
    pub const MAX_IMAGE_BYTES: u64 = 5 * 1024 * 1024;
    /// Pasted text.
    pub const MAX_PASTED_BYTES: u64 = 1024 * 1024;
    /// All items of a turn.
    pub const MAX_TOTAL_BYTES: u64 = 20 * 1024 * 1024;
    /// Text files up to this size are embedded (when the agent accepts embedded context).
    pub const MAX_EMBED_BYTES: u64 = 256 * 1024;
    /// Transcript of a mentioned session.
    pub const MAX_SESSION_BYTES: usize = 32 * 1024;
    /// Image mime types accepted.
    pub const IMAGE_MIMES: [&str; 4] = ["image/png", "image/jpeg", "image/gif", "image/webp"];
}

/// ACP block a context item becomes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum BlockKind {
    /// Embedded text resource.
    Resource,
    ResourceLink,
    Image,
    Text,
    /// The agent cannot take it (an image for a text-only agent); a note is sent instead.
    Unsupported,
}

/// `resolve_context` row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResolvedItem {
    pub index: u32,
    pub ok: bool,
    pub error: Option<String>,
    pub block_kind: BlockKind,
    pub bytes: u64,
    /// Chip label (file name, session title, …).
    pub label: String,
}

/// Kind of a mention candidate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum MentionKind {
    File,
    Folder,
    Session,
    /// A skill of the active agent (`harness-agent-config`); `item` is its `SKILL.md` (API 0.5.1).
    Skill,
    /// A subagent of the active agent; the UI inserts `insert_text` and adds no chip (API 0.5.1).
    Subagent,
}

/// `mention_candidates` request.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MentionQuery {
    pub session_id: SessionId,
    pub query: String,
    /// Empty = all kinds.
    pub kinds: Vec<MentionKind>,
    /// 0 → 20, max 50.
    pub limit: u32,
}

/// One mention popup row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MentionCandidate {
    pub kind: MentionKind,
    pub label: String,
    /// Relative path, session workspace, or skill / subagent description.
    pub detail: String,
    /// For `Subagent` only the definition file, for a preview (the mention itself is `insert_text`).
    pub item: ContextItem,
    /// Text the UI inserts into the draft instead of adding a chip (`@agent-<name>` for subagents).
    #[serde(default)]
    pub insert_text: Option<String>,
}

// ---- slash commands -------------------------------------------------------------------------------

/// Where a slash command comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum CommandSource {
    /// ACP `available_commands_update`.
    Agent,
    /// CLI user config (`~/.claude/commands`, Codex prompts).
    User,
    /// CLI workspace config (`<ws>/.claude/commands`).
    Workspace,
    Harness,
}

/// One slash command.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SlashCommand {
    /// Without the leading `/`.
    pub name: String,
    pub description: String,
    pub input_hint: Option<String>,
    pub source: CommandSource,
    pub path: Option<String>,
}

/// `list_commands` result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CommandList {
    pub agent: AgentId,
    /// The agent advertised commands at least once.
    pub known: bool,
    pub commands: Vec<SlashCommand>,
}

// ---- plan, usage, tool content --------------------------------------------------------------------

/// One ACP plan entry (todo row).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlanEntry {
    pub content: String,
    /// `high`, `medium`, `low`.
    pub priority: String,
    /// `pending`, `in_progress`, `completed`.
    pub status: String,
}

/// Cost reported by the agent; `amount` is decimal text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Cost {
    pub amount: String,
    pub currency: String,
}

/// Context window use (ACP `usage_update`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContextUsage {
    pub used: u64,
    pub size: u64,
    pub cost: Option<Cost>,
}

/// Token counts of one prompt (ACP `session/prompt` result `usage`).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TokenUsage {
    pub input: u64,
    pub output: u64,
    pub total: u64,
    pub thought: Option<u64>,
    pub cached_read: Option<u64>,
    pub cached_write: Option<u64>,
}

/// Tool call content (ACP `ToolCallContent`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum ToolContent {
    Text {
        text: String,
    },
    #[serde(rename_all = "camelCase")]
    Diff {
        path: String,
        old_text: Option<String>,
        new_text: String,
    },
    #[serde(rename_all = "camelCase")]
    Terminal {
        terminal_id: String,
    },
}

/// A file location a tool touches.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolLocation {
    pub path: String,
    pub line: Option<u32>,
}

/// Why a turn was sent (TL-014).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnOrigin {
    pub schedule_id: String,
    pub schedule_title: String,
    #[serde(default)]
    pub background: Option<String>,
}

// ---- command center -------------------------------------------------------------------------------

/// What the command center searches.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SearchScope {
    Actions,
    Sessions,
    Files,
}

/// One UI action offered to the ranker.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ActionEntry {
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub keywords: Vec<String>,
}

/// `command_search` request.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CommandSearch {
    pub query: String,
    pub workspace_path: Option<String>,
    /// Empty = all scopes.
    pub scopes: Vec<SearchScope>,
    pub actions: Vec<ActionEntry>,
    /// Per scope; 0 → 10, max 50.
    pub limit: u32,
}

/// A ranked action id.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScoredId {
    pub id: String,
    pub score: u32,
}

/// A ranked session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionHit {
    pub summary: SessionSummary,
    /// Transcript excerpt around the match (empty for a title match).
    pub snippet: String,
    pub score: u32,
}

/// `command_search` result.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CommandSearchResult {
    pub actions: Vec<ScoredId>,
    pub sessions: Vec<SessionHit>,
    pub files: Vec<FileHit>,
    /// False when no file source is injected.
    pub files_available: bool,
}

/// File-name search the hub borrows from the UI's workspace index (single owner of the index).
pub trait FileSearchSource: Send + Sync {
    /// Files and folders of `workspace_path` matching `query`.
    fn search(
        &self,
        workspace_path: String,
        query: String,
        limit: u32,
    ) -> BoxFuture<'_, ApiResult<Vec<FileHit>>>;
}

// ---- import ---------------------------------------------------------------------------------------

/// History source (decision 14A).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ImportSource {
    /// `ZCode` (legacy JSON snapshots and the CLI store).
    Zcode,
    ClaudeCode,
    Codex,
}

impl ImportSource {
    /// Prefix of `imported_from`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Zcode => "zcode",
            Self::ClaudeCode => "claude-code",
            Self::Codex => "codex",
        }
    }
}

/// `import_scan` request.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportScan {
    /// Empty = all sources.
    pub sources: Vec<ImportSource>,
}

/// A session that can be imported.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportCandidate {
    pub source: ImportSource,
    pub external_id: String,
    pub title: String,
    pub workspace_path: String,
    pub turn_count: u32,
    pub updated_at: i64,
    /// Already imported (dedupe).
    pub imported: bool,
}

/// `import_sessions` request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportRequest {
    pub source: ImportSource,
    pub external_ids: Vec<String>,
}

/// `import_sessions` result.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportReport {
    pub imported: Vec<SessionSummary>,
    /// `(external_id, reason)`.
    pub skipped: Vec<(String, String)>,
}

// ---- scheduled prompts ----------------------------------------------------------------------------

/// Where a scheduled prompt goes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum ScheduleTarget {
    #[serde(rename_all = "camelCase")]
    Session { session_id: SessionId },
    /// A new session per run.
    #[serde(rename_all = "camelCase")]
    NewSession {
        workspace_path: String,
        agent: Option<AgentId>,
    },
}

/// When a scheduled prompt runs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum ScheduleKind {
    /// Every `seconds` (≥ the hub minimum).
    Interval { seconds: u32 },
    /// Local time; `weekdays` 1=Mon…7=Sun, empty = every day.
    #[serde(rename_all = "camelCase")]
    Daily {
        minute_of_day: u32,
        weekdays: Vec<u8>,
    },
}

/// One scheduled prompt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScheduledPrompt {
    pub id: String,
    pub title: String,
    pub prompt: String,
    pub target: ScheduleTarget,
    pub kind: ScheduleKind,
    pub enabled: bool,
    /// Unix ms; `None` while disabled.
    pub next_run: Option<i64>,
    pub last_run: Option<i64>,
    pub last_result: Option<String>,
    pub created_at: i64,
}

/// `create_schedule` request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NewSchedule {
    pub title: String,
    pub prompt: String,
    pub target: ScheduleTarget,
    pub kind: ScheduleKind,
    pub enabled: bool,
}

/// `update_schedule` request.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateSchedule {
    pub id: String,
    pub title: Option<String>,
    pub prompt: Option<String>,
    pub target: Option<ScheduleTarget>,
    pub kind: Option<ScheduleKind>,
    pub enabled: Option<bool>,
}

/// `run_schedule_now` result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScheduleRun {
    pub session_id: SessionId,
    pub turn_id: TurnId,
}

// ---- queue edit / steer (API 0.5.1, phase-9-parity.md §9.2.5 R12) -----------------------------------

/// One queued turn in admission order (`EventKind::QueueChanged`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QueuedTurn {
    pub turn_id: TurnId,
    pub user_text: String,
}

/// How a steer was delivered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SteerMode {
    /// A new turn at the head of the queue (runs right after the current one, or now when idle). ACP v1 has no
    /// mid-turn input, so this is the only mode today.
    Queued,
    /// Reserved: injected into the running turn.
    Injected,
}

/// `steer` result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SteerAccepted {
    pub turn_id: TurnId,
    pub seq: u32,
    pub mode: SteerMode,
}

// ---- typed failures (API 0.5.1, phase-9-parity.md §9.2.5 R20) ----------------------------------------

/// What kind of failure ended a turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum FailureKind {
    /// The agent CLI needs a login (ACP `auth_required`).
    Auth,
    /// The agent process exited during the turn.
    ProcessExited,
    /// Spawn, `initialize` or `session/new` / `session/load` failed before the prompt.
    InitFailed,
    /// Rate limit or network error.
    Transient,
    Other,
}

/// What the user can do about a failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "hint", rename_all = "camelCase")]
pub enum RetryHint {
    /// Retry now (the port's `RetryInit { session }` re-runs the turn).
    Retry,
    /// Retry after a delay (transient).
    #[serde(rename_all = "camelCase")]
    After { secs: Option<u64> },
    /// Log in to the agent CLI first, then retry.
    Login,
    /// Retrying will not help.
    No,
}

/// A typed turn failure.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Failure {
    pub kind: FailureKind,
    pub message: String,
    /// JSON-RPC code and `data`, at most 4 KB.
    pub details: Option<String>,
    /// Tail of the agent's stderr, when kept.
    pub trace: Option<String>,
    pub retry: RetryHint,
}
