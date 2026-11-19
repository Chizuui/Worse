//! Adapter from the UI port ([`Command`] / [`Event`]) onto `harness_core_api::HarnessCore`
//! (spec `docs/specs/harness/phase-8-ui.md` §8).
//!
//! The hub owns sessions, turns, the sticky agent, quota/failover, the queue and permissions. This adapter
//! only translates: commands become API calls, hub events become UI items for the one opened session.
//! Theme and language live in the harness-settings store (`<data dir>/settings.json`, single writer, phase-9-parity.md
//! §9.1.2); the fallback order is read from and written to the hub.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use harness_agent_hub::agents::{AgentSpec, ErrorDialect, Transport};
use harness_agent_hub::{Hub, HubConfig};
use harness_core_api::{
    self as api, ApiError, EventKind, HarnessCore, HubEvent, PauseReason, PermissionDecision,
    PermissionOption, PermissionOptionKind, PermissionRequestView, RecvError, SwitchReason,
    ToolCallStatus, ToolCallView, TurnOutcome,
};
use harness_settings::SettingsStore;
use serde::Deserialize;
use tokio::sync::mpsc;

use super::prefs;
use super::{
    AgentId, AgentState, AgentStatus, ChatCall, Command, Event, FindMatch, Item, ItemId, Notice,
    PermissionChoice, QueuedMessage, Reply, SessionSummary, Settings, SettingsCall, ToolStatus,
    TurnStatus,
};

/// One extra agent from `HARNESS_UI_AGENTS` (a JSON file; development and gates only, spec §8).
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AgentOverride {
    id: String,
    name: String,
    program: String,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    env: HashMap<String, String>,
}

#[derive(Default, Clone)]
struct TurnMap {
    /// Assistant item still streaming (taken when it ends).
    assistant: Option<ItemId>,
    /// User bubble and last assistant item shown for the turn (find-in-session targets).
    user: Option<ItemId>,
    reply: Option<ItemId>,
}

struct PendingPermission {
    request_id: api::PermissionRequestId,
    options: Vec<PermissionOption>,
    tool_call_id: String,
}

/// Item bookkeeping of the opened session.
struct View {
    id: api::SessionId,
    next_item: ItemId,
    turns: HashMap<api::TurnId, TurnMap>,
    tools: HashMap<String, ItemId>,
    merged: HashMap<String, ToolCallView>,
    denied: HashSet<String>,
    /// Queued (not yet running) turns in admission order with their user text. Their user bubble is added
    /// when the turn starts; until then they are shown (and cancellable) in the composer's queue list.
    queued: Vec<(api::TurnId, String)>,
    /// Image paths of queued turns, shown on the user bubble once the turn starts.
    queued_images: HashMap<api::TurnId, Vec<String>>,
    /// Queued turns this UI cancelled; their later `TurnFinished` must not end the running turn.
    cancelled: HashSet<api::TurnId>,
    /// Turn of every shown item: turn actions (fork, rewind, retry, files) take an item id (`ChatCall`).
    item_turn: HashMap<ItemId, api::TurnId>,
    /// Last turn that ended with a typed failure (`Command::RetryInit { session: Some }` re-runs it).
    last_failed: Option<api::TurnId>,
    /// A turn of the session runs (the pickers are `locked`).
    running: bool,
    /// `SessionDetail.event_seq` of the snapshot this view was built from: the session's events at or below it are
    /// already shown.
    since: u64,
    /// Subagents of the session (§9.8), newest first.
    subagents: Vec<api::SubagentView>,
}

impl View {
    fn new(id: api::SessionId) -> Self {
        Self {
            last_failed: None,
            running: false,
            since: 0,
            subagents: Vec::new(),
            id,
            next_item: 1,
            turns: HashMap::new(),
            tools: HashMap::new(),
            merged: HashMap::new(),
            denied: HashSet::new(),
            queued: Vec::new(),
            queued_images: HashMap::new(),
            cancelled: HashSet::new(),
            item_turn: HashMap::new(),
        }
    }

    /// Allocates an item that belongs to `turn`.
    fn alloc_in(&mut self, turn: &api::TurnId) -> ItemId {
        let id = self.alloc();
        self.item_turn.insert(id, turn.clone());
        id
    }

    fn merge_tool(&mut self, tool: &ToolCallView) -> ToolCallView {
        let Some(existing) = self.merged.get_mut(&tool.tool_call_id) else {
            self.merged.insert(tool.tool_call_id.clone(), tool.clone());
            return tool.clone();
        };
        if !tool.title.trim().is_empty() {
            existing.title.clone_from(&tool.title);
        }
        if tool.kind.is_some() {
            existing.kind.clone_from(&tool.kind);
        }
        if tool.status.is_some() {
            existing.status.clone_from(&tool.status);
        }
        if tool.raw_input.is_some() {
            existing.raw_input.clone_from(&tool.raw_input);
        }
        if !tool.content.is_empty() {
            existing.content.clone_from(&tool.content);
        }
        if !tool.locations.is_empty() {
            existing.locations.clone_from(&tool.locations);
        }
        if tool.raw_output.is_some() {
            existing.raw_output.clone_from(&tool.raw_output);
        }
        existing.clone()
    }

    fn queue_items(&self) -> Vec<QueuedMessage> {
        self.queued
            .iter()
            .map(|(id, text)| QueuedMessage {
                id: id.0.clone(),
                text: text.clone(),
            })
            .collect()
    }

    /// Removes `turn` from the queue; returns its user text when it was queued.
    fn dequeue(&mut self, turn: &api::TurnId) -> Option<String> {
        let index = self.queued.iter().position(|(id, _)| id == turn)?;
        Some(self.queued.remove(index).1)
    }

    fn alloc(&mut self) -> ItemId {
        let id = self.next_item;
        self.next_item += 1;
        id
    }
}

/// A session option change (`Command::SetMode` / `SetModel` / `SetConfigOption`).
enum Pref {
    Mode(String),
    Model(String),
    Config(String, String),
    Machine(bool),
}

struct Adapter {
    core: Arc<dyn HarnessCore>,
    ev: mpsc::UnboundedSender<Event>,
    workspace: PathBuf,
    settings: Settings,
    agents: Vec<api::AgentInfo>,
    sticky: HashMap<api::SessionId, AgentId>,
    view: Option<View>,
    permissions: HashMap<u64, PendingPermission>,
    next_request: u64,
    fleet: super::fleet::Fleet,
}

fn emit(ev: &mpsc::UnboundedSender<Event>, event: Event) {
    // UI 已退出时发送失败是正常的关停路径，忽略即可。
    let _ = ev.send(event);
}

/// `resets_at` (unix seconds, UTC) as a short local display time (`HH:MM`, or `MM-DD HH:MM` on another day).
fn reset_label(resets_at: Option<i64>) -> Option<String> {
    resets_at
        .map(api::time::local_time_label)
        .filter(|label| !label.is_empty())
}

fn tool_status(status: Option<ToolCallStatus>) -> ToolStatus {
    match status {
        None | Some(ToolCallStatus::Pending | ToolCallStatus::InProgress) => ToolStatus::Running,
        Some(ToolCallStatus::Completed) => ToolStatus::Done,
        Some(ToolCallStatus::Failed) => ToolStatus::Failed,
    }
}

fn outcome_status(outcome: &TurnOutcome) -> TurnStatus {
    match outcome {
        TurnOutcome::Completed { .. } => TurnStatus::Done,
        TurnOutcome::Cancelled => TurnStatus::Cancelled,
        TurnOutcome::Failed { .. } => TurnStatus::Failed,
    }
}

fn summary(s: &api::SessionSummary) -> SessionSummary {
    SessionSummary {
        id: s.id.0.clone(),
        title: s.title.clone(),
        updated_ms: u64::try_from(s.updated_at).unwrap_or(0),
        agents_used: vec![s.active_agent.0.clone()],
        pinned: s.pinned,
        group_id: s.group_id.clone(),
        order: s.order,
        unread: s.unread,
        forked_from: s.forked_from.as_ref().map(|f| f.session_id.0.clone()),
        imported: s.imported_from.is_some(),
        workspace_path: s.workspace_path.clone(),
        project: s.workspace_key.clone(),
        branch: s.worktree.as_ref().map(|w| w.branch.clone()),
    }
}

/// Emits `Event::ToolDetail` when the agent sent anything beyond the title and status.
fn tool_detail(ev: &mpsc::UnboundedSender<Event>, session: &str, id: ItemId, tool: &ToolCallView) {
    let empty = tool.content.is_empty()
        && tool.locations.is_empty()
        && tool.kind.is_none()
        && tool.raw_input.is_none()
        && tool.raw_output.is_none();
    if empty {
        return;
    }
    emit(
        ev,
        Event::ToolDetail {
            session: session.to_owned(),
            id,
            kind: tool.kind.clone(),
            content: tool.content.clone(),
            locations: tool.locations.clone(),
            raw_input: tool.raw_input.clone(),
            raw_output: tool.raw_output.clone(),
        },
    );
}

fn tool_title(ev: &mpsc::UnboundedSender<Event>, session: &str, id: ItemId, title: &str) {
    let title = title.trim();
    if title.is_empty() {
        return;
    }
    emit(
        ev,
        Event::ToolTitle {
            session: session.to_owned(),
            id,
            title: title.to_owned(),
        },
    );
}

const DEFAULT_SESSION_TITLE: &str = "New session";
const TITLE_CHARS: usize = 60;

fn prompt_title(text: &str) -> Option<String> {
    let line = text.lines().map(str::trim).find(|l| !l.is_empty())?;
    let collapsed = line.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.chars().count() <= TITLE_CHARS {
        return Some(collapsed);
    }
    let mut out: String = collapsed.chars().take(TITLE_CHARS - 1).collect();
    out.truncate(out.trim_end().len());
    out.push('…');
    Some(out)
}

const DETAIL_LINES: usize = 6;
const DETAIL_VALUE_CHARS: usize = 160;

fn detail_value(text: &str) -> String {
    let first = text
        .lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .trim();
    let more = first.chars().count() > DETAIL_VALUE_CHARS || text.trim().lines().count() > 1;
    let mut out: String = first.chars().take(DETAIL_VALUE_CHARS).collect();
    if more {
        out.push('…');
    }
    out
}

fn permission_detail(tool: &ToolCallView) -> String {
    let Some(raw) = tool
        .raw_input
        .as_deref()
        .map(str::trim)
        .filter(|r| !r.is_empty())
    else {
        return tool.kind.clone().unwrap_or_default();
    };
    let Ok(serde_json::Value::Object(map)) = serde_json::from_str::<serde_json::Value>(raw) else {
        return raw.to_owned();
    };
    if let Some(description) = map
        .get("description")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|d| !d.is_empty())
    {
        return description.to_owned();
    }
    let lines: Vec<String> = map
        .iter()
        .filter_map(|(key, value)| {
            let text = match value {
                serde_json::Value::String(s) if !s.trim().is_empty() => detail_value(s),
                serde_json::Value::Number(n) => n.to_string(),
                serde_json::Value::Bool(b) => b.to_string(),
                _ => return None,
            };
            Some(format!("{key}: {text}"))
        })
        .take(DETAIL_LINES)
        .collect();
    if lines.is_empty() {
        return tool.kind.clone().unwrap_or_default();
    }
    lines.join("\n")
}

