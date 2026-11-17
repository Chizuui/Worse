//! Core port used by the UI (spec `docs/specs/harness/phase-8-ui.md` §3, §8).
//!
//! The core owns sessions, transcript items, sticky agent, fallback order, agent availability, the run queue,
//! permissions and settings persistence. The UI only sends [`Command`]s and mirrors [`Event`]s.
//! [`hub`] adapts the port onto `harness_core_api::HarnessCore` (the real agent hub, default);
//! [`fake`] is an in-process scripted core for UI work without agent binaries (`HARNESS_UI_CORE=fake`).

pub mod fake;
mod fleet;
pub mod hub;
mod prefs;

pub use fleet::FleetSession;

use std::path::PathBuf;

use harness_core_api as api;
use serde::{Deserialize, Serialize};

pub type AgentId = String;
pub type SessionId = String;
pub type ItemId = u64;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum AgentState {
    Available,
    /// CLI not installed / not detected.
    Unavailable,
    /// The CLI is installed, its ACP adapter is not (phase-9-parity.md §9.10).
    AdapterMissing,
    /// Credit or quota exhausted; `reset` is a display time ("10:00") when the agent reported one.
    Exhausted {
        reset: Option<String>,
    },
}

#[derive(Clone, Debug)]
pub struct AgentStatus {
    pub id: AgentId,
    pub name: String,
    pub state: AgentState,
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct SessionSummary {
    pub id: SessionId,
    pub title: String,
    pub updated_ms: u64,
    pub agents_used: Vec<AgentId>,
    /// Pinned to the top of the sidebar (hub API 0.5.0).
    #[serde(default)]
    pub pinned: bool,
    /// Sidebar group; `None` = ungrouped.
    #[serde(default)]
    pub group_id: Option<String>,
    /// Sort key inside the sidebar section (larger = higher).
    #[serde(default)]
    pub order: i64,
    #[serde(default)]
    pub unread: bool,
    /// Source session id of a fork.
    #[serde(default)]
    pub forked_from: Option<SessionId>,
    /// Imported read-only from `ZCode` or Claude Code history.
    #[serde(default)]
    pub imported: bool,
    /// Workspace folder of the session (projects tree, phase-9-parity.md §9.2.5).
    #[serde(default)]
    pub workspace_path: String,
    /// Project key (`api::ProjectInfo.workspace_key` of `SettingsCall::ListProjects`); groups sessions under their
    /// project.
    #[serde(default)]
    pub project: String,
    #[serde(default)]
    pub branch: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum TurnStatus {
    Streaming,
    Done,
    Cancelled,
    /// Cut off by quota exhaustion; the next agent continues.
    Interrupted,
    Failed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ToolStatus {
    Running,
    AwaitingPermission,
    Done,
    Denied,
    Failed,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum Notice {
    Failover {
        from: AgentId,
        to: AgentId,
        reset: Option<String>,
    },
    Paused {
        reset: Option<String>,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum Item {
    User {
        text: String,
        /// Turn the agent started on its own when a background task ended (§9.17): shown as one muted line,
        /// not as a user bubble.
        #[serde(default)]
        background: bool,
        /// Paths of the images attached to the message.
        #[serde(default)]
        images: Vec<String>,
    },
    Assistant {
        agent: AgentId,
        text: String,
        status: TurnStatus,
    },
    Tool {
        agent: AgentId,
        title: String,
        status: ToolStatus,
    },
    Notice {
        notice: Notice,
    },
}

/// A message waiting in a session's run queue (not yet sent to an agent); `id` is the core's queue key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QueuedMessage {
    pub id: String,
    pub text: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum ThemePref {
    #[default]
    System,
    Light,
    Dark,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Settings {
    pub theme: ThemePref,
    /// "en-US" or "zh-CN".
    pub language: String,
    pub fallback_order: Vec<AgentId>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            theme: ThemePref::System,
            language: "en-US".into(),
            fallback_order: vec!["claude-code".into(), "codex".into(), "agy".into()],
        }
    }
}

// 9.2 的新命令（ChoosePermission、SendContext、Chat）由 B 的 9.2 UI 构造；在其接入前不算死代码错误。
#[allow(dead_code)]
#[derive(Debug)]
pub enum Command {
    /// `agent` is the composer's current choice: the new session starts on it (§6.4). Empty = hub picks.
    /// `workspace` overrides the startup workspace ("Open folder…"); `None` = the workspace the app started in.
    CreateSession {
        agent: AgentId,
        workspace: Option<PathBuf>,
    },
    CreateWorktreeSession {
        agent: AgentId,
        workspace: Option<PathBuf>,
    },
    OpenSession {
        session: SessionId,
    },
    /// The agent is a parameter of every send (one session, many agents).
    Send {
        session: SessionId,
        agent: AgentId,
        text: String,
    },
    Cancel {
        session: SessionId,
    },
    /// Removes one queued (not yet running) message.
    CancelQueued {
        session: SessionId,
        id: String,
    },
    /// Stops a running background task (§9.17). Errors come back as `Event::Error`; the task list as
    /// `Event::BackgroundTasks`.
    StopBackgroundTask {
        session: SessionId,
        id: String,
    },
    /// Answers a permission request with the exact option the agent offered (`Event::PermissionChoices`).
    ChoosePermission {
        session: SessionId,
        request: u64,
        option_id: String,
    },
    /// `Send` with attachments and `@` / `#` mentions (ACP content blocks, phase-9-parity.md §9.2.1).
    SendContext {
        session: SessionId,
        agent: AgentId,
        text: String,
        context: Vec<api::ContextItem>,
    },
    /// Chat and workspace calls (hub API 0.5.0); each carries its own reply channel.
    Chat(ChatCall),
    /// Replaces the text of a queued message (phase-9-parity.md §9.2.5 R12). Errors (`Conflict` once it started)
    /// come back as `Event::Error`; the queue as `Event::Queue`.
    EditQueued {
        session: SessionId,
        id: String,
        text: String,
    },
    /// New queue order; `ids` must list every queued message once.
    ReorderQueued {
        session: SessionId,
        ids: Vec<String>,
    },
    /// Moves a queued message to the head of the queue (it runs next; the running turn is not stopped).
    RunQueuedNow {
        session: SessionId,
        id: String,
    },
    /// Sends while a turn runs: the message goes to the head of the queue (ACP v1 has no mid-turn input).
    Steer {
        session: SessionId,
        agent: AgentId,
        text: String,
        context: Vec<api::ContextItem>,
    },
    /// Mode / model / config option (effort) of a session's next prompts (R13); answered by `Event::SessionOptions`.
    SetMode {
        session: SessionId,
        mode: String,
    },
    SetModel {
        session: SessionId,
        model: String,
    },
    SetConfigOption {
        session: SessionId,
        id: String,
        value: String,
    },
    SetMachine {
        session: SessionId,
        on: bool,
    },
    /// Re-requests `Event::SessionOptions` of a session.
    SessionOptions {
        session: SessionId,
    },
    /// Retry after a failure (R20): `None` restarts the agent hub after `Event::Failure { session: None }`;
    /// `Some(session)` re-runs that session's last failed turn.
    RetryInit {
        session: Option<SessionId>,
    },
    SetStickyAgent {
        session: SessionId,
        agent: AgentId,
    },
    SetFallbackOrder(Vec<AgentId>),
    /// Requests the modes / models / efforts an agent advertised (answered by `Event::AgentOptions`, R7).
    AgentOptions(AgentId),
    /// Requests the Agents overview (answered by `Event::AgentsOverview`, phase-9-parity.md §9.1.5a).
    AgentsOverview,
    /// Runs the connection test of one agent in a separate process (never the pooled one). Answered by
    /// `Event::AgentTested`, then a fresh `Event::AgentsOverview`.
    TestAgent(AgentId),
    /// Extra Claude accounts (phase-9-parity.md §9.6); every call is answered by `Event::Accounts`.
    Accounts(AccountsCall),
    /// Custom API providers (phase-9-parity.md §9.7); every call is answered by `Event::Providers`.
    Providers(ProvidersCall),
    /// Antigravity accounts; every call is answered by `Event::AgyAccounts`.
    AgyAccounts(api::AgyAccountOp),
    /// First-run setup (phase-9-parity.md §9.10): `Status` is answered by `Event::SetupStatus`; `Install` streams
    /// `Event::AdapterInstall` and ends with fresh agents, overview and setup status.
    Setup(SetupCall),
    /// Memory / projects / archive calls of the settings pages; each carries its own reply channel.
    Settings(SettingsCall),
    Shutdown,
}

/// Account calls of the Agents page (phase-9-parity.md §9.6).
#[derive(Debug, Clone)]
pub enum AccountsCall {
    List,
    /// Adds a Claude Code account with this label.
    Add(String),
    Remove(AgentId),
}

/// First-run setup calls (phase-9-parity.md §9.10).
#[derive(Debug, Clone)]
pub enum SetupCall {
    Status,
    /// Installs the ACP adapter of this agent with npm.
    Install(AgentId),
}

/// Provider calls of the Agents page (phase-9-parity.md §9.7). `Debug` never prints the key.
#[derive(Clone)]
pub enum ProvidersCall {
    List,
    Add(api::AddProvider),
    Remove(AgentId),
}

impl std::fmt::Debug for ProvidersCall {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::List => f.write_str("List"),
            // AddProvider 自己的 Debug 已隐藏密钥。
            Self::Add(request) => f.debug_tuple("Add").field(request).finish(),
            Self::Remove(id) => f.debug_tuple("Remove").field(id).finish(),
        }
    }
}

/// Reply channel of a [`SettingsCall`]. `ApiError::Conflict` (memory changed since the plan) and `ApiError::Busy`
/// (a session is running) are expected outcomes the page shows; the call changed nothing then.
pub type Reply<T> = tokio::sync::oneshot::Sender<Result<T, api::ApiError>>;

/// Calls of the Memory, Projects and Archive pages (hub API 0.4.0, phase-9-parity.md §9.1.5a). Every mutation is
/// followed by `Event::Sessions` when the session list changed.
#[derive(Debug)]
pub enum SettingsCall {
    MemoryDoc(api::MemoryScope, Reply<api::MemoryDoc>),
    /// Plan a full replacement; show `plan.diff`, then `MemoryApply(plan)` on confirm (`no_op` plans need no apply).
    MemoryPlan(api::MemoryScope, String, Reply<api::MemoryPlan>),
    MemoryApply(api::MemoryPlan, Reply<()>),
    ListProjects(api::ListProjects, Reply<Vec<api::ProjectInfo>>),
    RemoveProjects {
        keys: Vec<String>,
        delete_data: bool,
        reply: Reply<()>,
    },
    RestoreProjects(Vec<String>, Reply<()>),
    ArchiveSessions(Vec<SessionId>, Reply<()>),
    ListArchived(api::ListArchived, Reply<api::ArchivePage>),
    Unarchive(api::SessionSelection, Reply<u32>),
    DeleteArchived(api::SessionSelection, Reply<u32>),
}

/// Chat and workspace calls (hub API 0.5.0, phase-9-parity.md §9.2, `.work/phase9/API_READY_9.2.md`).
///
/// Turn actions take the item id of any item of that turn in the opened session (user bubble, assistant text, tool
/// card); the port maps it to the hub turn and answers `ApiError::NotFound` when the item is unknown.
/// `Rewind`, `EditResend`, `Retry` and `ApplyFiles` answer `ApiError::Busy` while the session runs, queues or waits
/// for a permission; nothing changed then. After a rewind the port re-opens the session (`Event::SessionOpened`).
#[allow(dead_code)] // constructed by the 9.2 UI (agent B)
#[derive(Debug)]
pub enum ChatCall {
    Rename {
        session: SessionId,
        title: String,
        reply: Reply<()>,
    },
    SetPinned {
        sessions: Vec<SessionId>,
        pinned: bool,
        reply: Reply<()>,
    },
    /// Send `false` when a session is shown and on its `TurnEnded`.
    SetUnread {
        session: SessionId,
        unread: bool,
        reply: Reply<()>,
    },
    ListGroups(Reply<Vec<api::SessionGroup>>),
    CreateGroup {
        name: String,
        color: api::GroupColor,
        reply: Reply<api::SessionGroup>,
    },
    UpdateGroup(api::UpdateGroup, Reply<api::SessionGroup>),
    DeleteGroup(String, Reply<()>),
    /// A permutation of all group ids.
    ReorderGroups(Vec<String>, Reply<()>),
    /// Moves sessions into a group (`None` = ungrouped), on top of that section.
    MoveSessions {
        sessions: Vec<SessionId>,
        group: Option<String>,
        reply: Reply<()>,
    },
    /// Full snapshot of a session for "Export as Markdown".
    Export {
        session: SessionId,
        reply: Reply<api::SessionDetail>,
    },
    /// Copies the finished turns up to `item`'s turn (`None` = all) into a new session.
    Fork {
        session: SessionId,
        item: Option<ItemId>,
        reply: Reply<SessionSummary>,
    },
    /// Removes `item`'s turn and every later turn; the result carries the text and context for the composer.
    Rewind {
        session: SessionId,
        item: ItemId,
        restore_files: bool,
        reply: Reply<api::RewindResult>,
    },
    /// Rewinds to `item`'s turn and sends `text` in its place (`context: None` keeps the original context).
    EditResend {
        session: SessionId,
        item: ItemId,
        text: String,
        context: Option<Vec<api::ContextItem>>,
        agent: Option<AgentId>,
        restore_files: bool,
        reply: Reply<()>,
    },
    /// Re-runs the last turn (`item` = any item of it).
    Retry {
        session: SessionId,
        item: ItemId,
        agent: Option<AgentId>,
        restore_files: bool,
        reply: Reply<()>,
    },
    PlanFiles {
        session: SessionId,
        item: ItemId,
        direction: api::ChangeDirection,
        reply: Reply<api::FileChangePlan>,
    },
    /// `paths: None` = every safe file of the plan; unsafe files only with `include_unsafe`.
    ApplyFiles {
        session: SessionId,
        item: ItemId,
        direction: api::ChangeDirection,
        paths: Option<Vec<String>>,
        include_unsafe: bool,
        reply: Reply<api::FileRestoreReport>,
    },
    /// Find in the opened session; hits carry the item to scroll to.
    Find {
        session: SessionId,
        query: String,
        case_sensitive: bool,
        reply: Reply<Vec<FindMatch>>,
    },
    /// Validates composer chips for the session's active agent without sending.
    ResolveContext {
        session: SessionId,
        items: Vec<api::ContextItem>,
        reply: Reply<Vec<api::ResolvedItem>>,
    },
    /// `@` file / folder and `#` session popup rows.
    Mentions(api::MentionQuery, Reply<Vec<api::MentionCandidate>>),
    /// Slash commands (agent-advertised merged with agent-config commands).
    Commands {
        session: SessionId,
        reply: Reply<api::CommandList>,
    },
    /// Command center (Mod+K) ranking over the UI's actions, sessions and files.
    Search(api::CommandSearch, Reply<api::CommandSearchResult>),
    /// Read-only import of `ZCode` / Claude Code history.
    ImportScan(api::ImportScan, Reply<Vec<api::ImportCandidate>>),
    Import(api::ImportRequest, Reply<api::ImportReport>),
    /// Scheduled prompts (off by default).
    SchedulesEnabled(Reply<bool>),
    SetSchedulesEnabled(bool, Reply<()>),
    ListSchedules(Reply<Vec<api::ScheduledPrompt>>),
    CreateSchedule(api::NewSchedule, Reply<api::ScheduledPrompt>),
    UpdateSchedule(api::UpdateSchedule, Reply<api::ScheduledPrompt>),
    DeleteSchedule(String, Reply<()>),
    RunScheduleNow(String, Reply<api::ScheduleRun>),
    /// Orchestra graph (phase-9-parity.md §9.11).
    GetOrchestra(Reply<api::Orchestra>),
    SetOrchestra(api::Orchestra, Reply<()>),
    Board(String, Reply<api::Board>),
    CreateCard(api::NewCard, Reply<api::CardView>),
    UpdateCard(api::UpdateCard, Reply<api::CardView>),
    MoveCard {
        card: String,
        lane: api::BoardLane,
        index: u32,
        reply: Reply<api::CardView>,
    },
    DeleteCard(String, Reply<()>),
    StartCard(String, Reply<api::CardView>),
    LinkBoard(String, Reply<api::Board>),
    UnlinkBoard(String, Reply<()>),
    SyncBoard(String, Reply<api::Board>),
    InboxStatus(Reply<api::InboxStatus>),
    SetInboxEnabled(bool, Reply<()>),
    SaveInboxSource(api::InboxSourceInput, Reply<api::InboxSource>),
    DeleteInboxSource(String, Reply<()>),
    PollInboxNow(String, Reply<()>),
    Tickets(Reply<Vec<api::Ticket>>),
    Triggers(u32, Reply<Vec<api::TriggerRecord>>),
    PostTicket(String, Reply<()>),
    StopTicket(String, Reply<()>),
    DiscardTicket(String, Reply<()>),
}

/// One find-in-session hit mapped onto the opened transcript.
#[allow(dead_code)] // read by the 9.2 UI (agent B)
#[derive(Clone, Debug)]
pub struct FindMatch {
    /// Item to scroll to (`None` when the turn is not shown, e.g. still queued).
    pub item: Option<ItemId>,
    pub field: api::FindField,
    /// Char offset and length inside the field text.
    pub start: u32,
    pub len: u32,
    pub snippet: String,
}

// 9.2 的新事件字段由 B 的 9.2 UI 读取；在其接入前不算死代码错误。
#[allow(dead_code)]
#[derive(Clone, Debug)]
pub enum Event {
    Settings(Settings),
    /// What an agent advertised (hub `agent_options`); pairs are (id, display name). `known == false` until the
    /// agent was started once (lists are empty then).
    AgentOptions {
        agent: AgentId,
        modes: Vec<(String, String)>,
        models: Vec<(String, String)>,
        efforts: Vec<(String, String)>,
        known: bool,
    },
    Agents(Vec<AgentStatus>),
    /// Agents overview rows (installed, version, login presence, runtime, quota).
    AgentsOverview(Vec<api::AgentOverview>),
    /// Result of `Command::TestAgent`.
    AgentTested(api::AgentTestResult),
    /// Account list after `Command::Accounts`, or the error of the call.
    Accounts(Result<Vec<api::AccountInfo>, String>),
    /// Provider list after `Command::Providers`, or the error of the call.
    Providers(Result<Vec<api::ProviderInfo>, String>),
    /// Antigravity account list after `Command::AgyAccounts`; `sign_in` names the account whose login console
    /// the UI opens now (agy was just signed out for it).
    AgyAccounts {
        sign_in: Option<String>,
        result: Result<api::AgyAccounts, String>,
    },
    /// First-run setup rows after `Command::Setup(Status)` or an install, or the error of the call (§9.10).
    SetupStatus(Result<api::SetupStatus, String>),
    /// Progress of an adapter install: `Running` carries one output line, the final status a message.
    AdapterInstall {
        agent: AgentId,
        status: api::AdapterInstallStatus,
        line: Option<String>,
        message: Option<String>,
    },
    Sessions(Vec<SessionSummary>),
    SessionOpened {
        session: SessionSummary,
        items: Vec<(ItemId, Item)>,
        sticky: AgentId,
        queued: Vec<QueuedMessage>,
        paused: bool,
    },
    ItemAdded {
        session: SessionId,
        id: ItemId,
        item: Item,
    },
    TextDelta {
        session: SessionId,
        id: ItemId,
        text: String,
    },
    TurnEnded {
        session: SessionId,
        id: ItemId,
        status: TurnStatus,
    },
    ToolUpdated {
        session: SessionId,
        id: ItemId,
        status: ToolStatus,
    },
    ToolTitle {
        session: SessionId,
        id: ItemId,
        title: String,
    },
    PermissionRequested {
        session: SessionId,
        request: u64,
        title: String,
        detail: String,
    },
    PermissionCleared {
        session: SessionId,
        request: u64,
    },
    Sticky {
        session: SessionId,
        agent: AgentId,
    },
    Running {
        session: SessionId,
        running: bool,
    },
    Queue {
        session: SessionId,
        items: Vec<QueuedMessage>,
    },
    Paused {
        session: SessionId,
        paused: bool,
    },
    /// Exact answers of a permission request; follows each `PermissionRequested` (answer with `ChoosePermission`).
    PermissionChoices {
        session: SessionId,
        request: u64,
        options: Vec<PermissionChoice>,
    },
    /// Expandable tool card content (R8); replace semantics. Sent after `ItemAdded` / `ToolUpdated` when the agent
    /// sent content, and for every tool card right after `SessionOpened`.
    ToolDetail {
        session: SessionId,
        id: ItemId,
        kind: Option<String>,
        content: Vec<api::ToolContent>,
        locations: Vec<api::ToolLocation>,
        raw_input: Option<String>,
        raw_output: Option<String>,
    },
    /// Thinking delta of the running turn (live only, not persisted).
    Thought {
        session: SessionId,
        text: String,
    },
    /// Agent plan / todo list; replace semantics. Also sent right after `SessionOpened` (empty = none).
    Plan {
        session: SessionId,
        entries: Vec<api::PlanEntry>,
    },
    /// Subagents the session delegated to (phase-9-parity.md §9.8), newest first; replace semantics. Also sent
    /// right after `SessionOpened`.
    Subagents {
        session: SessionId,
        items: Vec<api::SubagentView>,
    },
    Fleet(Vec<FleetSession>),
    /// Background tasks of the session (§9.17), newest first; replace semantics. Also sent right after
    /// `SessionOpened`.
    BackgroundTasks {
        session: SessionId,
        tasks: Vec<api::BackgroundTask>,
    },
    /// The opened session is a subagent of `parent` (`(id, title)`), or a top-level session (`None`). Sent right
    /// after `SessionOpened`.
    ParentLink {
        session: SessionId,
        parent: Option<(String, String)>,
    },
    SessionMeta {
        session: SessionId,
        worktree: Option<api::WorktreeInfo>,
        cost: Option<api::SessionCost>,
        /// Output tokens the agent reported for the session's last turn, when that turn has finished and the
        /// agent reports token counts (generation speed, `speed.rs`).
        tokens: Option<u64>,
    },
    /// Context meter; also sent right after `SessionOpened` when known.
    Usage {
        session: SessionId,
        usage: api::ContextUsage,
    },
    /// The agent's slash commands changed: re-ask `ChatCall::Commands`.
    Commands {
        agent: AgentId,
    },
    /// Groups changed: re-ask `ChatCall::ListGroups`.
    Groups,
    /// Scheduled prompts changed: re-ask `ChatCall::ListSchedules`.
    Schedules,
    Board {
        workspace_key: String,
    },
    Inbox,
    /// A turn's files were restored (undo / reapply); `item` is the turn's user bubble when shown.
    FilesRestored {
        session: SessionId,
        item: Option<ItemId>,
        direction: api::ChangeDirection,
    },
    /// Mode / model / effort pickers of a session (phase-9-parity.md §9.2.5 R13); replace semantics. Sent after
    /// `SessionOpened`, before (`pending`) and after each `Set*` command, when the agent re-advertises, when the
    /// sticky agent changes and when the session starts or stops running (`locked` = a turn runs; a choice then
    /// applies from the next prompt). Pairs are (id, display name); `known == false` until the agent ran once.
    SessionOptions {
        session: SessionId,
        agent: AgentId,
        mode: Option<String>,
        model: Option<String>,
        effort: Option<String>,
        /// Config option id that carries the effort (`effort`, or the agent's `thought_level` option).
        effort_id: Option<String>,
        /// Config option id behind the model picker when the agent lists its models only as a config option
        /// (`None` = the ACP model list, set with `session/set_model`).
        model_id: Option<String>,
        modes: Vec<(String, String)>,
        models: Vec<(String, String)>,
        efforts: Vec<(String, String)>,
        known: bool,
        /// A `Set*` call is in flight.
        pending: bool,
        locked: bool,
        machine: bool,
    },
    /// Typed failure (R20). `session: None` = the agent hub failed to start (`Command::RetryInit { session: None }`
    /// retries); otherwise a turn failed, `item` is its user bubble when shown. Sent before the failed turn's
    /// `TurnEnded` / failed assistant item.
    Failure {
        session: Option<SessionId>,
        item: Option<ItemId>,
        kind: api::FailureKind,
        message: String,
        details: Option<String>,
        trace: Option<String>,
        retry: api::RetryHint,
    },
    Error(String),
}

/// One answer a permission request offers.
#[allow(dead_code)] // read by the 9.2 UI (agent B)
#[derive(Clone, Debug)]
pub struct PermissionChoice {
    pub id: String,
    pub name: String,
    pub kind: api::PermissionOptionKind,
}
