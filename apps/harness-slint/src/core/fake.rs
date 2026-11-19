//! Round-1 local fake core: real session/queue/failover/permission/persistence logic, scripted fake agents.
//!
//! Agents are scripted (no CLI is spawned). `HARNESS_FAKE_AGENTS` lists the available agent ids
//! (default `claude-code,codex`; `agy` then shows as not installed). Prompt keywords drive the script:
//! `permission` asks for a write permission, `quota` exhausts the agent mid-stream (failover),
//! `slow` streams a long reply slowly (to test Stop). Anything else: a tool call + a markdown reply.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use harness_settings::SettingsStore;
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, oneshot, watch};

use super::prefs;

mod admin;
mod chat;
mod queue;

use super::{
    AgentId, AgentState, AgentStatus, Command, Event, Item, ItemId, Notice, QueuedMessage,
    SessionId, SessionSummary, Settings, ToolStatus, TurnStatus,
};

const KNOWN_AGENTS: [(&str, &str); 3] = [
    ("claude-code", "Claude Code"),
    ("codex", "Codex"),
    ("agy", "agy"),
];

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SessionFile {
    summary: SessionSummary,
    sticky: AgentId,
    items: Vec<Item>,
}

struct Run {
    id: u64,
    agent: AgentId,
    prompt: String,
    assistant: Option<ItemId>,
    tool: Option<ItemId>,
    cancel: watch::Sender<bool>,
    permission: Option<(u64, oneshot::Sender<bool>)>,
}

struct Session {
    summary: SessionSummary,
    items: Vec<(ItemId, Item)>,
    next_item: ItemId,
    sticky: AgentId,
    queue: VecDeque<QueuedMessage>,
    next_queue: u64,
    run: Option<Run>,
    paused: bool,
}

impl Session {
    fn queue_id(&mut self) -> String {
        let id = format!("q{}", self.next_queue);
        self.next_queue += 1;
        id
    }

    fn push(&mut self, item: Item) -> ItemId {
        let id = self.next_item;
        self.next_item += 1;
        self.items.push((id, item));
        id
    }

    fn item_mut(&mut self, id: ItemId) -> Option<&mut Item> {
        self.items
            .iter_mut()
            .find(|(item_id, _)| *item_id == id)
            .map(|(_, item)| item)
    }
}

enum AgentOut {
    Delta(String),
    ToolStart(String),
    ToolEnd(ToolStatus),
    Permission {
        title: String,
        detail: String,
        reply: oneshot::Sender<bool>,
    },
    Exhausted {
        reset: Option<String>,
    },
    Finished {
        cancelled: bool,
    },
}

struct AgentMsg {
    run: u64,
    session: SessionId,
    out: AgentOut,
}

struct Job {
    run: u64,
    session: SessionId,
    agent: AgentId,
    name: String,
    prompt: String,
    continuation: bool,
}

/// What the fake core persists itself (only the fallback order; theme/language belong to the settings store).
#[derive(Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct FakeState {
    fallback_order: Option<Vec<AgentId>>,
}

/// 修复说明：fake core 原先把整个 Settings 写进 `<home>/settings.json`，而该文件现在归 harness-settings
/// 存储所有（唯一写者，phase-9-parity.md §9.1.2）；每次改主题/语言/fallback order 都会覆盖整个文件并丢掉
/// 存储的键。改为仅把 fallback order 存到 `fake-core.json`，主题/语言经存储读写。
const FAKE_STATE_FILE: &str = "fake-core.json";

pub struct FakeCore {
    sessions_dir: PathBuf,
    settings_file: PathBuf,
    ev: mpsc::UnboundedSender<Event>,
    agent_tx: mpsc::UnboundedSender<AgentMsg>,
    settings: Settings,
    agents: Vec<AgentStatus>,
    summaries: Vec<SessionSummary>,
    open: HashMap<SessionId, Session>,
    next_run: u64,
    next_request: u64,
    admin: admin::FakeAdmin,
    /// Mode / model / effort choices per session (R13).
    prefs: HashMap<SessionId, queue::FakePrefs>,
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

async fn write_json<T: Serialize>(path: &Path, value: &T) -> std::io::Result<()> {
    let bytes = serde_json::to_vec_pretty(value).map_err(std::io::Error::other)?;
    if let Some(dir) = path.parent() {
        tokio::fs::create_dir_all(dir).await?;
    }
    let tmp = path.with_extension("json.tmp");
    tokio::fs::write(&tmp, bytes).await?;
    tokio::fs::rename(&tmp, path).await
}

fn demo_subagents(parent: &SessionId) -> Vec<harness_core_api::SubagentView> {
    use harness_core_api::{AgentId, SessionId as ApiId, SubagentStatus, SubagentView};
    let now = i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis(),
    )
    .unwrap_or(0);
    let row =
        |n: u32, agent: &str, title: &str, status, activity: &str, preview: &str, ago: i64| {
            SubagentView {
                session_id: ApiId::new(format!("sess_demo{n}")),
                parent_id: ApiId::new(parent.clone()),
                agent: AgentId::new(agent.to_owned()),
                title: title.to_owned(),
                status,
                activity: activity.to_owned(),
                started_at: now - ago * 1000,
                finished_at: (status == SubagentStatus::Done).then_some(now - 20_000),
                result_preview: preview.to_owned(),
                scope: (n == 1).then(|| harness_core_api::ScopeView {
                    paths: vec!["src/core".to_owned()],
                    tools: vec!["edit".to_owned()],
                    network: false,
                    expires_at: Some(now + 600_000),
                }),
            }
        };
    vec![
        row(
            1,
            "deepseek",
            "Rewrite hub path claiming",
            SubagentStatus::Running,
            "edit src/core/hub.rs · +38 −12",
            "",
            221,
        ),
        row(
            2,
            "deepseek",
            "Backfill claim_paths tests",
            SubagentStatus::Running,
            "cargo check -p harness-core",
            "",
            140,
        ),
        row(
            3,
            "codex",
            "Port shell runner to tokio",
            SubagentStatus::NeedsApproval,
            "Run cargo test?",
            "",
            95,
        ),
        row(
            4,
            "agy",
            "Audit agent picker revert",
            SubagentStatus::Done,
            "",
            "No regressions found in agent_picker.slint.",
            400,
        ),
    ]
}