fn with_content_warnings(detail: String, tool: &ToolCallView) -> String {
    let warnings: Vec<String> = tool
        .locations
        .iter()
        .map(|l| Path::new(&l.path))
        .filter(|p| p.is_file())
        .take(4)
        .filter_map(|p| {
            let warning = harness_workspace::filetype::inspect(p).warning?;
            let name = p.file_name().map_or_else(
                || p.display().to_string(),
                |n| n.to_string_lossy().into_owned(),
            );
            Some(format!("⚠ {name}: {warning}"))
        })
        .collect();
    if warnings.is_empty() {
        return detail;
    }
    if detail.is_empty() {
        return warnings.join("\n");
    }
    format!("{detail}\n{}", warnings.join("\n"))
}

/// Find hits per call (the hub caps at 500).
const FIND_LIMIT: u32 = 200;

/// Runs `call` on the runtime and answers on `reply`. A dropped receiver (page closed) is not an error.
fn answer<T: Send + 'static>(
    reply: Reply<T>,
    call: impl std::future::Future<Output = Result<T, ApiError>> + Send + 'static,
) {
    tokio::spawn(async move {
        let _ = reply.send(call.await);
    });
}

fn ids(sessions: Vec<String>) -> Vec<api::SessionId> {
    sessions.into_iter().map(api::SessionId::new).collect()
}

/// Request of `Command::CreateSession`: carries the composer's agent so a new session does not fall back to the
/// hub's own order (which starts with Claude Code) and visibly revert the choice.
/// 修复：`workspace` 为 `Some` 时使用该目录（“Open folder…”选择的文件夹），不再重启应用换工作区。
fn create_request(
    workspace: &Path,
    worktree: bool,
    agent: &AgentId,
    target: Option<&Path>,
) -> api::CreateSession {
    let workspace = target.unwrap_or(workspace);
    api::CreateSession {
        workspace_path: workspace.to_string_lossy().into_owned(),
        worktree: worktree.then(api::WorktreeRequest::default),
        agent: (!agent.is_empty()).then(|| api::AgentId::new(agent)),
        ..api::CreateSession::default()
    }
}

/// Item lookup of the opened session for find hits (copied before the call is spawned).
#[derive(Default)]
struct FindTargets {
    turns: HashMap<api::TurnId, (Option<ItemId>, Option<ItemId>)>,
    tools: HashMap<String, ItemId>,
}

impl FindTargets {
    /// Live transcripts split a reply into several items around tool cards; the hit points at the last one
    /// (exact after a re-open, which shows one reply item per turn).
    fn place(&self, hit: api::FindHit) -> FindMatch {
        let turn = self.turns.get(&hit.turn_id).copied().unwrap_or_default();
        let item = match hit.field {
            api::FindField::User => turn.0,
            api::FindField::Assistant => turn.1,
            api::FindField::Tool => hit
                .tool_call_id
                .as_ref()
                .and_then(|id| self.tools.get(id).copied()),
        };
        FindMatch {
            item,
            field: hit.field,
            start: hit.start,
            len: hit.len,
            snippet: hit.snippet,
        }
    }
}

fn is_reject(kind: PermissionOptionKind) -> bool {
    matches!(
        kind,
        PermissionOptionKind::RejectOnce | PermissionOptionKind::RejectAlways
    )
}

/// Hub configuration: environment defaults, `home` from the UI, per-agent defaults from settings, optional agent
/// override file.
async fn hub_config(home: &Path, store: &SettingsStore) -> Result<HubConfig, String> {
    let mut config = HubConfig::from_env().map_err(|e| e.to_string())?;
    config.home = home.to_path_buf();
    // §9.9：产品只发一个可执行文件；记忆/委派 MCP 与 API 代理都以本程序的模式参数启动（HARNESS_MEMORY_MCP 覆盖除外）。
    let exe = std::env::current_exe().map_err(|e| format!("current executable: {e}"))?;
    config.helpers =
        harness_agent_hub::helpers::HelperLaunch::for_app(exe).map_err(|e| e.to_string())?;
    config.agent_defaults = prefs::agent_defaults(&store.snapshot());
    let policy = config.delegation.clone();
    let fact_check = config.fact_check.clone();
    let apply = move |settings: &harness_settings::Settings| {
        policy.set_limit(usize::from(settings.orchestra.max_concurrent));
        policy.set_nested(settings.orchestra.nested_subagents);
        policy.set_nested_limit(usize::from(settings.orchestra.max_nested_per_subagent));
        fact_check.set_on(settings.research.fact_check);
    };
    apply(&store.snapshot());
    let mut changes = store.subscribe();
    tokio::spawn(async move {
        while changes.changed().await.is_ok() {
            let settings = changes.borrow_and_update().clone();
            apply(&settings);
        }
    });
    // `@` mentions and the command center search files through the workspace index (built on first use).
    config.file_search = Some(harness_agent_hub::SharedFileSearch(Arc::new(
        harness_workspace::WorkspaceFileSearch::new(),
    )));
    config.agy_creds = crate::agy_cred::shared();
    let Some(file) = std::env::var_os("HARNESS_UI_AGENTS").filter(|v| !v.is_empty()) else {
        return Ok(config);
    };
    let bytes = tokio::fs::read(&file)
        .await
        .map_err(|e| format!("HARNESS_UI_AGENTS: {e}"))?;
    let list: Vec<AgentOverride> =
        serde_json::from_slice(&bytes).map_err(|e| format!("HARNESS_UI_AGENTS: {e}"))?;
    config.agents = list
        .into_iter()
        .map(|a| AgentSpec {
            id: api::AgentId::new(a.id),
            display_name: a.name,
            program: a.program,
            args: a.args,
            env: a.env.into_iter().collect(),
            install_hint: "configured in HARNESS_UI_AGENTS".into(),
            transport: Transport::Acp,
            dialect: ErrorDialect::Generic,
        })
        .collect();
    config.default_fallback_order = config.agents.iter().map(|a| a.id.clone()).collect();
    Ok(config)
}

static CORE: std::sync::Mutex<Option<Arc<dyn HarnessCore>>> = std::sync::Mutex::new(None);

fn set_core_handle(core: Option<Arc<dyn HarnessCore>>) {
    let mut slot = CORE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    *slot = core;
}

/// The running hub, for in-process consumers outside the UI port (the remote phone server).
pub fn core_handle() -> Option<Arc<dyn HarnessCore>> {
    CORE.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
}

/// Runs the hub adapter until `Command::Shutdown` or the command channel closes.
pub async fn run(
    home: PathBuf,
    workspace: PathBuf,
    store: Arc<SettingsStore>,
    mut commands: mpsc::UnboundedReceiver<Command>,
    ev: mpsc::UnboundedSender<Event>,
) {
    let core: Arc<dyn HarnessCore> = loop {
        let hub = match hub_config(&home, &store).await {
            Ok(config) => Hub::start(config).await.map_err(|e| e.to_string()),
            Err(error) => Err(error),
        };
        match hub {
            Ok(hub) => break hub,
            Err(error) => {
                emit(
                    &ev,
                    Event::Error(format!("agent hub failed to start: {error}")),
                );
                // 类型化失败（§9.2.5 R20）：UI 显示带重试按钮的横幅；`RetryInit { session: None }` 重新启动 hub。
                emit(
                    &ev,
                    Event::Failure {
                        session: None,
                        item: None,
                        kind: api::FailureKind::InitFailed,
                        message: "agent hub failed to start".into(),
                        details: Some(error),
                        trace: None,
                        retry: api::RetryHint::Retry,
                    },
                );
                // 没有 hub 时仍需等待 UI 关停，否则 core 线程提前退出会让 UI 的命令全部丢失且无提示。
                let mut retry = false;
                while let Some(command) = commands.recv().await {
                    match command {
                        Command::Shutdown => break,
                        Command::RetryInit { session: None } => {
                            retry = true;
                            break;
                        }
                        _ => {}
                    }
                }
                if !retry {
                    return;
                }
            }
        }
    };
    // Agent A's panels (Diff "Turn changes") read turn diffs from the same hub; called inside this runtime.
    crate::panels::attach_core(Arc::clone(&core));
    set_core_handle(Some(Arc::clone(&core)));
    let mut events = core.subscribe();
    let mut store_rx = store.subscribe();
    let snapshot = store.snapshot();
    let mut adapter = Adapter {
        core: Arc::clone(&core),
        ev,
        workspace,
        settings: Settings {
            theme: prefs::theme_of(&snapshot),
            language: prefs::language_tag(snapshot.general.language),
            fallback_order: Vec::new(),
        },
        agents: Vec::new(),
        sticky: HashMap::new(),
        view: None,
        permissions: HashMap::new(),
        next_request: 1,
        fleet: super::fleet::Fleet::default(),
    };
    adapter.startup().await;
    loop {
        tokio::select! {
            command = commands.recv() => match command {
                None | Some(Command::Shutdown) => break,
                Some(command) => adapter.handle(command).await,
            },
            event = events.recv() => match event {
                Ok(event) => adapter.on_event(event).await,
                // 事件丢失时按 API 约定重新拉取快照，而不是在缺口上继续拼接。
                Err(RecvError::Lagged(_)) => adapter.resync().await,
                Err(RecvError::Closed) => break,
            },
            // settings.json changed (UI, external edit): mirror theme/language into Event::Settings. Nothing is
            // written back, so there is no loop.
            changed = store_rx.changed() => {
                if changed.is_err() {
                    continue;
                }
                let snapshot = store_rx.borrow_and_update().clone();
                adapter.on_store(&snapshot);
            }
        }
    }
    set_core_handle(None);
    if let Err(error) = core.shutdown().await {
        emit(&adapter.ev, Event::Error(format!("hub shutdown: {error}")));
    }
}

async fn emit_setup_status(core: &dyn HarnessCore, ev: &mpsc::UnboundedSender<Event>) {
    emit(
        ev,
        Event::SetupStatus(core.setup_status().await.map_err(|e| e.to_string())),
    );
}

async fn emit_overview(core: &dyn HarnessCore, ev: &mpsc::UnboundedSender<Event>) {
    match core.agents_overview().await {
        Ok(rows) => emit(ev, Event::AgentsOverview(rows)),
        Err(error) => emit(ev, Event::Error(format!("agents overview: {error}"))),
    }
}

/// Runs one account call (phase-9-parity.md §9.6) and answers with the fresh list or the error.
async fn accounts_call(
    core: Arc<dyn HarnessCore>,
    ev: mpsc::UnboundedSender<Event>,
    call: super::AccountsCall,
) {
    let done = match call {
        super::AccountsCall::List => Ok(()),
        super::AccountsCall::Add(label) => core
            .add_account(api::AddAccount {
                base: api::AgentId::new("claude-code"),
                label,
            })
            .await
            .map(|_| ()),
        super::AccountsCall::Remove(id) => core.remove_account(api::AgentId::new(id)).await,
    };
    let result = match done {
        Ok(()) => core.list_accounts().await.map_err(|e| e.to_string()),
        Err(error) => Err(error.to_string()),
    };
    emit(&ev, Event::Accounts(result));
}

/// Runs one provider call (phase-9-parity.md §9.7) and answers with the fresh list or the error.
async fn providers_call(
    core: Arc<dyn HarnessCore>,
    ev: mpsc::UnboundedSender<Event>,
    call: super::ProvidersCall,
) {
    let done = match call {
        super::ProvidersCall::List => Ok(()),
        super::ProvidersCall::Add(request) => core.add_provider(request).await.map(|_| ()),
        super::ProvidersCall::Remove(id) => core.remove_provider(api::AgentId::new(id)).await,
    };
    let result = match done {
        Ok(()) => core.list_providers().await.map_err(|e| e.to_string()),
        Err(error) => Err(error.to_string()),
    };
    emit(&ev, Event::Providers(result));
}

/// Runs one settings-page call and answers on its own channel. A dropped receiver (page closed) is not an error.
/// Session-list changes come back as the hub's `SessionsChanged` event, which refreshes `Event::Sessions`.
async fn settings_call(core: &dyn HarnessCore, call: SettingsCall) {
    match call {
        SettingsCall::MemoryDoc(scope, reply) => {
            let _ = reply.send(core.memory_doc(scope).await);
        }
        SettingsCall::MemoryPlan(scope, content, reply) => {
            let _ = reply.send(core.memory_plan(scope, content).await);
        }
        SettingsCall::MemoryApply(plan, reply) => {
            let _ = reply.send(core.memory_apply(plan).await);
        }
        SettingsCall::ListProjects(request, reply) => {
            let _ = reply.send(core.list_projects(request).await);
        }
        SettingsCall::RemoveProjects {
            keys,
            delete_data,
            reply,
        } => {
            let _ = reply.send(core.remove_projects(keys, delete_data).await);
        }
        SettingsCall::RestoreProjects(keys, reply) => {
            let _ = reply.send(core.restore_projects(keys).await);
        }
        SettingsCall::ArchiveSessions(ids, reply) => {
            let ids = ids.into_iter().map(api::SessionId::new).collect();
            let _ = reply.send(core.archive_sessions(ids).await);
        }
        SettingsCall::ListArchived(request, reply) => {
            let _ = reply.send(core.list_archived(request).await);
        }
        SettingsCall::Unarchive(selection, reply) => {
            let _ = reply.send(core.unarchive(selection).await);
        }
        SettingsCall::DeleteArchived(selection, reply) => {
            let _ = reply.send(core.delete_archived(selection).await);
        }
    }
}

impl Adapter {
    fn emit(&self, event: Event) {
        emit(&self.ev, event);
    }

    fn report(&self, what: &str, error: &ApiError) {
        self.emit(Event::Error(format!("{what}: {error}")));
    }

    fn active_id(&self) -> Option<String> {
        self.view.as_ref().map(|v| v.id.0.clone())
    }

    async fn startup(&mut self) {
        self.refresh_settings().await;
        self.refresh_agents().await;
        self.refresh_sessions().await;
    }

    async fn resync(&mut self) {
        self.startup().await;
        if let Some(id) = self.active_id() {
            self.open(&id).await;
        }
    }

    async fn refresh_settings(&mut self) {
        match self.core.get_fallback_order().await {
            Ok(order) => self.settings.fallback_order = order.into_iter().map(|a| a.0).collect(),
            Err(error) => self.report("fallback order", &error),
        }
        self.emit(Event::Settings(self.settings.clone()));
    }

    fn on_store(&mut self, snapshot: &harness_settings::Settings) {
        let theme = prefs::theme_of(snapshot);
        let language = prefs::language_tag(snapshot.general.language);
        if theme != self.settings.theme || language != self.settings.language {
            self.settings.theme = theme;
            self.settings.language = language;
            self.emit(Event::Settings(self.settings.clone()));
        }
    }

    async fn refresh_agents(&mut self) {
        match self.core.list_agents().await {
            Ok(list) => self.agents = list,
            Err(error) => {
                self.report("agents", &error);
                return;
            }
        }
        let statuses = self
            .agents
            .iter()
            .map(|a| AgentStatus {
                id: a.id.0.clone(),
                name: a.display_name.clone(),
                state: if matches!(a.availability, api::Availability::AdapterMissing { .. }) {
                    AgentState::AdapterMissing
                } else if !a.availability.is_available() {
                    AgentState::Unavailable
                } else if let api::QuotaState::Exhausted { resets_at, .. } = &a.quota {
                    AgentState::Exhausted {
                        reset: reset_label(*resets_at),
                    }
                } else {
                    AgentState::Available
                },
            })
            .collect();
        self.emit(Event::Agents(statuses));
    }

    async fn refresh_sessions(&mut self) {
        match self.core.list_sessions(api::ListSessions::default()).await {
            Ok(list) => {
                for s in &list {
                    self.sticky.insert(s.id.clone(), s.active_agent.0.clone());
                }
                self.fleet.set_sessions(&list);
                self.emit(Event::Sessions(list.iter().map(summary).collect()));
                if let Some(fleet) = self.fleet.changed() {
                    self.emit(Event::Fleet(fleet));
                }
            }
            Err(error) => self.report("sessions", &error),
        }
    }

    async fn handle(&mut self, command: Command) {
        match command {
            Command::CreateSession { agent, workspace } => {
                self.create(false, agent, workspace).await;
            }
            Command::CreateWorktreeSession { agent, workspace } => {
                self.create(true, agent, workspace).await;
            }
            Command::OpenSession { session } => self.open(&session).await,
            Command::Send {
                session,
                agent,
                text,
            } => self.send(session, agent, text, Vec::new()).await,
            Command::SendContext {
                session,
                agent,
                text,
                context,
            } => self.send(session, agent, text, context).await,
            Command::ChoosePermission {
                session,
                request,
                option_id,
            } => self.choose(session, request, option_id).await,
            Command::Chat(call) => self.chat(call),
            Command::Cancel { session } => {
                if let Err(error) = self.core.stop(api::SessionId::new(session)).await {
                    self.report("stop", &error);
                }
            }
            Command::CancelQueued { session, id } => self.cancel_queued(session, id).await,
            Command::StopBackgroundTask { session, id } => {
                let result = self
                    .core
                    .stop_background_task(api::SessionId::new(session), id)
                    .await;
                if let Err(error) = result {
                    self.report("stop background task", &error);
                }
            }
            command @ (Command::EditQueued { .. }
            | Command::ReorderQueued { .. }
            | Command::RunQueuedNow { .. }) => self.queue_command(command).await,
            Command::Steer {
                session,
                agent,
                text,
                context,
            } => self.steer(session, agent, text, context).await,
            Command::SetMode { session, mode } => {
                self.set_pref(session, Pref::Mode(mode)).await;
            }
            Command::SetModel { session, model } => {
                self.set_pref(session, Pref::Model(model)).await;
            }
            Command::SetConfigOption { session, id, value } => {
                self.set_pref(session, Pref::Config(id, value)).await;
            }
            Command::SetMachine { session, on } => self.set_pref(session, Pref::Machine(on)).await,
            Command::SessionOptions { session } => self.session_options(&session, false).await,
            Command::RetryInit { session } => self.retry_failed(session).await,
            Command::SetStickyAgent { session, agent } => {
                let result = self
                    .core
                    .set_active_agent(api::SessionId::new(session), api::AgentId::new(agent))
                    .await;
                if let Err(error) = result {
                    self.report("switch agent", &error);
                }
                self.refresh_agents().await;
            }

            Command::SetFallbackOrder(order) => {
                let order = order.into_iter().map(api::AgentId::new).collect();
                if let Err(error) = self.core.set_fallback_order(order).await {
                    self.report("fallback order", &error);
                }
                self.refresh_settings().await;
            }
            command @ (Command::AgentOptions(_)
            | Command::AgentsOverview
            | Command::TestAgent(_)
            | Command::Accounts(_)
            | Command::Providers(_)
            | Command::AgyAccounts(_)
            | Command::Setup(_)
            | Command::Settings(_)) => self.spawn_settings(command),
            Command::Shutdown => {}
        }
    }

    /// Settings-page commands run in their own task: the test may take up to 20 s and the settings calls must not
    /// stall hub event handling.
    fn spawn_settings(&self, command: Command) {
        let (core, ev) = (Arc::clone(&self.core), self.ev.clone());
        match command {
            // 读取选项时 hub 可能先启动 agent 探测（几秒）：放到独立任务里，避免阻塞聊天命令。
            Command::AgentOptions(agent) => drop(tokio::spawn(async move {
                match core.agent_options(api::AgentId::new(agent.clone())).await {
                    Ok(options) => emit(&ev, prefs::agent_options_event(agent, &options)),
                    Err(error) => emit(&ev, Event::Error(format!("agent options: {error}"))),
                }
            })),
            Command::AgentsOverview => drop(tokio::spawn(async move {
                emit_overview(&*core, &ev).await;
            })),
            Command::TestAgent(agent) => drop(tokio::spawn(async move {
                match core.test_agent(api::AgentId::new(agent)).await {
                    Ok(result) => emit(&ev, Event::AgentTested(result)),
                    Err(error) => emit(&ev, Event::Error(format!("agent test: {error}"))),
                }
                emit_overview(&*core, &ev).await;
            })),
            Command::Accounts(call) => drop(tokio::spawn(accounts_call(core, ev, call))),
            Command::Providers(call) => drop(tokio::spawn(providers_call(core, ev, call))),
            Command::AgyAccounts(op) => drop(tokio::spawn(async move {
                let sign_in = match &op {
                    api::AgyAccountOp::SignIn { id } => Some(id.clone()),
                    _ => None,
                };
                let result = core.agy_accounts(op).await.map_err(|e| e.to_string());
                let sign_in = sign_in.filter(|_| result.is_ok());
                emit(&ev, Event::AgyAccounts { sign_in, result });
            })),
            Command::Setup(super::SetupCall::Status) => drop(tokio::spawn(async move {
                emit_setup_status(&*core, &ev).await;
            })),
            // 安装可能持续数分钟：独立任务，不阻塞聊天命令；进度与结束由 hub 的 AdapterInstall 事件送达。
            Command::Setup(super::SetupCall::Install(agent)) => drop(tokio::spawn(async move {
                let result = if agent == "gh" {
                    core.install_gh().await
                } else {
                    core.install_adapter(api::AgentId::new(agent.clone()))
                        .await
                        .map(drop)
                };
                if let Err(error) = result {
                    // 参数错误等不经过 hub 事件，这里补一个失败结束事件，让 UI 退出 "Installing…" 状态。
                    emit(
                        &ev,
                        Event::AdapterInstall {
                            agent,
                            status: api::AdapterInstallStatus::Failed,
                            line: None,
                            message: Some(error.to_string()),
                        },
                    );
                    emit_setup_status(&*core, &ev).await;
                }
            })),
            Command::Settings(call) => drop(tokio::spawn(async move {
                settings_call(&*core, call).await;
            })),
            _ => {}
        }
    }