fn emit(ev: &mpsc::UnboundedSender<Event>, event: Event) {
    // UI 已退出时发送失败是正常的关停路径，忽略即可。
    let _ = ev.send(event);
}

/// The four ACP answers the fake agent offers (`ChoosePermission` maps `deny*` to a rejection).
fn fake_choices() -> Vec<super::PermissionChoice> {
    use harness_core_api::PermissionOptionKind as Kind;
    [
        ("allow", "Allow", Kind::AllowOnce),
        ("allow-always", "Always allow", Kind::AllowAlways),
        ("deny", "Deny", Kind::RejectOnce),
        ("deny-always", "Always deny", Kind::RejectAlways),
    ]
    .into_iter()
    .map(|(id, name, kind)| super::PermissionChoice {
        id: id.into(),
        name: name.into(),
        kind,
    })
    .collect()
}

fn agent_name(id: &str) -> String {
    KNOWN_AGENTS
        .iter()
        .find(|(known, _)| *known == id)
        .map_or_else(|| id.to_string(), |(_, name)| (*name).to_string())
}

/// Runs the fake core until `Command::Shutdown` or the command channel closes.
pub async fn run(
    home: PathBuf,
    store: Arc<SettingsStore>,
    mut commands: mpsc::UnboundedReceiver<Command>,
    ev: mpsc::UnboundedSender<Event>,
) {
    let (agent_tx, mut agent_rx) = mpsc::unbounded_channel();
    let mut store_rx = store.subscribe();
    let mut core = FakeCore::load(&home, &store, ev, agent_tx).await;
    core.emit_startup();
    loop {
        tokio::select! {
            command = commands.recv() => match command {
                None | Some(Command::Shutdown) => break,
                Some(command) => core.handle(command).await,
            },
            Some(message) = agent_rx.recv() => core.on_agent(message).await,
            changed = store_rx.changed() => {
                if changed.is_err() {
                    continue;
                }
                let snapshot = store_rx.borrow_and_update().clone();
                core.on_store(&snapshot);
            }
        }
    }
    core.shutdown().await;
}