    /// R12 queue edit / reorder / run-now. The answer is the hub's `QueueChanged` (→ `Event::Queue`).
    async fn queue_command(&self, command: Command) {
        let (what, result) = match command {
            Command::EditQueued { session, id, text } => (
                "edit queued message",
                self.core
                    .edit_queued(api::SessionId::new(session), api::TurnId::new(id), text)
                    .await,
            ),
            Command::ReorderQueued { session, ids } => (
                "reorder queue",
                self.core
                    .reorder_queued(
                        api::SessionId::new(session),
                        ids.into_iter().map(api::TurnId::new).collect(),
                    )
                    .await,
            ),
            Command::RunQueuedNow { session, id } => (
                "run queued message now",
                self.core
                    .run_queued_now(api::SessionId::new(session), api::TurnId::new(id))
                    .await,
            ),
            _ => return,
        };
        if let Err(error) = result {
            self.report(what, &error);
        }
    }

    async fn create(&mut self, worktree: bool, agent: AgentId, workspace: Option<PathBuf>) {
        let target = workspace.as_deref();
        let request = create_request(&self.workspace, worktree, &agent, target);
        let chosen = request.agent.is_some();
        let mut result = self.core.create_session(request).await;
        // 修复：所选 agent 已卸载/失效时不要让新建会话失败，退回 hub 的默认顺序。
        if chosen
            && matches!(
                result,
                Err(api::ApiError::AgentUnavailable(_) | api::ApiError::NotFound(_))
            )
        {
            result = self
                .core
                .create_session(create_request(
                    &self.workspace,
                    worktree,
                    &AgentId::new(),
                    target,
                ))
                .await;
        }
        match result {
            Ok(created) => {
                self.refresh_sessions().await;
                self.open(created.id.as_str()).await;
            }
            Err(error) => self.report("new session", &error),
        }
    }

    fn send_request(
        &self,
        session: String,
        agent: AgentId,
        text: String,
        context: Vec<api::ContextItem>,
    ) -> api::SendMessage {
        // 只有与会话当前粘滞 agent 不同时才作为手动切换传给 hub；相同则交给 hub 的粘滞/故障转移逻辑。
        let current = self
            .sticky
            .get(&api::SessionId::new(session.clone()))
            .cloned();
        let agent = (!agent.is_empty() && current.as_deref() != Some(agent.as_str()))
            .then(|| api::AgentId::new(agent));
        api::SendMessage {
            session_id: api::SessionId::new(session),
            text,
            agent,
            client_message_id: Some(uuid::Uuid::new_v4().to_string()),
            context,
        }
    }

    async fn send(
        &mut self,
        session: String,
        agent: AgentId,
        text: String,
        context: Vec<api::ContextItem>,
    ) {
        let first = self
            .view
            .as_ref()
            .is_some_and(|v| v.id.0 == session && v.turns.is_empty());
        let title = if first { prompt_title(&text) } else { None };
        let request = self.send_request(session.clone(), agent, text, context);
        if let Err(error) = self.core.send_message(request).await {
            self.report("send", &error);
            return;
        }
        if let Some(title) = title {
            self.auto_title(session, title).await;
        }
    }

    async fn auto_title(&self, session: String, title: String) {
        let id = api::SessionId::new(session);
        let current = self
            .core
            .list_sessions(api::ListSessions::default())
            .await
            .unwrap_or_default()
            .into_iter()
            .find(|s| s.id == id)
            .map(|s| s.title);
        let untitled = current
            .as_deref()
            .is_some_and(|t| t.trim().is_empty() || t.trim() == DEFAULT_SESSION_TITLE);
        if !untitled {
            return;
        }
        if let Err(error) = self.core.rename_session(id, title).await {
            self.report("rename session", &error);
        }
    }

    /// Steer = the message goes to the head of the queue (the hub emits `QueueChanged`, mirrored as `Event::Queue`).
    async fn steer(
        &mut self,
        session: String,
        agent: AgentId,
        text: String,
        context: Vec<api::ContextItem>,
    ) {
        let request = self.send_request(session, agent, text, context);
        if let Err(error) = self.core.steer(request).await {
            self.report("steer", &error);
        }
    }

    async fn cancel_queued(&mut self, session: String, id: String) {
        let turn = api::TurnId::new(id);
        if let Err(error) = self
            .core
            .cancel_queued(api::SessionId::new(session.clone()), turn.clone())
            .await
        {
            self.report("cancel queued message", &error);
            return;
        }
        // hub 可能不为排队中被移除的回合发事件，所以本地同步移除。
        // 修复：之后到达的该回合 TurnFinished 已不在队列里，会走正常结束路径，把正在运行的回合标记为结束
        // （Stop 按钮消失）；记录已取消的回合，其 TurnFinished 到达时直接忽略。
        if let Some(view) = self.view.as_mut().filter(|v| v.id.0 == session) {
            if view.dequeue(&turn).is_some() {
                view.cancelled.insert(turn);
                let items = view.queue_items();
                emit(&self.ev, Event::Queue { session, items });
            }
        }
    }

    /// Answers with the exact option the agent offered (the only permission answer path).
    async fn choose(&mut self, session: String, request: u64, option_id: String) {
        let Some(pending) = self.permissions.get(&request) else {
            return;
        };
        if !pending.options.iter().any(|o| o.option_id == option_id) {
            self.emit(Event::Error(format!(
                "the agent did not offer the answer {option_id}"
            )));
            return;
        }
        let request_id = pending.request_id.clone();
        if let Err(error) = self
            .core
            .respond_permission(api::SessionId::new(session), request_id, option_id)
            .await
        {
            self.report("permission", &error);
        }
    }

    /// Hub turn of `item` in the opened session `session`.
    fn turn_of(&self, session: &str, item: ItemId) -> Result<api::TurnId, ApiError> {
        self.view
            .as_ref()
            .filter(|v| v.id.0 == session)
            .and_then(|v| v.item_turn.get(&item).cloned())
            .ok_or_else(|| ApiError::NotFound(format!("item {item} of session {session}")))
    }

    /// Item lookup of the opened session for find hits: turn → (user, reply) and tool call → item.
    fn find_targets(&self, session: &str) -> FindTargets {
        self.view
            .as_ref()
            .filter(|v| v.id.0 == session)
            .map(|v| FindTargets {
                turns: v
                    .turns
                    .iter()
                    .map(|(id, t)| (id.clone(), (t.user, t.reply)))
                    .collect(),
                tools: v.tools.clone(),
            })
            .unwrap_or_default()
    }