impl FakeCore {
    async fn load(
        home: &Path,
        store: &SettingsStore,
        ev: mpsc::UnboundedSender<Event>,
        agent_tx: mpsc::UnboundedSender<AgentMsg>,
    ) -> Self {
        let sessions_dir = home.join("sessions");
        let settings_file = home.join(FAKE_STATE_FILE);
        let state: FakeState = match tokio::fs::read(&settings_file).await {
            Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_default(),
            Err(_) => FakeState::default(),
        };
        let snapshot = store.snapshot();
        let defaults = Settings::default();
        let settings = Settings {
            theme: prefs::theme_of(&snapshot),
            language: prefs::language_tag(snapshot.general.language),
            fallback_order: state.fallback_order.unwrap_or(defaults.fallback_order),
        };
        let available_list =
            std::env::var("HARNESS_FAKE_AGENTS").unwrap_or_else(|_| "claude-code,codex".into());
        let available: Vec<&str> = available_list.split(',').map(str::trim).collect();
        let agents = KNOWN_AGENTS
            .iter()
            .map(|(id, name)| AgentStatus {
                id: (*id).to_string(),
                name: (*name).to_string(),
                state: if available.contains(id) {
                    AgentState::Available
                } else {
                    AgentState::Unavailable
                },
            })
            .collect();
        let mut summaries = Vec::new();
        if let Ok(mut dir) = tokio::fs::read_dir(&sessions_dir).await {
            while let Ok(Some(entry)) = dir.next_entry().await {
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) != Some("json") {
                    continue;
                }
                if let Ok(bytes) = tokio::fs::read(&path).await {
                    if let Ok(file) = serde_json::from_slice::<SessionFile>(&bytes) {
                        summaries.push(file.summary);
                    }
                }
            }
        }
        summaries.sort_by_key(|s| std::cmp::Reverse(s.updated_ms));
        Self {
            sessions_dir,
            settings_file,
            ev,
            agent_tx,
            settings,
            agents,
            summaries,
            open: HashMap::new(),
            next_run: 1,
            next_request: 1,
            admin: admin::FakeAdmin::default(),
            prefs: HashMap::new(),
        }
    }

    fn emit_startup(&self) {
        emit(&self.ev, Event::Settings(self.settings.clone()));
        emit(&self.ev, Event::Agents(self.agents.clone()));
        emit(&self.ev, Event::Sessions(self.summaries.clone()));
    }

    fn is_available(&self, agent: &str) -> bool {
        self.agents
            .iter()
            .any(|a| a.id == agent && a.state == AgentState::Available)
    }

    /// Next available agent in the fallback order after `from` (wrapping), never `from` itself.
    fn next_available_after(&self, from: &str) -> Option<AgentId> {
        let order = &self.settings.fallback_order;
        let start = order.iter().position(|a| a == from).map_or(0, |p| p + 1);
        (0..order.len())
            .map(|i| &order[(start + i) % order.len()])
            .find(|a| a.as_str() != from && self.is_available(a))
            .cloned()
    }

    fn first_available(&self) -> Option<AgentId> {
        self.settings
            .fallback_order
            .iter()
            .find(|a| self.is_available(a))
            .cloned()
    }

    fn on_store(&mut self, snapshot: &harness_settings::Settings) {
        let theme = prefs::theme_of(snapshot);
        let language = prefs::language_tag(snapshot.general.language);
        if theme != self.settings.theme || language != self.settings.language {
            self.settings.theme = theme;
            self.settings.language = language;
            emit(&self.ev, Event::Settings(self.settings.clone()));
        }
    }

    async fn persist_settings(&self) {
        let state = FakeState {
            fallback_order: Some(self.settings.fallback_order.clone()),
        };
        if let Err(error) = write_json(&self.settings_file, &state).await {
            emit(
                &self.ev,
                Event::Error(format!("settings not saved: {error}")),
            );
        }
    }

    async fn persist_session(&self, session: &SessionId) {
        let Some(s) = self.open.get(session) else {
            return;
        };
        let file = SessionFile {
            summary: s.summary.clone(),
            sticky: s.sticky.clone(),
            items: s.items.iter().map(|(_, item)| item.clone()).collect(),
        };
        if let Err(error) =
            write_json(&self.sessions_dir.join(format!("{session}.json")), &file).await
        {
            emit(
                &self.ev,
                Event::Error(format!("session not saved: {error}")),
            );
        }
    }

    fn sync_summary(&mut self, session: &SessionId) {
        let Some(s) = self.open.get(session) else {
            return;
        };
        let summary = s.summary.clone();
        self.summaries.retain(|existing| existing.id != summary.id);
        self.summaries.insert(0, summary);
        emit(&self.ev, Event::Sessions(self.summaries.clone()));
    }

    async fn ensure_open(&mut self, session: &SessionId) -> bool {
        if self.open.contains_key(session) {
            return true;
        }
        let path = self.sessions_dir.join(format!("{session}.json"));
        let Ok(bytes) = tokio::fs::read(&path).await else {
            return false;
        };
        let Ok(file) = serde_json::from_slice::<SessionFile>(&bytes) else {
            return false;
        };
        let mut loaded = Session {
            summary: file.summary,
            items: Vec::new(),
            next_item: 1,
            sticky: file.sticky,
            queue: VecDeque::new(),
            next_queue: 1,
            run: None,
            paused: false,
        };
        for mut item in file.items {
            // 上次退出时仍在流式输出的回合不会再继续，按中断处理，避免恢复后永远显示“流式中”。
            if let Item::Assistant { status, .. } = &mut item {
                if *status == TurnStatus::Streaming {
                    *status = TurnStatus::Interrupted;
                }
            }
            loaded.push(item);
        }
        self.open.insert(session.clone(), loaded);
        true
    }

    fn emit_opened(&self, session: &SessionId) {
        let Some(s) = self.open.get(session) else {
            return;
        };
        emit(
            &self.ev,
            Event::SessionOpened {
                session: s.summary.clone(),
                items: s.items.clone(),
                sticky: s.sticky.clone(),
                queued: s.queue.iter().cloned().collect(),
                paused: s.paused,
            },
        );
        emit(
            &self.ev,
            Event::Running {
                session: session.clone(),
                running: s.run.is_some(),
            },
        );
        // QC aid for the Orchestra live view: HARNESS_FAKE_SUBAGENTS=1 gives every session demo subagents.
        if std::env::var_os("HARNESS_FAKE_SUBAGENTS").is_some() {
            emit(
                &self.ev,
                Event::Subagents {
                    session: session.clone(),
                    items: demo_subagents(session),
                },
            );
            let other = "fake-background-session".to_owned();
            emit(
                &self.ev,
                Event::Fleet(vec![
                    super::FleetSession {
                        id: session.clone(),
                        title: s.summary.title.clone(),
                        prompt: "Split the hub into smaller modules and keep the tests green"
                            .into(),
                        agent: s.sticky.clone(),
                        running: s.run.is_some(),
                        open: true,
                        subagents: demo_subagents(session),
                    },
                    super::FleetSession {
                        id: other.clone(),
                        title: "Refactor billing module".into(),
                        prompt: "Move invoice rounding into one helper".into(),
                        agent: "codex".into(),
                        running: true,
                        open: false,
                        subagents: demo_subagents(&other).into_iter().take(1).collect(),
                    },
                ]),
            );
        }
    }

    #[allow(clippy::too_many_lines)]
    async fn handle(&mut self, command: Command) {
        match command {
            Command::CreateSession { agent, .. } | Command::CreateWorktreeSession { agent, .. } => {
                let id = uuid::Uuid::new_v4().to_string();
                let sticky = if agent.is_empty() || !self.is_available(&agent) {
                    self.first_available()
                        .unwrap_or_else(|| "claude-code".into())
                } else {
                    agent
                };
                let summary = SessionSummary {
                    id: id.clone(),
                    title: String::new(),
                    updated_ms: now_ms(),
                    ..SessionSummary::default()
                };
                self.open.insert(
                    id.clone(),
                    Session {
                        summary,
                        items: Vec::new(),
                        next_item: 1,
                        sticky,
                        queue: VecDeque::new(),
                        next_queue: 1,
                        run: None,
                        paused: false,
                    },
                );
                self.persist_session(&id).await;
                self.sync_summary(&id);
                self.emit_opened(&id);
            }
            Command::OpenSession { session } => {
                if self.ensure_open(&session).await {
                    self.emit_opened(&session);
                    self.emit_session_options(&session, false);
                } else {
                    emit(
                        &self.ev,
                        Event::Error(format!("session {session} not found")),
                    );
                }
            }
            // fake 不处理附件：带上下文的发送按普通发送处理。
            Command::Send {
                session,
                agent,
                text,
            }
            | Command::SendContext {
                session,
                agent,
                text,
                ..
            } => self.send(session, agent, text).await,
            Command::Cancel { session } => self.cancel(&session),
            Command::CancelQueued { session, id } => {
                if let Some(s) = self.open.get_mut(&session) {
                    s.queue.retain(|q| q.id != id);
                    let items = s.queue.iter().cloned().collect();
                    emit(&self.ev, Event::Queue { session, items });
                }
            }
            // fake 只提供 allow / deny 两类选项：以 deny/reject 开头的选项按拒绝处理。
            Command::ChoosePermission {
                session,
                request,
                option_id,
            } => {
                let allow = !(option_id.starts_with("deny") || option_id.starts_with("reject"));
                self.respond_permission(&session, request, allow);
            }
            Command::Chat(call) => self.chat_call(call).await,
            Command::EditQueued { session, id, text } => {
                self.queue_op(session, queue::QueueOp::Edit(id, text));
            }
            Command::ReorderQueued { session, ids } => {
                self.queue_op(session, queue::QueueOp::Reorder(ids));
            }
            Command::RunQueuedNow { session, id } => {
                self.queue_op(session, queue::QueueOp::RunNow(id));
            }
            // fake 不处理附件：带上下文的插话按普通插话处理。
            Command::Steer {
                session,
                agent,
                text,
                ..
            } => self.steer(session, agent, text).await,
            Command::SetMode { session, mode } => self.set_pref(&session, |p| p.mode = Some(mode)),
            Command::SetMachine { session, on } => self.set_pref(&session, |p| p.machine = on),
            Command::SetModel { session, model } => {
                self.set_pref(&session, |p| p.model = Some(model));
            }
            Command::SetConfigOption { session, id, value } => {
                if id == "effort" {
                    self.set_pref(&session, |p| p.effort = Some(value));
                } else {
                    emit(
                        &self.ev,
                        Event::Error(format!("unknown config option {id}")),
                    );
                }
            }
            Command::SessionOptions { session } => self.emit_session_options(&session, false),
            Command::SetStickyAgent { session, agent } => {
                if !self.ensure_open(&session).await {
                    return;
                }
                if let Some(s) = self.open.get_mut(&session) {
                    s.sticky.clone_from(&agent);
                }
                emit(
                    &self.ev,
                    Event::Sticky {
                        session: session.clone(),
                        agent,
                    },
                );
                self.persist_session(&session).await;
            }

            Command::SetFallbackOrder(order) => {
                self.settings.fallback_order = order;
                self.persist_settings().await;
                emit(&self.ev, Event::Settings(self.settings.clone()));
            }
            Command::AgentOptions(agent) => {
                emit(&self.ev, super::prefs::fake_agent_options(agent));
            }
            Command::AgentsOverview => self.emit_overview(),
            Command::TestAgent(agent) => self.test_agent(&agent),
            // fake core 没有账号存储（§9.6 由 hub 负责）：列表恒为空，增删报告不支持。
            Command::Accounts(call) => emit(
                &self.ev,
                Event::Accounts(match call {
                    super::AccountsCall::List => Ok(Vec::new()),
                    _ => Err("accounts need the agent hub".into()),
                }),
            ),
            // provider 同样由 hub 存储（§9.7）。
            Command::AgyAccounts(op) => emit(
                &self.ev,
                Event::AgyAccounts {
                    sign_in: None,
                    result: match op {
                        harness_core_api::AgyAccountOp::List => {
                            Ok(harness_core_api::AgyAccounts::default())
                        }
                        _ => Err("Antigravity accounts need the agent hub".into()),
                    },
                },
            ),
            Command::Providers(call) => emit(
                &self.ev,
                Event::Providers(match call {
                    super::ProvidersCall::List => Ok(Vec::new()),
                    _ => Err("providers need the agent hub".into()),
                }),
            ),
            // fake core 没有需要安装的适配器（§9.10）：空行 + 有 npm，首次启动对话框不会弹出。
            Command::Setup(super::SetupCall::Status) => emit(
                &self.ev,
                Event::SetupStatus(Ok(harness_core_api::SetupStatus {
                    npm_path: Some("fake://npm".into()),
                    npm_steps: String::new(),
                    agents: Vec::new(),
                    gh: harness_core_api::GhSetup {
                        path: Some("fake://gh".into()),
                        bundled: false,
                    },
                })),
            ),
            Command::Setup(super::SetupCall::Install(agent)) => emit(
                &self.ev,
                Event::AdapterInstall {
                    agent,
                    status: harness_core_api::AdapterInstallStatus::Failed,
                    line: None,
                    message: Some("adapters need the agent hub".into()),
                },
            ),
            Command::Settings(call) => self.settings_call(call).await,
            // fake core 不会启动失败，也不产生类型化失败：RetryInit 没有可重试的内容。
            Command::Shutdown | Command::RetryInit { .. } | Command::StopBackgroundTask { .. } => {}
        }
    }

    async fn send(&mut self, session: SessionId, agent: AgentId, text: String) {
        // Sending to an archived session unarchives it (same rule as the hub, §9.1.5a); sync_summary re-lists it.
        self.admin.archived.retain(|(s, _)| s.id != session);
        if !self.ensure_open(&session).await {
            emit(
                &self.ev,
                Event::Error(format!("session {session} not found")),
            );
            return;
        }
        let Some(s) = self.open.get_mut(&session) else {
            return;
        };
        if s.sticky != agent {
            s.sticky.clone_from(&agent);
            emit(
                &self.ev,
                Event::Sticky {
                    session: session.clone(),
                    agent,
                },
            );
        }
        let Some(s) = self.open.get_mut(&session) else {
            return;
        };
        if s.run.is_some() || s.paused {
            let id = s.queue_id();
            s.queue.push_back(QueuedMessage { id, text });
            let items = s.queue.iter().cloned().collect();
            emit(&self.ev, Event::Queue { session, items });
            return;
        }
        self.start_turn(&session, text, false);
    }

    /// Starts a turn with the session's sticky agent (falling back when it is not available).
    fn start_turn(&mut self, session: &SessionId, prompt: String, continuation: bool) {
        let Some(sticky) = self.open.get(session).map(|s| s.sticky.clone()) else {
            return;
        };
        let agent = if self.is_available(&sticky) {
            Some(sticky.clone())
        } else {
            self.next_available_after(&sticky)
        };
        let Some(agent) = agent else {
            self.pause(session, prompt);
            return;
        };
        if agent != sticky {
            if let Some(s) = self.open.get_mut(session) {
                s.sticky.clone_from(&agent);
            }
            emit(
                &self.ev,
                Event::Sticky {
                    session: session.clone(),
                    agent: agent.clone(),
                },
            );
        }
        let run_id = self.next_run;
        self.next_run += 1;
        let (cancel_tx, cancel_rx) = watch::channel(false);
        let Some(s) = self.open.get_mut(session) else {
            return;
        };
        if !continuation {
            if s.summary.title.is_empty() {
                s.summary.title = prompt.chars().take(60).collect();
            }
            let id = s.push(Item::User {
                text: prompt.clone(),
                background: false,
                images: Vec::new(),
            });
            let item = Item::User {
                text: prompt.clone(),
                background: false,
                images: Vec::new(),
            };
            emit(
                &self.ev,
                Event::ItemAdded {
                    session: session.clone(),
                    id,
                    item,
                },
            );
        }
        let Some(s) = self.open.get_mut(session) else {
            return;
        };
        if !s.summary.agents_used.contains(&agent) {
            s.summary.agents_used.push(agent.clone());
        }
        s.summary.updated_ms = now_ms();
        s.run = Some(Run {
            id: run_id,
            agent: agent.clone(),
            prompt: prompt.clone(),
            assistant: None,
            tool: None,
            cancel: cancel_tx,
            permission: None,
        });
        emit(
            &self.ev,
            Event::Running {
                session: session.clone(),
                running: true,
            },
        );
        self.sync_summary(session);
        let job = Job {
            run: run_id,
            session: session.clone(),
            name: agent_name(&agent),
            agent,
            prompt,
            continuation,
        };
        tokio::spawn(fake_agent(job, self.agent_tx.clone(), cancel_rx));
    }

    fn pause(&mut self, session: &SessionId, prompt: String) {
        let reset = self.agents.iter().find_map(|a| {
            if let AgentState::Exhausted { reset } = &a.state {
                reset.clone()
            } else {
                None
            }
        });
        let Some(s) = self.open.get_mut(session) else {
            return;
        };
        s.paused = true;
        // 未完成的提示词放回队首，等有可用 agent 时继续，而不是丢弃。
        let id = s.queue_id();
        s.queue.push_front(QueuedMessage { id, text: prompt });
        let items: Vec<QueuedMessage> = s.queue.iter().cloned().collect();
        let notice = Item::Notice {
            notice: Notice::Paused { reset },
        };
        let id = s.push(notice.clone());
        emit(
            &self.ev,
            Event::ItemAdded {
                session: session.clone(),
                id,
                item: notice,
            },
        );
        emit(
            &self.ev,
            Event::Paused {
                session: session.clone(),
                paused: true,
            },
        );
        emit(
            &self.ev,
            Event::Queue {
                session: session.clone(),
                items,
            },
        );
        emit(
            &self.ev,
            Event::Running {
                session: session.clone(),
                running: false,
            },
        );
    }

    fn cancel(&mut self, session: &SessionId) {
        let Some(run) = self.open.get_mut(session).and_then(|s| s.run.as_mut()) else {
            return;
        };
        let _ = run.cancel.send(true);
        if let Some((request, _reply)) = run.permission.take() {
            emit(
                &self.ev,
                Event::PermissionCleared {
                    session: session.clone(),
                    request,
                },
            );
        }
    }

    fn respond_permission(&mut self, session: &SessionId, request: u64, allow: bool) {
        let Some(run) = self.open.get_mut(session).and_then(|s| s.run.as_mut()) else {
            return;
        };
        let Some((pending, _)) = &run.permission else {
            return;
        };
        if *pending != request {
            return;
        }
        if let Some((_, reply)) = run.permission.take() {
            let _ = reply.send(allow);
        }
        emit(
            &self.ev,
            Event::PermissionCleared {
                session: session.clone(),
                request,
            },
        );
    }

    #[allow(clippy::too_many_lines)]
    async fn on_agent(&mut self, message: AgentMsg) {
        let AgentMsg { run, session, out } = message;
        let current = self
            .open
            .get(&session)
            .and_then(|s| s.run.as_ref())
            .is_some_and(|r| r.id == run);
        if !current {
            // 过期 run（已取消/已切换）的迟到消息直接丢弃；权限请求的 reply 随之 drop，agent 视为拒绝。
            return;
        }
        let Some(s) = self.open.get_mut(&session) else {
            return;
        };
        let Some(r) = s.run.as_mut() else { return };
        let agent = r.agent.clone();
        match out {
            AgentOut::Delta(text) => {
                let existing = r.assistant;
                let id = if let Some(id) = existing {
                    id
                } else {
                    let item = Item::Assistant {
                        agent: agent.clone(),
                        text: String::new(),
                        status: TurnStatus::Streaming,
                    };
                    let id = s.push(item.clone());
                    if let Some(r) = s.run.as_mut() {
                        r.assistant = Some(id);
                    }
                    emit(
                        &self.ev,
                        Event::ItemAdded {
                            session: session.clone(),
                            id,
                            item,
                        },
                    );
                    id
                };
                let Some(s) = self.open.get_mut(&session) else {
                    return;
                };
                if let Some(Item::Assistant { text: buffer, .. }) = s.item_mut(id) {
                    buffer.push_str(&text);
                }
                emit(&self.ev, Event::TextDelta { session, id, text });
            }
            AgentOut::ToolStart(title) => {
                let item = Item::Tool {
                    agent,
                    title,
                    status: ToolStatus::Running,
                };
                let id = s.push(item.clone());
                if let Some(r) = s.run.as_mut() {
                    r.tool = Some(id);
                    // 工具之后的文字属于新的助手段落，保证时间顺序与显示顺序一致。
                    r.assistant = None;
                }
                emit(&self.ev, Event::ItemAdded { session, id, item });
            }
            AgentOut::ToolEnd(status) => {
                let Some(id) = r.tool else { return };
                if let Some(Item::Tool {
                    status: current, ..
                }) = s.item_mut(id)
                {
                    *current = status;
                }
                emit(
                    &self.ev,
                    Event::ToolUpdated {
                        session,
                        id,
                        status,
                    },
                );
            }
            AgentOut::Permission {
                title,
                detail,
                reply,
            } => {
                let request = self.next_request;
                self.next_request += 1;
                let Some(s) = self.open.get_mut(&session) else {
                    return;
                };
                let Some(r) = s.run.as_mut() else { return };
                r.permission = Some((request, reply));
                let tool = r.tool;
                if let Some(id) = tool {
                    if let Some(Item::Tool { status, .. }) = s.item_mut(id) {
                        *status = ToolStatus::AwaitingPermission;
                    }
                    emit(
                        &self.ev,
                        Event::ToolUpdated {
                            session: session.clone(),
                            id,
                            status: ToolStatus::AwaitingPermission,
                        },
                    );
                }
                emit(
                    &self.ev,
                    Event::PermissionRequested {
                        session: session.clone(),
                        request,
                        title,
                        detail,
                    },
                );
                emit(
                    &self.ev,
                    Event::PermissionChoices {
                        session,
                        request,
                        options: fake_choices(),
                    },
                );
            }
            AgentOut::Exhausted { reset } => {
                let prompt = r.prompt.clone();
                let assistant = r.assistant;
                s.run = None;
                if let Some(id) = assistant {
                    if let Some(Item::Assistant { status, .. }) = s.item_mut(id) {
                        *status = TurnStatus::Interrupted;
                    }
                    emit(
                        &self.ev,
                        Event::TurnEnded {
                            session: session.clone(),
                            id,
                            status: TurnStatus::Interrupted,
                        },
                    );
                }
                if let Some(a) = self.agents.iter_mut().find(|a| a.id == agent) {
                    a.state = AgentState::Exhausted {
                        reset: reset.clone(),
                    };
                }
                emit(&self.ev, Event::Agents(self.agents.clone()));
                if let Some(to) = self.next_available_after(&agent) {
                    let notice = Item::Notice {
                        notice: Notice::Failover {
                            from: agent,
                            to: to.clone(),
                            reset,
                        },
                    };
                    let Some(s) = self.open.get_mut(&session) else {
                        return;
                    };
                    let id = s.push(notice.clone());
                    s.sticky.clone_from(&to);
                    emit(
                        &self.ev,
                        Event::ItemAdded {
                            session: session.clone(),
                            id,
                            item: notice,
                        },
                    );
                    emit(
                        &self.ev,
                        Event::Sticky {
                            session: session.clone(),
                            agent: to,
                        },
                    );
                    self.start_turn(&session, prompt, true);
                } else {
                    self.pause(&session, prompt);
                }
                self.persist_session(&session).await;
            }
            AgentOut::Finished { cancelled } => {
                let assistant = r.assistant;
                let tool = r.tool;
                s.run = None;
                let status = if cancelled {
                    TurnStatus::Cancelled
                } else {
                    TurnStatus::Done
                };
                if let Some(id) = assistant {
                    if let Some(Item::Assistant {
                        status: current, ..
                    }) = s.item_mut(id)
                    {
                        *current = status;
                    }
                    emit(
                        &self.ev,
                        Event::TurnEnded {
                            session: session.clone(),
                            id,
                            status,
                        },
                    );
                }
                if let Some(id) = tool {
                    let Some(s) = self.open.get_mut(&session) else {
                        return;
                    };
                    if let Some(Item::Tool {
                        status: tool_status,
                        ..
                    }) = s.item_mut(id)
                    {
                        if matches!(
                            tool_status,
                            ToolStatus::Running | ToolStatus::AwaitingPermission
                        ) {
                            *tool_status = ToolStatus::Failed;
                            emit(
                                &self.ev,
                                Event::ToolUpdated {
                                    session: session.clone(),
                                    id,
                                    status: ToolStatus::Failed,
                                },
                            );
                        }
                    }
                }
                emit(
                    &self.ev,
                    Event::Running {
                        session: session.clone(),
                        running: false,
                    },
                );
                let Some(s) = self.open.get_mut(&session) else {
                    return;
                };
                s.summary.updated_ms = now_ms();
                let next = if s.paused {
                    None
                } else {
                    s.queue.pop_front().map(|q| q.text)
                };
                let items: Vec<QueuedMessage> = s.queue.iter().cloned().collect();
                self.persist_session(&session).await;
                self.sync_summary(&session);
                if let Some(prompt) = next {
                    emit(
                        &self.ev,
                        Event::Queue {
                            session: session.clone(),
                            items,
                        },
                    );
                    self.start_turn(&session, prompt, false);
                }
            }
        }
    }

    async fn shutdown(&mut self) {
        let ids: Vec<SessionId> = self.open.keys().cloned().collect();
        for id in &ids {
            if let Some(run) = self.open.get_mut(id).and_then(|s| s.run.take()) {
                let _ = run.cancel.send(true);
            }
            self.persist_session(id).await;
        }
    }
}