    /// Runs one chat call off the event loop; turn items are resolved here, before the call is spawned.
    #[allow(clippy::too_many_lines)]
    fn chat(&self, call: ChatCall) {
        let core = Arc::clone(&self.core);
        match call {
            ChatCall::Rename {
                session,
                title,
                reply,
            } => answer(reply, async move {
                core.rename_session(api::SessionId::new(session), title)
                    .await
            }),
            ChatCall::SetPinned {
                sessions,
                pinned,
                reply,
            } => answer(reply, async move {
                core.set_pinned(ids(sessions), pinned).await
            }),
            ChatCall::SetUnread {
                session,
                unread,
                reply,
            } => answer(reply, async move {
                core.set_unread(api::SessionId::new(session), unread).await
            }),
            ChatCall::ListGroups(reply) => answer(reply, async move { core.list_groups().await }),
            ChatCall::CreateGroup { name, color, reply } => {
                answer(reply, async move { core.create_group(name, color).await });
            }
            ChatCall::UpdateGroup(request, reply) => {
                answer(reply, async move { core.update_group(request).await });
            }
            ChatCall::DeleteGroup(id, reply) => {
                answer(reply, async move { core.delete_group(id).await });
            }
            ChatCall::ReorderGroups(order, reply) => {
                answer(reply, async move { core.reorder_groups(order).await });
            }
            ChatCall::MoveSessions {
                sessions,
                group,
                reply,
            } => answer(reply, async move {
                core.move_sessions(ids(sessions), group).await
            }),
            ChatCall::Export { session, reply } => answer(reply, async move {
                core.open_session(api::SessionId::new(session)).await
            }),
            ChatCall::Fork {
                session,
                item,
                reply,
            } => {
                let up_to_turn = match item.map(|i| self.turn_of(&session, i)).transpose() {
                    Ok(turn) => turn,
                    Err(error) => return drop(reply.send(Err(error))),
                };
                answer(reply, async move {
                    let request = api::ForkSession {
                        session_id: api::SessionId::new(session),
                        up_to_turn,
                        title: None,
                    };
                    core.fork_session(request).await.map(|s| summary(&s))
                });
            }
            ChatCall::Rewind {
                session,
                item,
                restore_files,
                reply,
            } => {
                let turn_id = match self.turn_of(&session, item) {
                    Ok(turn) => turn,
                    Err(error) => return drop(reply.send(Err(error))),
                };
                answer(reply, async move {
                    let request = api::Rewind {
                        session_id: api::SessionId::new(session),
                        turn_id,
                        restore_files,
                    };
                    core.rewind_session(request).await
                });
            }
            ChatCall::EditResend {
                session,
                item,
                text,
                context,
                agent,
                restore_files,
                reply,
            } => {
                let turn_id = match self.turn_of(&session, item) {
                    Ok(turn) => turn,
                    Err(error) => return drop(reply.send(Err(error))),
                };
                answer(reply, async move {
                    let request = api::EditResend {
                        session_id: api::SessionId::new(session),
                        turn_id,
                        text,
                        context,
                        agent: agent.map(api::AgentId::new),
                        restore_files,
                    };
                    core.edit_and_resend(request).await.map(drop)
                });
            }
            ChatCall::Retry {
                session,
                item,
                agent,
                restore_files,
                reply,
            } => {
                let turn_id = match self.turn_of(&session, item) {
                    Ok(turn) => turn,
                    Err(error) => return drop(reply.send(Err(error))),
                };
                answer(reply, async move {
                    let request = api::Retry {
                        session_id: api::SessionId::new(session),
                        turn_id,
                        agent: agent.map(api::AgentId::new),
                        restore_files,
                    };
                    core.retry_turn(request).await.map(drop)
                });
            }
            ChatCall::PlanFiles {
                session,
                item,
                direction,
                reply,
            } => {
                let turn_id = match self.turn_of(&session, item) {
                    Ok(turn) => turn,
                    Err(error) => return drop(reply.send(Err(error))),
                };
                answer(reply, async move {
                    core.plan_file_changes(api::SessionId::new(session), turn_id, direction)
                        .await
                });
            }
            ChatCall::ApplyFiles {
                session,
                item,
                direction,
                paths,
                include_unsafe,
                reply,
            } => {
                let turn_id = match self.turn_of(&session, item) {
                    Ok(turn) => turn,
                    Err(error) => return drop(reply.send(Err(error))),
                };
                answer(reply, async move {
                    let request = api::ApplyFileChanges {
                        session_id: api::SessionId::new(session),
                        turn_id,
                        direction,
                        paths,
                        include_unsafe,
                    };
                    core.apply_file_changes(request).await
                });
            }
            ChatCall::Find {
                session,
                query,
                case_sensitive,
                reply,
            } => {
                let targets = self.find_targets(&session);
                answer(reply, async move {
                    let request = api::FindInSession {
                        session_id: api::SessionId::new(session),
                        query,
                        case_sensitive,
                        limit: FIND_LIMIT,
                    };
                    let hits = core.find_in_session(request).await?;
                    Ok(hits.into_iter().map(|h| targets.place(h)).collect())
                });
            }
            ChatCall::ResolveContext {
                session,
                items,
                reply,
            } => answer(reply, async move {
                core.resolve_context(api::SessionId::new(session), items)
                    .await
            }),
            ChatCall::Mentions(query, reply) => {
                answer(reply, async move { core.mention_candidates(query).await });
            }
            ChatCall::Commands { session, reply } => answer(reply, async move {
                core.list_commands(api::SessionId::new(session)).await
            }),
            ChatCall::Search(request, reply) => {
                answer(reply, async move { core.command_search(request).await });
            }
            ChatCall::ImportScan(request, reply) => {
                answer(reply, async move { core.import_scan(request).await });
            }
            ChatCall::Import(request, reply) => {
                answer(reply, async move { core.import_sessions(request).await });
            }
            ChatCall::SchedulesEnabled(reply) => {
                answer(reply, async move { core.schedules_enabled().await });
            }
            ChatCall::SetSchedulesEnabled(enabled, reply) => {
                answer(
                    reply,
                    async move { core.set_schedules_enabled(enabled).await },
                );
            }
            ChatCall::ListSchedules(reply) => {
                answer(reply, async move { core.list_schedules().await });
            }
            ChatCall::CreateSchedule(request, reply) => {
                answer(reply, async move { core.create_schedule(request).await });
            }
            ChatCall::UpdateSchedule(request, reply) => {
                answer(reply, async move { core.update_schedule(request).await });
            }
            ChatCall::DeleteSchedule(id, reply) => {
                answer(reply, async move { core.delete_schedule(id).await });
            }
            ChatCall::RunScheduleNow(id, reply) => {
                answer(reply, async move { core.run_schedule_now(id).await });
            }
            ChatCall::GetOrchestra(reply) => {
                answer(reply, async move { core.get_orchestra().await });
            }
            ChatCall::SetOrchestra(orchestra, reply) => {
                answer(reply, async move { core.set_orchestra(orchestra).await });
            }
            ChatCall::Board(key, reply) => answer(reply, async move { core.board(key).await }),
            ChatCall::CreateCard(request, reply) => {
                answer(reply, async move { core.create_card(request).await });
            }
            ChatCall::UpdateCard(request, reply) => {
                answer(reply, async move { core.update_card(request).await });
            }
            ChatCall::MoveCard {
                card,
                lane,
                index,
                reply,
            } => answer(
                reply,
                async move { core.move_card(card, lane, index).await },
            ),
            ChatCall::DeleteCard(card, reply) => {
                answer(reply, async move { core.delete_card(card).await });
            }
            ChatCall::StartCard(card, reply) => {
                answer(reply, async move { core.start_card(card).await });
            }
            ChatCall::LinkBoard(key, reply) => {
                answer(reply, async move { core.link_board(key).await });
            }
            ChatCall::UnlinkBoard(key, reply) => {
                answer(reply, async move { core.unlink_board(key).await });
            }
            ChatCall::SyncBoard(key, reply) => {
                answer(reply, async move { core.sync_board(key).await });
            }
            ChatCall::InboxStatus(reply) => answer(reply, async move { core.inbox_status().await }),
            ChatCall::SetInboxEnabled(on, reply) => {
                answer(reply, async move { core.set_inbox_enabled(on).await });
            }
            ChatCall::SaveInboxSource(source, reply) => {
                answer(reply, async move { core.save_inbox_source(source).await });
            }
            ChatCall::DeleteInboxSource(id, reply) => {
                answer(reply, async move { core.delete_inbox_source(id).await });
            }
            ChatCall::PollInboxNow(id, reply) => {
                answer(reply, async move { core.poll_inbox_now(id).await });
            }
            ChatCall::Tickets(reply) => answer(reply, async move { core.list_tickets().await }),
            ChatCall::Triggers(limit, reply) => {
                answer(reply, async move { core.list_triggers(limit).await });
            }
            ChatCall::PostTicket(id, reply) => {
                answer(reply, async move { core.post_ticket(id).await });
            }
            ChatCall::StopTicket(id, reply) => {
                answer(reply, async move { core.stop_ticket(id).await });
            }
            ChatCall::DiscardTicket(id, reply) => {
                answer(reply, async move { core.discard_ticket(id).await });
            }
            ChatCall::ResumableRun { session, reply } => answer(reply, async move {
                core.resumable_run(api::SessionId::new(session)).await
            }),
            ChatCall::ResumeRun { session, reply } => answer(reply, async move {
                core.resume_run(api::SessionId::new(session))
                    .await
                    .map(drop)
            }),
            ChatCall::DiscardRun { session, reply } => answer(reply, async move {
                core.discard_run(api::SessionId::new(session)).await
            }),
        }
    }

    /// Title of a (top-level) session for the subagent bar; falls back to the id.
    async fn session_title(&self, id: &api::SessionId) -> String {
        let list = self
            .core
            .list_sessions(api::ListSessions::default())
            .await
            .unwrap_or_default();
        list.into_iter()
            .find(|s| &s.id == id)
            .map(|s| s.title)
            .filter(|t| !t.trim().is_empty())
            .unwrap_or_else(|| id.0.clone())
    }

    #[allow(clippy::too_many_lines)]
    async fn open(&mut self, session: &str) {
        let detail = match self.core.open_session(api::SessionId::new(session)).await {
            Ok(detail) => detail,
            Err(error) => {
                self.report("open session", &error);
                return;
            }
        };
        self.permissions.clear();
        // The chat now shows this session: the Diff panel's "Turn changes" follows it.
        crate::panels::session_opened(detail.summary.id.clone());
        let mut view = View::new(detail.summary.id.clone());
        view.since = detail.event_seq;
        view.last_failed = detail
            .turns
            .last()
            .filter(|t| t.status == api::TurnStatus::Failed)
            .map(|t| t.id.clone());
        let mut items = Vec::new();
        let mut running = false;
        let mut paused = false;
        for turn in &detail.turns {
            running |= turn.status == api::TurnStatus::Running;
            paused |= turn.status == api::TurnStatus::Paused;
            if detail.queue.contains(&turn.id) {
                // 排队中的消息只显示在输入框上方的队列里（可取消），开始运行时才进入对话。
                view.turns.insert(turn.id.clone(), TurnMap::default());
                continue;
            }
            if turn.stop_reason.as_deref() == Some("steered") {
                view.turns.insert(turn.id.clone(), TurnMap::default());
                continue;
            }
            snapshot_turn(&mut view, turn, &mut items);
        }
        view.queued = detail
            .queue
            .iter()
            .map(|id| {
                let turn = detail.turns.iter().find(|t| &t.id == id);
                let images =
                    turn.map_or_else(Vec::new, |t| api::ContextItem::image_paths(&t.context));
                if !images.is_empty() {
                    view.queued_images.insert(id.clone(), images);
                }
                (
                    id.clone(),
                    turn.map(|t| t.user_text.clone()).unwrap_or_default(),
                )
            })
            .collect();
        let id = detail.summary.id.0.clone();
        let sticky = detail.summary.active_agent.0.clone();
        self.sticky
            .insert(detail.summary.id.clone(), sticky.clone());
        self.emit(Event::SessionOpened {
            session: summary(&detail.summary),
            items,
            sticky,
            queued: view.queue_items(),
            paused,
        });
        view.running = running;
        self.emit(Event::Running {
            session: id.clone(),
            running,
        });
        for turn in &detail.turns {
            for tool in &turn.tool_calls {
                if let Some(&item) = view.tools.get(&tool.tool_call_id) {
                    tool_detail(&self.ev, &id, item, tool);
                }
            }
        }
        self.emit(Event::Plan {
            session: id.clone(),
            entries: detail.plan,
        });
        view.subagents.clone_from(&detail.subagents);
        let prompt = detail
            .turns
            .iter()
            .rev()
            .find(|t| !detail.queue.contains(&t.id) && !t.user_text.is_empty())
            .map_or("", |t| t.user_text.as_str());
        self.fleet.set_open(&id, &detail.subagents, prompt);
        self.fleet.set_running(&id, running);
        if let Some(fleet) = self.fleet.changed() {
            self.emit(Event::Fleet(fleet));
        }
        self.emit(Event::Subagents {
            session: id.clone(),
            items: detail.subagents,
        });
        self.emit(Event::BackgroundTasks {
            session: id.clone(),
            tasks: detail.background_tasks,
        });
        let parent = match &detail.summary.parent_id {
            Some(parent) => Some((parent.0.clone(), self.session_title(parent).await)),
            None => None,
        };
        self.emit(Event::ParentLink {
            session: id.clone(),
            parent,
        });
        self.emit(Event::SessionMeta {
            session: id.clone(),
            worktree: detail.summary.worktree.clone(),
            cost: detail.cost.clone(),
            // Opening a session: no turn has just ended.
            tokens: None,
        });
        if let Some(usage) = detail.context_usage {
            self.emit(Event::Usage { session: id, usage });
        }
        self.view = Some(view);
        for request in detail.pending_permissions {
            self.permission_requested(request);
        }
        self.session_options(detail.summary.id.as_str(), false)
            .await;
    }

    fn permission_requested(&mut self, request: PermissionRequestView) {
        let Some(view) = self.view.as_mut() else {
            return;
        };
        let session = view.id.0.clone();
        let tool_id = request.tool.tool_call_id.clone();
        let tool = view.merge_tool(&request.tool);
        let item = if let Some(&item) = view.tools.get(&tool_id) {
            item
        } else {
            let item = view.alloc_in(&request.turn_id);
            view.tools.insert(tool_id.clone(), item);
            emit(
                &self.ev,
                Event::ItemAdded {
                    session: session.clone(),
                    id: item,
                    item: Item::Tool {
                        agent: request.agent.0.clone(),
                        title: tool.title.clone(),
                        status: ToolStatus::AwaitingPermission,
                    },
                },
            );
            item
        };
        let number = self.next_request;
        self.next_request += 1;
        let detail = with_content_warnings(permission_detail(&tool), &tool);
        self.emit(Event::ToolUpdated {
            session: session.clone(),
            id: item,
            status: ToolStatus::AwaitingPermission,
        });
        tool_title(&self.ev, &session, item, &tool.title);
        self.emit(Event::PermissionRequested {
            session: session.clone(),
            request: number,
            title: tool.title.clone(),
            detail,
        });
        self.emit(Event::PermissionChoices {
            session: session.clone(),
            request: number,
            options: request
                .options
                .iter()
                .map(|o| PermissionChoice {
                    id: o.option_id.clone(),
                    name: o.name.clone(),
                    kind: o.kind,
                })
                .collect(),
        });
        tool_detail(&self.ev, &session, item, &tool);
        self.permissions.insert(
            number,
            PendingPermission {
                request_id: request.request_id,
                options: request.options,
                tool_call_id: tool_id,
            },
        );
    }

    async fn on_event(&mut self, event: HubEvent) {
        if self.fleet.on_event(&event) {
            if let Some(fleet) = self.fleet.changed() {
                self.emit(Event::Fleet(fleet));
            }
        }
        match &event.kind {
            EventKind::SessionCreated { .. }
            | EventKind::SessionUpdated { .. }
            | EventKind::SessionsChanged => {
                self.refresh_sessions().await;
            }
            EventKind::AgentQuota { .. } => self.refresh_agents().await,
            EventKind::CommandsChanged { agent } => self.emit(Event::Commands {
                agent: agent.0.clone(),
            }),
            EventKind::GroupsChanged => self.emit(Event::Groups),
            EventKind::SchedulesChanged => self.emit(Event::Schedules),
            EventKind::BoardChanged { workspace_key } => self.emit(Event::Board {
                workspace_key: workspace_key.clone(),
            }),
            EventKind::InboxChanged => self.emit(Event::Inbox),
            EventKind::AdapterInstall {
                agent,
                status,
                line,
                message,
                ..
            } => {
                self.emit(Event::AdapterInstall {
                    agent: agent.0.clone(),
                    status: *status,
                    line: line.clone(),
                    message: message.clone(),
                });
                // §9.10：安装结束后重新探测，agent 无需重启即可用。
                if *status != api::AdapterInstallStatus::Running {
                    self.refresh_agents().await;
                    emit_overview(&*self.core, &self.ev).await;
                    emit_setup_status(&*self.core, &self.ev).await;
                }
            }
            _ => {}
        }
        // 修复说明：重试/回退/编辑重发后端口会重新打开会话；hub 在快照之前已发布的本会话事件（TurnQueued、
        // TurnStarted…）仍在订阅缓冲里。若照常应用，TurnQueued 会以空映射覆盖快照建立的条目映射并重复添加用户
        // 气泡，之后的 MessageDelta 另起新助手行，快照里的助手行永远停在 Streaming。快照之前的事件已包含在快照中，丢弃。
        let is_open = match (&self.view, &event.session_id) {
            (Some(view), Some(id)) => &view.id == id && event.seq > view.since,
            _ => false,
        };
        if !is_open {
            let open = self.view.as_ref().map(|v| v.id.0.clone());
            if let Some(tool) = open.and_then(|open| self.fleet.subagent_tool(&open, &event)) {
                self.emit(tool);
            }
        }
        if is_open && matches!(event.kind, EventKind::TurnsRemoved { .. }) {
            // 回退/编辑重发/重试删除了回合：已显示的条目和 item→turn 映射都已失效，按快照重建视图。
            if let Some(id) = self.active_id() {
                self.open(&id).await;
            }
            return;
        }
        if is_open {
            let options = matches!(
                event.kind,
                EventKind::TurnStarted { .. }
                    | EventKind::TurnFinished { .. }
                    | EventKind::TurnPaused { .. }
                    | EventKind::SessionOptionsChanged { .. }
                    | EventKind::AgentSwitched { .. }
            );
            let meta = match &event.kind {
                EventKind::TurnFinished { .. } => true,
                EventKind::SubagentUpdated { subagent } => {
                    subagent.finished_at.is_some() && !subagent.status.is_active()
                }
                _ => false,
            };
            self.on_session_event(event.kind);
            if options {
                if let Some(id) = self.active_id() {
                    self.session_options(&id, false).await;
                }
            }
            if meta {
                if let Some(id) = self.active_id() {
                    self.session_meta(&id).await;
                }
            }
        }
    }

    /// Emits `Event::SessionOptions` of `session` (R13). Reads the hub's session lists (cached, no agent start).
    async fn session_meta(&self, session: &str) {
        if let Ok(detail) = self
            .core
            .open_session(api::SessionId::new(session.to_owned()))
            .await
        {
            let tokens = detail
                .turns
                .last()
                .filter(|turn| turn.finished_at.is_some())
                .and_then(|turn| turn.usage.as_ref())
                .map(|usage| usage.output)
                .filter(|output| *output > 0);
            self.emit(Event::SessionMeta {
                session: session.to_owned(),
                worktree: detail.summary.worktree,
                cost: detail.cost,
                tokens,
            });
        }
    }

    async fn session_options(&self, session: &str, pending: bool) {
        let id = api::SessionId::new(session.to_owned());
        let (mode_list, catalog, config, machine) = tokio::join!(
            self.core.list_modes(id.clone()),
            self.core.list_models(id.clone()),
            self.core.list_config_options(id.clone()),
            self.core.machine(id),
        );
        let (mode_list, catalog, config) = match (mode_list, catalog, config) {
            (Ok(m), Ok(n), Ok(c)) => (m, n, c),
            (Err(error), _, _) | (_, Err(error), _) | (_, _, Err(error)) => {
                return self.report("session options", &error);
            }
        };
        let effort = prefs::effort_option(&config.options);
        // No ACP model list: fall back to the agent's `model` config option (newer Claude Code).
        let model_option = if catalog.available.is_empty() {
            prefs::model_option(&config.options)
        } else {
            None
        };
        let locked = self
            .view
            .as_ref()
            .is_some_and(|v| v.id.0 == session && v.running);
        self.emit(Event::SessionOptions {
            session: session.to_owned(),
            agent: mode_list.agent.0.clone(),
            mode: mode_list.current.clone(),
            model: match model_option {
                Some(o) => o.current.clone(),
                None => catalog.current.clone(),
            },
            effort: effort.and_then(|o| o.current.clone()),
            effort_id: effort.map(|o| o.id.clone()),
            model_id: model_option.map(|o| o.id.clone()),
            modes: mode_list
                .available
                .iter()
                .map(|m| (m.id.clone(), m.name.clone()))
                .collect(),
            models: match model_option {
                Some(o) => o
                    .values
                    .iter()
                    .map(|v| (v.value.clone(), v.name.clone()))
                    .collect(),
                None => catalog
                    .available
                    .iter()
                    .map(|m| (m.id.clone(), m.name.clone()))
                    .collect(),
            },
            efforts: effort
                .map(|o| {
                    o.values
                        .iter()
                        .map(|v| (v.value.clone(), v.name.clone()))
                        .collect()
                })
                .unwrap_or_default(),
            known: mode_list.known || catalog.known || config.known,
            pending,
            locked,
            machine: machine.unwrap_or(false),
        });
    }

    /// Mode / model / config option of the session's next prompts: `pending` options, the hub call, then the
    /// result (the hub also emits `SessionOptionsChanged`, which re-sends them).
    async fn set_pref(&mut self, session: String, pref: Pref) {
        self.session_options(&session, true).await;
        let id = api::SessionId::new(session.clone());
        let result = match pref {
            Pref::Mode(mode) => self.core.set_mode(id, mode).await,
            Pref::Model(model) => self.core.set_model(id, model).await,
            Pref::Config(option, value) => self.core.set_config_option(id, option, value).await,
            Pref::Machine(on) => self.core.set_machine(id, on).await,
        };
        if let Err(error) = result {
            self.report("session option", &error);
        }
        self.session_options(&session, false).await;
    }

    /// `RetryInit { Some }` re-runs the session's last failed turn (same agent choice as a plain retry).
    async fn retry_failed(&mut self, session: Option<String>) {
        let Some(session) = session else {
            // hub 已在运行：重启请求只在启动失败的等待循环里处理（见 `run`）。
            return;
        };
        let turn = self
            .view
            .as_ref()
            .filter(|v| v.id.0 == session)
            .and_then(|v| v.last_failed.clone());
        let Some(turn_id) = turn else {
            return self.emit(Event::Error(format!(
                "session {session} has no failed turn to retry"
            )));
        };
        let request = api::Retry {
            session_id: api::SessionId::new(session),
            turn_id,
            agent: None,
            restore_files: false,
        };
        match self.core.retry_turn(request).await {
            Ok(_) => {
                if let Some(view) = self.view.as_mut() {
                    view.last_failed = None;
                }
            }
            Err(error) => self.report("retry", &error),
        }
    }