/// The markdown the `ZCode` reference capture gets from the mock provider (`P7_MD_*`, phase7-parity
/// mockProvider.mjs `MARKDOWN_REPLY`), so side-by-side screenshots compare the same content.
const PLAN_REPLY: &str = "## Plan\n\nI checked `src/main.rs` and the build. Steps:\n\n\
    1. Parse the config\n2. Start the **hub**\n3. Report the result\n\n\
    ```rust\nfn main() {\n    // start the hub\n    let port = 8080;\n    println!(\"listening on {port}\");\n}\n```\n\n\
    | Agent | State | Resets |\n| --- | --- | --- |\n| Claude Code | available | - |\n| Codex | limit reached | 18:00 |\n";

fn reply_body(job: &Job) -> String {
    if job.prompt.to_lowercase().contains("plan") {
        return PLAN_REPLY.to_string();
    }
    let short: String = job.prompt.chars().take(40).collect();
    format!(
        "## {name} reply\n\nYou asked: *{short}*. This is a **fake** {name} answer with `inline code`.\n\n\
         - streamed in small chunks\n- rendered as [markdown](https://example.com/harness)\n\n\
         ```rust\nfn main() {{\n    println!(\"hello from {id}\");\n}}\n```\n\nDone ({id}).",
        name = job.name,
        id = job.agent,
    )
}