    #[allow(clippy::too_many_lines)]
    fn on_session_event(&mut self, kind: EventKind) {
        let Some(view) = self.view.as_mut() else {
            return;
        };
        let session = view.id.0.clone();
        let ev = self.ev.clone();
        match kind {
            EventKind::TurnQueued {
                turn_id,
                user_text,
                position,
                background,
                images,
                ..
            } => {
                view.turns.insert(turn_id.clone(), TurnMap::default());
                if position > 0 {
                    if !images.is_empty() {
                        view.queued_images.insert(turn_id.clone(), images);
                    }
                    view.queued.push((turn_id, user_text));
                    emit(
                        &ev,
                        Event::Queue {
                            session,
                            items: view.queue_items(),
                        },
                    );
                } else {
                    let id = view.alloc_in(&turn_id);
                    view.turns.entry(turn_id).or_default().user = Some(id);
                    emit(
                        &ev,
                        Event::ItemAdded {
                            session,
                            id,
                            item: Item::User {
                                text: user_text,
                                background,
                                images,
                            },
                        },
                    );
                }
            }
            EventKind::TurnStarted {
                turn_id, attempt, ..
            } => {
                if let Some(text) = view.dequeue(&turn_id) {
                    let images = view.queued_images.remove(&turn_id).unwrap_or_default();
                    let id = view.alloc_in(&turn_id);
                    view.turns.entry(turn_id.clone()).or_default().user = Some(id);
                    emit(
                        &ev,
                        Event::ItemAdded {
                            session: session.clone(),
                            id,
                            item: Item::User {
                                text,
                                background: false,
                                images,
                            },
                        },
                    );
                }
                let items = view.queue_items();
                let turn = view.turns.entry(turn_id).or_default();
                if let Some(previous) = turn.assistant.take() {
                    if attempt > 1 {
                        emit(
                            &ev,
                            Event::TurnEnded {
                                session: session.clone(),
                                id: previous,
                                status: TurnStatus::Interrupted,
                            },
                        );
                    }
                }
                emit(
                    &ev,
                    Event::Queue {
                        session: session.clone(),
                        items,
                    },
                );
                emit(
                    &ev,
                    Event::Paused {
                        session: session.clone(),
                        paused: false,
                    },
                );
                view.running = true;
                emit(
                    &ev,
                    Event::Running {
                        session,
                        running: true,
                    },
                );
            }
            EventKind::MessageDelta {
                turn_id,
                agent,
                text,
            } => {
                let existing = view.turns.get(&turn_id).and_then(|t| t.assistant);
                let id = if let Some(id) = existing {
                    id
                } else {
                    let id = view.alloc_in(&turn_id);
                    let turn = view.turns.entry(turn_id).or_default();
                    turn.assistant = Some(id);
                    turn.reply = Some(id);
                    emit(
                        &ev,
                        Event::ItemAdded {
                            session: session.clone(),
                            id,
                            item: Item::Assistant {
                                agent: agent.0,
                                text: String::new(),
                                status: TurnStatus::Streaming,
                            },
                        },
                    );
                    id
                };
                emit(&ev, Event::TextDelta { session, id, text });
            }
            EventKind::ToolCall {
                turn_id,
                agent,
                tool,
            } => {
                // 工具调用之后的文字属于新的助手段落，先结束当前段落以保持时间顺序。
                if let Some(previous) = view
                    .turns
                    .entry(turn_id.clone())
                    .or_default()
                    .assistant
                    .take()
                {
                    emit(
                        &ev,
                        Event::TurnEnded {
                            session: session.clone(),
                            id: previous,
                            status: TurnStatus::Done,
                        },
                    );
                }
                let tool = view.merge_tool(&tool);
                if let Some(&item) = view.tools.get(&tool.tool_call_id) {
                    emit(
                        &ev,
                        Event::ToolUpdated {
                            session: session.clone(),
                            id: item,
                            status: tool_status(tool.status),
                        },
                    );
                    tool_title(&ev, &session, item, &tool.title);
                    tool_detail(&ev, &session, item, &tool);
                    return;
                }
                let item = view.alloc_in(&turn_id);
                view.tools.insert(tool.tool_call_id.clone(), item);
                emit(
                    &ev,
                    Event::ItemAdded {
                        session: session.clone(),
                        id: item,
                        item: Item::Tool {
                            agent: agent.0,
                            title: tool.title.clone(),
                            status: tool_status(tool.status),
                        },
                    },
                );
                tool_detail(&ev, &session, item, &tool);
            }
            EventKind::ToolCallUpdated { tool, .. } => {
                let tool = view.merge_tool(&tool);
                if let Some(&item) = view.tools.get(&tool.tool_call_id) {
                    let status = if view.denied.contains(&tool.tool_call_id) {
                        ToolStatus::Denied
                    } else {
                        tool_status(tool.status)
                    };
                    emit(
                        &ev,
                        Event::ToolUpdated {
                            session: session.clone(),
                            id: item,
                            status,
                        },
                    );
                    tool_title(&ev, &session, item, &tool.title);
                    tool_detail(&ev, &session, item, &tool);
                }
            }
            EventKind::ThoughtDelta { text, .. } => emit(&ev, Event::Thought { session, text }),
            EventKind::PlanUpdated { entries, .. } => emit(&ev, Event::Plan { session, entries }),
            EventKind::SubagentUpdated { subagent } => {
                match view
                    .subagents
                    .iter_mut()
                    .find(|s| s.session_id == subagent.session_id)
                {
                    Some(slot) => *slot = subagent,
                    None => view.subagents.insert(0, subagent),
                }
                emit(
                    &ev,
                    Event::Subagents {
                        session,
                        items: view.subagents.clone(),
                    },
                );
            }
            EventKind::UsageUpdated { usage, .. } => emit(&ev, Event::Usage { session, usage }),
            EventKind::BackgroundTasksChanged { tasks } => emit(
                &ev,
                Event::BackgroundTasks {
                    session,
                    tasks: tasks.clone(),
                },
            ),
            EventKind::FilesRestored { turn_id, direction } => {
                let item = view.turns.get(&turn_id).and_then(|t| t.user);
                emit(
                    &ev,
                    Event::FilesRestored {
                        session,
                        item,
                        direction,
                    },
                );
            }
            EventKind::PermissionRequested { request } => self.permission_requested(request),
            EventKind::PermissionResolved {
                request_id,
                decision,
                ..
            } => self.permission_resolved(&request_id, &decision),
            EventKind::TurnFinished {
                turn_id,
                agent,
                outcome,
            } => {
                if view.cancelled.remove(&turn_id) {
                    return;
                }
                if view.dequeue(&turn_id).is_some() {
                    // 排队中被取消的回合从未运行：只更新队列，不能把正在运行的回合标记为结束。
                    emit(
                        &ev,
                        Event::Queue {
                            session,
                            items: view.queue_items(),
                        },
                    );
                    return;
                }
                let status = outcome_status(&outcome);
                let assistant = view
                    .turns
                    .get_mut(&turn_id)
                    .and_then(|t| t.assistant.take());
                if let Some(id) = assistant {
                    emit(
                        &ev,
                        Event::TurnEnded {
                            session: session.clone(),
                            id,
                            status,
                        },
                    );
                } else if let TurnOutcome::Failed { error, .. } = &outcome {
                    let id = view.alloc_in(&turn_id);
                    view.turns.entry(turn_id.clone()).or_default().reply = Some(id);
                    emit(
                        &ev,
                        Event::ItemAdded {
                            session: session.clone(),
                            id,
                            item: Item::Assistant {
                                agent: agent.map(|a| a.0).unwrap_or_default(),
                                text: error.clone(),
                                status: TurnStatus::Failed,
                            },
                        },
                    );
                }
                view.running = false;
                emit(
                    &ev,
                    Event::Running {
                        session,
                        running: false,
                    },
                );
            }
            EventKind::TurnPaused { turn_id, reason } => {
                let PauseReason::AllAgentsExhausted { agents } = reason;
                let earliest = agents.iter().filter_map(|(_, r)| *r).min();
                if let Some(previous) = view
                    .turns
                    .get_mut(&turn_id)
                    .and_then(|t| t.assistant.take())
                {
                    emit(
                        &ev,
                        Event::TurnEnded {
                            session: session.clone(),
                            id: previous,
                            status: TurnStatus::Interrupted,
                        },
                    );
                }
                let id = view.alloc_in(&turn_id);
                emit(
                    &ev,
                    Event::ItemAdded {
                        session: session.clone(),
                        id,
                        item: Item::Notice {
                            notice: Notice::Paused {
                                reset: reset_label(earliest),
                            },
                        },
                    },
                );
                view.running = false;
                emit(
                    &ev,
                    Event::Running {
                        session: session.clone(),
                        running: false,
                    },
                );
                emit(
                    &ev,
                    Event::Paused {
                        session,
                        paused: true,
                    },
                );
            }
            EventKind::AgentSwitched {
                from, to, reason, ..
            } => {
                if let (SwitchReason::QuotaExhausted { resets_at }, Some(from)) = (&reason, from) {
                    let id = view.alloc();
                    emit(
                        &ev,
                        Event::ItemAdded {
                            session: session.clone(),
                            id,
                            item: Item::Notice {
                                notice: Notice::Failover {
                                    from: from.0,
                                    to: to.0.clone(),
                                    reset: reset_label(*resets_at),
                                },
                            },
                        },
                    );
                }
                self.sticky.insert(view.id.clone(), to.0.clone());
                emit(
                    &ev,
                    Event::Sticky {
                        session,
                        agent: to.0,
                    },
                );
            }
            EventKind::SteerInjected {
                turn_id,
                steered_turn_id,
                text,
            } => {
                view.turns.entry(steered_turn_id).or_default();
                if let Some(previous) = view
                    .turns
                    .entry(turn_id.clone())
                    .or_default()
                    .assistant
                    .take()
                {
                    emit(
                        &ev,
                        Event::TurnEnded {
                            session: session.clone(),
                            id: previous,
                            status: TurnStatus::Done,
                        },
                    );
                }
                let id = view.alloc_in(&turn_id);
                emit(
                    &ev,
                    Event::ItemAdded {
                        session,
                        id,
                        item: Item::User {
                            text,
                            background: false,
                            images: Vec::new(),
                        },
                    },
                );
            }
            EventKind::QueueChanged { queue } => {
                // hub 队列是唯一事实来源（替换语义）：按其顺序重建本地排队列表，不在 UI 侧推算顺序。
                view.queued = queue
                    .into_iter()
                    .map(|q| (q.turn_id, q.user_text))
                    .collect();
                for (turn, _) in &view.queued {
                    view.turns.entry(turn.clone()).or_default();
                }
                emit(
                    &ev,
                    Event::Queue {
                        session,
                        items: view.queue_items(),
                    },
                );
            }
            EventKind::TurnFailure {
                turn_id, failure, ..
            } => {
                view.last_failed = Some(turn_id.clone());
                let item = view.turns.get(&turn_id).and_then(|t| t.user);
                emit(
                    &ev,
                    Event::Failure {
                        session: Some(session),
                        item,
                        kind: failure.kind,
                        message: failure.message,
                        details: failure.details,
                        trace: failure.trace,
                        retry: failure.retry,
                    },
                );
            }
            _ => {}
        }
    }

    fn permission_resolved(
        &mut self,
        request_id: &api::PermissionRequestId,
        decision: &PermissionDecision,
    ) {
        let Some(view) = self.view.as_mut() else {
            return;
        };
        let session = view.id.0.clone();
        let Some((&number, pending)) = self
            .permissions
            .iter()
            .find(|(_, p)| &p.request_id == request_id)
        else {
            return;
        };
        let tool = pending.tool_call_id.clone();
        self.permissions.remove(&number);
        emit(
            &self.ev,
            Event::PermissionCleared {
                session: session.clone(),
                request: number,
            },
        );
        let status = match decision {
            PermissionDecision::Selected { kind, .. } if is_reject(*kind) => {
                view.denied.insert(tool.clone());
                ToolStatus::Denied
            }
            PermissionDecision::Selected { .. } => ToolStatus::Running,
            PermissionDecision::Cancelled => ToolStatus::Failed,
        };
        if let Some(&item) = view.tools.get(&tool) {
            emit(
                &self.ev,
                Event::ToolUpdated {
                    session,
                    id: item,
                    status,
                },
            );
        }
    }
}