/// Streams `text` in small chunks. Returns false when cancelled.
async fn stream(
    job: &Job,
    tx: &mpsc::UnboundedSender<AgentMsg>,
    cancel: &mut watch::Receiver<bool>,
    text: &str,
    delay: Duration,
) -> bool {
    let chars: Vec<char> = text.chars().collect();
    for chunk in chars.chunks(8) {
        if *cancel.borrow() {
            return false;
        }
        let piece: String = chunk.iter().collect();
        if tx
            .send(AgentMsg {
                run: job.run,
                session: job.session.clone(),
                out: AgentOut::Delta(piece),
            })
            .is_err()
        {
            return false;
        }
        tokio::select! {
            () = tokio::time::sleep(delay) => {}
            _ = cancel.changed() => return false,
        }
    }
    true
}

async fn fake_agent(
    job: Job,
    tx: mpsc::UnboundedSender<AgentMsg>,
    mut cancel: watch::Receiver<bool>,
) {
    let send = |out: AgentOut| {
        tx.send(AgentMsg {
            run: job.run,
            session: job.session.clone(),
            out,
        })
        .is_ok()
    };
    let lower = job.prompt.to_lowercase();
    let fast = Duration::from_millis(15);
    let mut completed = true;
    if job.continuation {
        completed = stream(
            &job,
            &tx,
            &mut cancel,
            "_(continuing after an agent switch)_\n\n",
            fast,
        )
        .await;
    }
    if !completed {
    } else if lower.contains("permission") {
        send(AgentOut::ToolStart("Write notes.txt".into()));
        let (reply_tx, reply_rx) = oneshot::channel();
        send(AgentOut::Permission {
            title: "Allow Write?".into(),
            detail: format!("{} wants to write notes.txt in the workspace.", job.name),
            reply: reply_tx,
        });
        let allowed = tokio::select! {
            answer = reply_rx => answer.unwrap_or(false),
            _ = cancel.changed() => { send(AgentOut::Finished { cancelled: true }); return; }
        };
        send(AgentOut::ToolEnd(if allowed {
            ToolStatus::Done
        } else {
            ToolStatus::Denied
        }));
        let text = if allowed {
            "Permission **approved**: wrote `notes.txt`."
        } else {
            "Permission **denied**: nothing was written."
        };
        completed = stream(&job, &tx, &mut cancel, text, fast).await;
    } else if lower.contains("quota") && !job.continuation {
        let body = reply_body(&job);
        let half: String = body.chars().take(body.chars().count() / 2).collect();
        if stream(&job, &tx, &mut cancel, &half, fast).await {
            let reset = std::env::var("HARNESS_FAKE_RESET")
                .ok()
                .or_else(|| Some("10:00".into()));
            send(AgentOut::Exhausted { reset });
            return;
        }
        completed = false;
    } else if lower.contains("slow") {
        let long = "This is a deliberately slow streamed answer. ".repeat(200);
        completed = stream(&job, &tx, &mut cancel, &long, Duration::from_millis(80)).await;
    } else {
        send(AgentOut::ToolStart("Read README.md".into()));
        tokio::select! {
            () = tokio::time::sleep(Duration::from_millis(60)) => {}
            _ = cancel.changed() => { send(AgentOut::Finished { cancelled: true }); return; }
        }
        send(AgentOut::ToolEnd(ToolStatus::Done));
        completed = stream(&job, &tx, &mut cancel, &reply_body(&job), fast).await;
    }
    send(AgentOut::Finished {
        cancelled: !completed,
    });
}