/// Turns one snapshot turn into UI items (user text, earlier attempts + failover notices, tools, reply).
fn snapshot_turn(view: &mut View, turn: &api::TurnView, items: &mut Vec<(ItemId, Item)>) {
    let start = items.len();
    snapshot_turn_items(view, turn, items);
    let ended = !matches!(
        turn.status,
        api::TurnStatus::Running | api::TurnStatus::Queued | api::TurnStatus::Paused
    );
    if !ended {
        return;
    }
    let cut = turn.status != api::TurnStatus::Completed;
    let mut marked = false;
    for (_, item) in &mut items[start..] {
        match item {
            Item::Tool { status, .. }
                if matches!(status, ToolStatus::Running | ToolStatus::AwaitingPermission) =>
            {
                *status = ToolStatus::Failed;
            }
            Item::Assistant { status, .. } => marked = *status != TurnStatus::Done,
            _ => {}
        }
    }
    if cut && !marked {
        let status = match turn.status {
            api::TurnStatus::Cancelled => TurnStatus::Cancelled,
            api::TurnStatus::Failed => TurnStatus::Failed,
            _ => TurnStatus::Interrupted,
        };
        let id = view.alloc_in(&turn.id);
        items.push((
            id,
            Item::Assistant {
                agent: turn.agent.as_ref().map(|a| a.0.clone()).unwrap_or_default(),
                text: String::new(),
                status,
            },
        ));
    }
}

#[allow(clippy::too_many_lines)]
fn snapshot_turn_items(view: &mut View, turn: &api::TurnView, items: &mut Vec<(ItemId, Item)>) {
    let user = view.alloc_in(&turn.id);
    items.push((
        user,
        Item::User {
            text: turn.user_text.clone(),
            background: turn.origin.as_ref().is_some_and(|o| o.background.is_some()),
            images: api::ContextItem::image_paths(&turn.context),
        },
    ));
    let last = turn.attempts.len().saturating_sub(1);
    for (index, attempt) in turn.attempts.iter().enumerate().take(last) {
        if !attempt.partial_text.is_empty() {
            let id = view.alloc_in(&turn.id);
            items.push((
                id,
                Item::Assistant {
                    agent: attempt.agent.0.clone(),
                    text: attempt.partial_text.clone(),
                    status: TurnStatus::Interrupted,
                },
            ));
        }
        if let (Some(api::ErrorClass::QuotaExhausted { resets_at }), Some(next)) =
            (&attempt.error_class, turn.attempts.get(index + 1))
        {
            let id = view.alloc_in(&turn.id);
            items.push((
                id,
                Item::Notice {
                    notice: Notice::Failover {
                        from: attempt.agent.0.clone(),
                        to: next.agent.0.clone(),
                        reset: reset_label(*resets_at),
                    },
                },
            ));
        }
    }
    let denied: HashSet<&str> = turn
        .permissions
        .iter()
        .filter_map(|(title, decision)| match decision {
            PermissionDecision::Selected { kind, .. } if is_reject(*kind) => Some(title.as_str()),
            _ => None,
        })
        .collect();
    let agent = turn.agent.as_ref().map(|a| a.0.clone()).unwrap_or_default();
    let mut map = TurnMap {
        user: Some(user),
        ..TurnMap::default()
    };
    let status = match turn.status {
        api::TurnStatus::Running => Some(TurnStatus::Streaming),
        api::TurnStatus::Completed => Some(TurnStatus::Done),
        api::TurnStatus::Cancelled => Some(TurnStatus::Cancelled),
        api::TurnStatus::Failed => Some(TurnStatus::Failed),
        api::TurnStatus::Interrupted => Some(TurnStatus::Interrupted),
        api::TurnStatus::Queued | api::TurnStatus::Paused => None,
    };
    let interleaved = turn
        .segments
        .iter()
        .any(|s| matches!(s, api::TurnSegment::Text { text } if !text.trim().is_empty()));
    if interleaved {
        snapshot_segments(view, turn, &agent, &denied, status, &mut map, items);
        if turn.status == api::TurnStatus::Paused {
            let id = view.alloc_in(&turn.id);
            items.push((
                id,
                Item::Notice {
                    notice: Notice::Paused { reset: None },
                },
            ));
        }
        view.turns.insert(turn.id.clone(), map);
        return;
    }
    for tool in &turn.tool_calls {
        let id = view.alloc_in(&turn.id);
        view.tools.insert(tool.tool_call_id.clone(), id);
        view.merged.insert(tool.tool_call_id.clone(), tool.clone());
        items.push((
            id,
            snapshot_tool(&agent, tool, denied.contains(tool.title.as_str())),
        ));
    }
    for segment in &turn.segments {
        if let api::TurnSegment::Steer { text } = segment {
            let id = view.alloc_in(&turn.id);
            items.push((
                id,
                Item::User {
                    text: text.clone(),
                    background: false,
                    images: Vec::new(),
                },
            ));
        }
    }
    if let Some(status) = status {
        let text = if turn.status == api::TurnStatus::Failed && turn.assistant_text.is_empty() {
            turn.error.clone().unwrap_or_default()
        } else {
            turn.assistant_text.clone()
        };
        if !text.is_empty() || status == TurnStatus::Streaming {
            let id = view.alloc_in(&turn.id);
            items.push((
                id,
                Item::Assistant {
                    agent: agent.clone(),
                    text,
                    status,
                },
            ));
            map.reply = Some(id);
            if status == TurnStatus::Streaming {
                map.assistant = Some(id);
            }
        }
    }
    if turn.status == api::TurnStatus::Paused {
        let id = view.alloc_in(&turn.id);
        items.push((
            id,
            Item::Notice {
                notice: Notice::Paused { reset: None },
            },
        ));
    }
    view.turns.insert(turn.id.clone(), map);
}

#[allow(clippy::too_many_lines)]
fn snapshot_segments(
    view: &mut View,
    turn: &api::TurnView,
    agent: &str,
    denied: &HashSet<&str>,
    status: Option<TurnStatus>,
    map: &mut TurnMap,
    items: &mut Vec<(ItemId, Item)>,
) {
    let mut placed: HashSet<&str> = HashSet::new();
    let mut pending = String::new();
    let mut last_text: Option<usize> = None;
    let push_tool = |view: &mut View, items: &mut Vec<(ItemId, Item)>, tool: &ToolCallView| {
        let id = view.alloc_in(&turn.id);
        view.tools.insert(tool.tool_call_id.clone(), id);
        view.merged.insert(tool.tool_call_id.clone(), tool.clone());
        items.push((
            id,
            snapshot_tool(agent, tool, denied.contains(tool.title.as_str())),
        ));
    };
    let flush = |view: &mut View,
                 items: &mut Vec<(ItemId, Item)>,
                 pending: &mut String,
                 last_text: &mut Option<usize>| {
        let text = pending.trim();
        if text.is_empty() {
            pending.clear();
            return;
        }
        let id = view.alloc_in(&turn.id);
        items.push((
            id,
            Item::Assistant {
                agent: agent.to_string(),
                text: text.to_string(),
                status: TurnStatus::Done,
            },
        ));
        *last_text = Some(items.len() - 1);
        pending.clear();
    };
    for segment in &turn.segments {
        match segment {
            api::TurnSegment::Text { text } => pending.push_str(text),
            api::TurnSegment::Tool { tool_call_id } => {
                flush(view, items, &mut pending, &mut last_text);
                if let Some(tool) = turn
                    .tool_calls
                    .iter()
                    .find(|t| &t.tool_call_id == tool_call_id)
                {
                    if placed.insert(tool.tool_call_id.as_str()) {
                        push_tool(view, items, tool);
                    }
                }
            }
            api::TurnSegment::Steer { text } => {
                flush(view, items, &mut pending, &mut last_text);
                let id = view.alloc_in(&turn.id);
                items.push((
                    id,
                    Item::User {
                        text: text.clone(),
                        background: false,
                        images: Vec::new(),
                    },
                ));
            }
        }
    }
    let trailing_text = !pending.trim().is_empty();
    for tool in &turn.tool_calls {
        if !placed.contains(tool.tool_call_id.as_str()) {
            flush(view, items, &mut pending, &mut last_text);
            push_tool(view, items, tool);
        }
    }
    flush(view, items, &mut pending, &mut last_text);
    let Some(status) = status else {
        return;
    };
    let ends_with_text = trailing_text && last_text == Some(items.len() - 1);
    match last_text {
        Some(index) if ends_with_text || status != TurnStatus::Streaming => {
            let (id, item) = &mut items[index];
            if let Item::Assistant { status: s, .. } = item {
                *s = status;
            }
            map.reply = Some(*id);
            if status == TurnStatus::Streaming {
                map.assistant = Some(*id);
            }
        }
        _ => {
            let text = if status == TurnStatus::Failed {
                turn.error.clone().unwrap_or_default()
            } else {
                String::new()
            };
            if text.is_empty() && status != TurnStatus::Streaming {
                return;
            }
            let id = view.alloc_in(&turn.id);
            items.push((
                id,
                Item::Assistant {
                    agent: agent.to_string(),
                    text,
                    status,
                },
            ));
            map.reply = Some(id);
            if status == TurnStatus::Streaming {
                map.assistant = Some(id);
            }
        }
    }
}

fn snapshot_tool(agent: &str, tool: &ToolCallView, denied: bool) -> Item {
    Item::Tool {
        agent: agent.to_string(),
        title: tool.title.clone(),
        status: if denied {
            ToolStatus::Denied
        } else {
            tool_status(tool.status)
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A composer choice made before any session exists must survive into the created session instead of the hub
    /// picking its default order (Claude Code first).
    #[test]
    fn create_session_passes_the_composer_agent() {
        let request = create_request(Path::new("C:/w"), false, &"codex".to_string(), None);
        assert_eq!(request.agent, Some(api::AgentId::new("codex")));
        assert_eq!(request.workspace_path, "C:/w");
        assert!(request.worktree.is_none());
    }

    #[test]
    fn create_session_without_a_choice_lets_the_hub_pick() {
        let request = create_request(Path::new("C:/w"), true, &String::new(), None);
        assert_eq!(request.agent, None);
        assert!(request.worktree.is_some());
    }

    /// “Open folder…”：选中的文件夹作为新会话的工作区，而不是进程启动时的工作区。
    #[test]
    fn create_session_uses_the_chosen_folder() {
        let request = create_request(
            Path::new("C:/startup"),
            false,
            &"codex".to_string(),
            Some(Path::new("D:/picked/project")),
        );
        assert_eq!(request.workspace_path, "D:/picked/project");
        assert!(request.worktree.is_none());
        assert_eq!(request.agent, Some(api::AgentId::new("codex")));
    }
}
