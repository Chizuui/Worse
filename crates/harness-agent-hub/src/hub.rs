//! The hub: [`HarnessCore`] implementation, shared context, actor registry.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use harness_core::WorkspaceKey;
use harness_core_api::{
    AccountInfo, AddAccount, AddProvider, AgentOverview, AgentTestResult, ArchivePage,
    ListArchived, ListProjects, MemoryDoc, MemoryFileInfo, MemoryPlan, ProjectInfo, ProviderInfo,
    SessionSelection, UpdateProvider,
};
use harness_core_api::{
    AgentId, AgentInfo, ApiError, ApiResult, BoxFuture, CreateSession, ErrorClass, EventKind,
    EventStream, HarnessCore, ListSessions, MemoryScope, MemoryWriteMode, PermissionRequestId,
    QuotaState, ResumableRun, SendAccepted, SendMessage, SessionDetail, SessionId, SessionSummary,
    TurnDiff, TurnId,
};
use harness_core_api::{AgentOptions, SessionConfigOptions, SessionModels, SessionModes};
use harness_core_api::{
    ApplyFileChanges, ChangeDirection, CommandList, CommandSearch, CommandSearchResult,
    ContextItem, EditResend, FileChangePlan, FileRestoreReport, FindHit, FindInSession,
    ForkSession, GroupColor, MentionCandidate, MentionQuery, ResolvedItem, Retry, Rewind,
    RewindResult, SessionGroup, UpdateGroup,
};
use harness_core_api::{
    ImportCandidate, ImportReport, ImportRequest, ImportScan, NewSchedule, ScheduleRun,
    ScheduledPrompt, UpdateSchedule,
};
use serde_json::{json, Value};
use tokio::sync::oneshot;

use crate::agents::AgentSpec;
use crate::bus::EventBus;
use crate::config::HubConfig;
use crate::error::HubError;
use crate::memory::{MemoryFiles, MemoryTarget};
use crate::options::{self, Advertised, PrefChange, Prefs, CACHE_PREFIX};
use crate::pool::AgentPool;
use crate::quota::{AgentErrorInfo, QuotaRegistry};
use crate::session::{ActorMsg, ActorTx, SessionActor, SessionCmd};
use crate::store::{self, SessionRow, Store};
use crate::util::{new_id, now_ms, now_secs};

const FALLBACK_KEY: &str = "fallback_order";

/// State shared by the hub and its session actors.
pub(crate) struct HubCtx {
    pub(crate) config: HubConfig,
    pub(crate) store: Store,
    pub(crate) pool: AgentPool,
    pub(crate) quota: QuotaRegistry,
    pub(crate) events: EventBus,
    pub(crate) memory: MemoryFiles,
    pub(crate) specs: HashMap<AgentId, AgentSpec>,
    fallback: Mutex<Vec<AgentId>>,
    /// Advertised modes/models/options per agent (lazy cache of store keys `agent_options:<id>`).
    advertised: Mutex<HashMap<AgentId, Advertised>>,
    /// Serializes option probes (phase-9-parity.md §9.1 `agent_options` probe).
    probe_lock: tokio::sync::Mutex<()>,
    /// Lazy chat caches (prompt capabilities, advertised slash commands).
    pub(crate) chat: crate::hub_chat::ChatState,
    /// Subagent sessions and the delegate endpoint (phase-9-parity.md §9.8).
    pub(crate) subagents: crate::delegate::Registry,
    pub(crate) machine: crate::machine::Machine,
    pub(crate) routers: crate::inbox::Routers,
    pub(crate) agy: crate::agy_ops::AgyState,
    /// Next member of each key pool (`config.key_pools`) for the round robin over API keys.
    rotation: Mutex<HashMap<usize, usize>>,
}

impl HubCtx {
    /// The agent a new session asked for `agent` gets. A member of a key pool (several API keys of one
    /// service, see `providers.rs`) is replaced by the pool's next member that is installed and not at its
    /// usage limit, so sessions take the keys in turn. Anything else, or a pool with nobody usable, stays.
    pub(crate) fn rotated(&self, agent: AgentId) -> AgentId {
        if !self.config.rotate_keys {
            return agent;
        }
        let Some((index, pool)) = crate::providers::pool_of(&self.config.key_pools, &agent) else {
            return agent;
        };
        let Ok(mut next) = self.rotation.lock() else {
            return agent;
        };
        let start = next.get(&index).copied().unwrap_or(0);
        let picked = (0..pool.len())
            .map(|offset| (start + offset) % pool.len())
            .find(|i| self.is_installed(&pool[*i]) && self.quota.is_usable(&pool[*i]));
        match picked {
            Some(i) => {
                next.insert(index, (i + 1) % pool.len());
                pool[i].clone()
            }
            None => agent,
        }
    }

    pub(crate) fn display_name(&self, agent: &AgentId) -> String {
        self.specs
            .get(agent)
            .map_or_else(|| agent.to_string(), |s| s.display_name.clone())
    }

    pub(crate) fn is_installed(&self, agent: &AgentId) -> bool {
        self.specs
            .get(agent)
            .is_some_and(|s| s.probe().is_available())
    }

    pub(crate) fn require_available(&self, agent: &AgentId) -> ApiResult<()> {
        let spec = self
            .specs
            .get(agent)
            .ok_or_else(|| ApiError::NotFound(format!("agent {agent}")))?;
        match spec.probe() {
            harness_core_api::Availability::Available { .. } => Ok(()),
            harness_core_api::Availability::NotInstalled { hint }
            | harness_core_api::Availability::AdapterMissing { hint, .. } => Err(
                ApiError::AgentUnavailable(format!("{}: {hint}", spec.display_name)),
            ),
        }
    }

    pub(crate) fn fallback_order(&self) -> Vec<AgentId> {
        self.fallback.lock().map(|o| o.clone()).unwrap_or_default()
    }

    pub(crate) fn classify(&self, agent: &AgentId, error: &AgentErrorInfo) -> ErrorClass {
        self.specs
            .get(agent)
            .map_or(ErrorClass::Fatal, |s| s.classify_error(error))
    }

    /// Single write path of quota state: registry, store, event.
    pub(crate) async fn set_quota(&self, agent: &AgentId, quota: QuotaState) {
        self.quota.set(agent, quota.clone());
        let id = agent.clone();
        let stored = quota.clone();
        if let Err(error) = self
            .store
            .run(move |c| store::save_quota(c, &id, &stored, now_secs()))
            .await
        {
            tracing::warn!(%error, "saving quota state failed");
        }
        self.events.emit(
            None,
            EventKind::AgentQuota {
                agent: agent.clone(),
                quota,
            },
        );
    }

    /// What `agent` advertised last (store cache on first use).
    pub(crate) async fn advertised(&self, agent: &AgentId) -> Advertised {
        if let Some(cached) = self
            .advertised
            .lock()
            .ok()
            .and_then(|m| m.get(agent).cloned())
        {
            return cached;
        }
        let key = format!("{CACHE_PREFIX}{agent}");
        let stored = self
            .store
            .run(move |c| store::get_setting(c, &key))
            .await
            .ok()
            .flatten()
            .and_then(|json| serde_json::from_str::<Advertised>(&json).ok())
            .unwrap_or_default();
        if let Ok(mut map) = self.advertised.lock() {
            return map.entry(agent.clone()).or_insert(stored).clone();
        }
        stored
    }

    /// Fills an empty advertisement cache by asking the agent once (spec phase-9-parity.md §9.1 `agent_options`).
    ///
    /// 修复：Settings › Agents 的默认 mode/model/effort 下拉框完全为空。原因是这些列表只来自 agent 在
    /// `session/new` 中的广播，而从未在 Harness 里聊过天的 agent（例如刚装好适配器的 Claude Code）没有缓存。
    /// 这里在缓存为空时发一次不带 prompt 的 `session/new`（cwd 为 `<HARNESS_HOME>/probe`），结果仍走唯一写入路径
    /// `record_advertised`。
    pub(crate) async fn probe_advertised(&self, agent: &AgentId) {
        let empty = |a: &Advertised| a.modes.is_none() && a.models.is_none() && a.config.is_none();
        if !empty(&self.advertised(agent).await) {
            return;
        }
        let Some(spec) = self.specs.get(agent) else {
            return;
        };
        if !matches!(
            spec.probe(),
            harness_core_api::Availability::Available { .. }
        ) {
            return;
        }
        let _probing = self.probe_lock.lock().await;
        // 等锁期间可能已有会话或另一次探测写入了缓存。
        if !empty(&self.advertised(agent).await) {
            return;
        }
        let cwd = self.config.home.join("probe");
        if let Err(error) = tokio::fs::create_dir_all(&cwd).await {
            tracing::warn!(%error, %agent, "agent options probe: no probe folder");
            return;
        }
        let connection = match self.pool.acquire(agent).await {
            Ok(connection) => connection,
            Err(error) => {
                tracing::warn!(%error, %agent, "agent options probe: agent did not start");
                return;
            }
        };
        let result = connection
            .call(
                "session/new",
                json!({"cwd": cwd.to_string_lossy(), "mcpServers": []}),
            )
            .await;
        self.pool.release(agent, connection.generation()).await;
        match result {
            Ok(result) => {
                self.record_advertised(agent, |a| a.merge_response(&result))
                    .await;
            }
            Err(error) => {
                tracing::warn!(error = %error.info().message, %agent, "agent options probe failed");
            }
        }
    }

    /// Merges a new advertisement of `agent` (single write path of the cache: memory, store). Returns true
    /// when it changed.
    pub(crate) async fn record_advertised(
        &self,
        agent: &AgentId,
        merge: impl FnOnce(&mut Advertised) -> bool,
    ) -> bool {
        let mut current = self.advertised(agent).await;
        if !merge(&mut current) {
            return false;
        }
        if let Ok(mut map) = self.advertised.lock() {
            map.insert(agent.clone(), current.clone());
        }
        let key = format!("{CACHE_PREFIX}{agent}");
        match serde_json::to_string(&current) {
            Ok(json) => {
                if let Err(error) = self
                    .store
                    .run(move |c| store::set_setting(c, &key, &json))
                    .await
                {
                    tracing::warn!(%error, "saving agent options failed");
                }
            }
            Err(error) => tracing::warn!(%error, "encoding agent options failed"),
        }
        true
    }

    /// `mcpServers` of `session/new` / `session/load`: `harness-memory` first, then the enabled entries of the
    /// Harness overlay `<home>/mcp.json` for `agent` (read fresh each time; spec phase-9-parity.md §9.1.4).
    pub(crate) async fn mcp_servers(
        &self,
        agent: &AgentId,
        cwd: &str,
        caps: (bool, bool),
        session: &SessionId,
    ) -> Value {
        let mut servers = vec![self.memory_mcp_server(cwd, session, &self.display_name(agent))];
        if let Some(delegate) =
            self.delegate_mcp_server(session, crate::agy_ops::is_agy(self, agent))
        {
            servers.push(delegate);
        }
        if matches!(
            self.config.helpers,
            crate::helpers::HelperLaunch::SelfExe { .. }
        ) {
            let helper = crate::helpers::Helper::Computer;
            servers.push(json!({
                "name": "harness-computer",
                "command": self.config.helpers.program(helper).to_string_lossy(),
                "args": self.config.helpers.args(helper),
                "env": [
                    {"name": "HARNESS_HOME", "value": self.config.home.to_string_lossy()},
                    {"name": "HARNESS_MACHINE_SESSION", "value": session.as_str()}
                ]
            }));
        }
        let caps = harness_agent_config::overlay::AcpMcpCapabilities {
            http: caps.0,
            sse: caps.1,
        };
        match harness_agent_config::overlay::acp_servers(&self.config.home, agent.as_str(), caps)
            .await
        {
            Ok(extra) => servers.extend(extra),
            // 叠加配置损坏时不能让轮次失败：只记录告警并仅传 harness-memory（spec §9.1.4）。
            Err(error) => tracing::warn!(%error, "ignoring unreadable MCP overlay"),
        }
        // The project's Graphify graph, unless the user already configured a server of that name.
        let named = |server: &Value| {
            server.get("name").and_then(Value::as_str) == Some(crate::graphify::MCP_NAME)
        };
        if !servers.iter().any(named) && crate::recall::Store::new(&self.config.home).config().graph
        {
            if let Some(graph) = crate::graphify::mcp_server(cwd) {
                servers.push(graph);
            }
        }
        Value::Array(servers)
    }

    /// ACP `McpServer` entry of `harness-delegate` (§9.8). Subagents get the `child` role, or `nested` (delegate tools
    /// plus `notify_parent`) while settings allow subagents to start subagents.
    fn delegate_mcp_server(&self, session: &SessionId, compact: bool) -> Option<Value> {
        let endpoint = self.subagents.endpoint.get()?;
        // §9.8.1：子会话也拿到该服务器，但只是 child 角色（notify_parent / claim_paths），仍不能再委派。
        let role = if self.routers.is_router(session) {
            "router"
        } else if self.subagents.is_child(session) && self.config.delegation.nested() {
            "nested"
        } else if self.subagents.is_child(session) {
            "child"
        } else {
            "parent"
        };
        let helper = crate::helpers::Helper::Delegate;
        Some(json!({
            "name": "harness-delegate",
            "command": self.config.helpers.program(helper).to_string_lossy(),
            "args": self.config.helpers.args(helper),
            "env": [
                {"name": "HARNESS_DELEGATE_ADDR", "value": endpoint.addr},
                {"name": "HARNESS_DELEGATE_TOKEN", "value": endpoint.token},
                {"name": "HARNESS_DELEGATE_SESSION", "value": session.as_str()},
                {"name": "HARNESS_DELEGATE_ROLE", "value": role},
                {"name": "HARNESS_DELEGATE_COMPACT", "value": if compact { "1" } else { "0" }}
            ]
        }))
    }

    /// ACP `McpServer` (stdio) entry for the Harness memory server. `session` is passed as `--session` so the
    /// server can read and write the working notes of this session, and `agent` (a display name) as `--agent`
    /// so the memories it saves say who wrote them.
    pub(crate) fn memory_mcp_server(
        &self,
        workspace_path: &str,
        session: &SessionId,
        agent: &str,
    ) -> Value {
        let helper = crate::helpers::Helper::MemoryMcp;
        let mut args = self.config.helpers.args(helper);
        args.extend([
            "--project".to_owned(),
            workspace_path.to_owned(),
            "--session".to_owned(),
            session.as_str().to_owned(),
            "--agent".to_owned(),
            agent.to_owned(),
        ]);
        json!({
            "name": "harness-memory",
            "command": self.config.helpers.program(helper).to_string_lossy(),
            "args": args,
            "env": [{"name": "HARNESS_HOME", "value": self.config.home.to_string_lossy()}]
        })
    }
}

/// The agent hub. Create with [`Hub::start`]; use through [`HarnessCore`].
pub struct Hub {
    pub(crate) ctx: Arc<HubCtx>,
    pub(crate) actors: tokio::sync::Mutex<HashMap<SessionId, ActorTx>>,
    shut_down: AtomicBool,
    /// Weak self for the lazy scheduler task (never keeps the hub alive).
    pub(crate) me: std::sync::Weak<Hub>,
    pub(crate) scheduler: crate::schedule::Scheduler,
    pub(crate) inbox: crate::inbox::Inbox,
    pub(crate) board_sync: crate::board_sync::BoardSyncSlot,
    /// Accept task of the delegate server (aborted on shutdown).
    delegate_task: Mutex<Option<tokio::task::AbortHandle>>,
}

impl Hub {
    /// Opens the store under `config.home` and starts the hub (no agent process is started).
    ///
    /// # Errors
    ///
    /// Store/IO failures.
    pub async fn start(mut config: HubConfig) -> Result<Arc<Self>, HubError> {
        if config.load_agents_file {
            config.apply_agents_file().await?;
        }
        // §9.6：额外的 Claude 账号作为独立 agent（各自 CLAUDE_CONFIG_DIR），放在 Claude Code 之后参与 failover。
        crate::accounts::apply(&mut config).await;
        crate::agy_agents::apply(&mut config).await;
        // 自定义 API provider（§9.7）同样在启动时变成 agent，排在默认顺序末尾。
        crate::providers::apply(&mut config).await;
        let db_path = config.db_path();
        let store = tokio::task::spawn_blocking(move || Store::open(&db_path))
            .await
            .map_err(|e| HubError::Config(format!("store open task failed: {e}")))??;
        let (quota, fallback, subagents) = store
            .run(|c| {
                Ok((
                    store::load_quota(c)?,
                    store::get_setting(c, FALLBACK_KEY)?,
                    crate::delegate::Registry::load(c)?,
                ))
            })
            .await?;
        let specs: HashMap<AgentId, AgentSpec> = config
            .agents
            .iter()
            .map(|s| (s.id.clone(), s.clone()))
            .collect();
        let fallback = fallback
            .and_then(|json| serde_json::from_str::<Vec<AgentId>>(&json).ok())
            .map(|order| {
                // 修复：已保存的 fallback 顺序会丢掉之后新增的 agent（如新账号），使其永远不参与 failover。
                // 依据 §9.6：缺失的 agent 按默认顺序插到其前一个默认邻居之后。
                let mut order = order
                    .into_iter()
                    .filter(|a| specs.contains_key(a))
                    .collect::<Vec<_>>();
                for (i, agent) in config.default_fallback_order.iter().enumerate() {
                    if order.contains(agent) {
                        continue;
                    }
                    let position = config.default_fallback_order[..i]
                        .iter()
                        .rev()
                        .find_map(|prev| order.iter().position(|a| a == prev))
                        .map_or(0, |p| p + 1);
                    order.insert(position, agent.clone());
                }
                order
            })
            .filter(|order| !order.is_empty())
            .unwrap_or_else(|| config.default_fallback_order.clone());
        let events = EventBus::new(config.event_capacity);
        let pool = AgentPool::new(&config.agents, config.agent_idle_timeout, events.clone());
        let memory = MemoryFiles::new(config.home.clone());
        let machine = crate::machine::Machine::load(&config.home).await;
        crate::gh::set_home(&config.home);
        let ctx = Arc::new(HubCtx {
            config,
            store,
            pool,
            quota: QuotaRegistry::with_state(quota),
            events,
            memory,
            specs,
            fallback: Mutex::new(fallback),
            advertised: Mutex::new(HashMap::new()),
            probe_lock: tokio::sync::Mutex::new(()),
            chat: crate::hub_chat::ChatState::default(),
            subagents,
            machine,
            routers: crate::inbox::Routers::default(),
            agy: crate::agy_ops::AgyState::default(),
            rotation: Mutex::new(HashMap::new()),
        });
        let hub = Arc::new_cyclic(|me| Self {
            ctx,
            actors: tokio::sync::Mutex::new(HashMap::new()),
            shut_down: AtomicBool::new(false),
            me: me.clone(),
            scheduler: crate::schedule::Scheduler::default(),
            inbox: crate::inbox::Inbox::default(),
            board_sync: crate::board_sync::BoardSyncSlot::default(),
            delegate_task: Mutex::new(None),
        });
        hub.start_scheduler().await;
        hub.start_delegate().await;
        hub.start_routing().await;
        tokio::spawn(crate::inbox::track(hub.me.clone()));
        hub.ensure_inbox().await;
        tokio::spawn(crate::board_sync::track(hub.me.clone()));
        hub.ensure_board_sync().await;
        Ok(hub)
    }

    /// Starts the subagent tracker and the loopback delegate server (§9.8). `HARNESS_DELEGATE=0` turns the
    /// delegate tool off; a bind failure only logs (agents then simply get no `harness-delegate` server).
    async fn start_delegate(&self) {
        tokio::spawn(crate::delegate::track(Arc::downgrade(&self.ctx)));
        tokio::spawn(crate::board::track(Arc::downgrade(&self.ctx)));
        tokio::spawn(crate::resume::track(Arc::downgrade(&self.ctx)));
        if std::env::var("HARNESS_DELEGATE").is_ok_and(|v| v == "0") {
            return;
        }
        match crate::delegate_server::start(self.me.clone()).await {
            Ok((endpoint, task)) => {
                tokio::spawn(crate::delegate_server::push(self.me.clone()));
                let _ = self.ctx.subagents.endpoint.set(endpoint);
                if let Ok(mut slot) = self.delegate_task.lock() {
                    *slot = Some(task.abort_handle());
                }
            }
            Err(error) => tracing::warn!(%error, "delegate server not started"),
        }
    }

    pub(crate) fn check_open(&self) -> ApiResult<()> {
        if self.shut_down.load(Ordering::SeqCst) {
            Err(ApiError::ShutDown)
        } else {
            Ok(())
        }
    }

    async fn actor(&self, id: &SessionId) -> ApiResult<ActorTx> {
        self.check_open()?;
        let mut actors = self.actors.lock().await;
        if let Some(tx) = actors.get(id) {
            if !tx.is_closed() {
                return Ok(tx.clone());
            }
        }
        let lookup = id.clone();
        let row = self
            .ctx
            .store
            .run(move |c| store::load_session(c, &lookup))
            .await?
            .ok_or_else(|| ApiError::NotFound(format!("session {id}")))?;
        let tx = SessionActor::spawn(Arc::clone(&self.ctx), row).await?;
        actors.insert(id.clone(), tx.clone());
        Ok(tx)
    }

    pub(crate) async fn ask<T>(
        &self,
        id: &SessionId,
        make: impl FnOnce(oneshot::Sender<ApiResult<T>>) -> SessionCmd,
    ) -> ApiResult<T> {
        let tx = self.actor(id).await?;
        let (reply, rx) = oneshot::channel();
        if let Err(unsent) = tx.send(ActorMsg::Cmd(make(reply))) {
            self.actor(id)
                .await?
                .send(unsent.0)
                .map_err(|_| ApiError::ShutDown)?;
        }
        rx.await.map_err(|_| ApiError::ShutDown)?
    }

    pub(crate) fn memory_target(scope: &MemoryScope) -> MemoryTarget {
        match scope {
            MemoryScope::Global => MemoryTarget::Global,
            MemoryScope::Project { workspace_path } => {
                MemoryTarget::Project(std::path::PathBuf::from(workspace_path))
            }
        }
    }

    async fn do_turn_diff(&self, session_id: SessionId, turn_id: TurnId) -> ApiResult<TurnDiff> {
        self.check_open()?;
        let (lookup_session, lookup_turn) = (session_id.clone(), turn_id.clone());
        let (session, owner, snapshot) = self
            .ctx
            .store
            .run(move |c| {
                Ok((
                    store::load_session(c, &lookup_session)?,
                    store::turn_session(c, &lookup_turn)?,
                    store::load_snapshot(c, &lookup_turn)?,
                ))
            })
            .await?;
        let session = session.ok_or_else(|| ApiError::NotFound(format!("session {session_id}")))?;
        if owner.as_ref() != Some(&session_id) {
            return Err(ApiError::NotFound(format!(
                "turn {turn_id} in session {session_id}"
            )));
        }
        let unavailable = |reason: String| TurnDiff {
            turn_id: turn_id.clone(),
            available: false,
            reason: Some(reason),
            files: Vec::new(),
            truncated: false,
            undone: false,
        };
        let disabled = if self.ctx.config.turn_snapshots {
            "no snapshot was taken for this turn"
        } else {
            "turn snapshots are disabled (HARNESS_TURN_SNAPSHOTS=0)"
        };
        let Some(snapshot) = snapshot else {
            return Ok(unavailable(disabled.into()));
        };
        let Some(before) = snapshot.before_tree else {
            return Ok(unavailable(
                snapshot.error.unwrap_or_else(|| disabled.into()),
            ));
        };
        let workspace = std::path::PathBuf::from(&session.workspace_path);
        let after = match snapshot.after_tree {
            Some(after) => after,
            // Running (or ended without an after snapshot): compare against the worktree now.
            None => crate::snapshot::take(&self.ctx.config.home.join("tmp"), &workspace)
                .await
                .map_err(ApiError::Internal)?
                .ok_or_else(|| {
                    ApiError::Internal("workspace is no longer a git repository".into())
                })?,
        };
        for tree in [&before, &after] {
            if !crate::snapshot::exists(&workspace, tree).await {
                return Ok(unavailable("snapshot no longer in the repository".into()));
            }
        }
        let undone_turn = turn_id.clone();
        let undone = self
            .ctx
            .store
            .run(move |c| crate::store_chat::is_undone(c, &undone_turn))
            .await?;
        let (files, truncated) = crate::snapshot::diff(&workspace, &before, &after)
            .await
            .map_err(ApiError::Internal)?;
        Ok(TurnDiff {
            turn_id,
            available: true,
            reason: None,
            files,
            truncated,
            undone,
        })
    }

    /// Active agent, its advertisement and the session's choices (read through the session actor).
    async fn session_options(
        &self,
        session_id: &SessionId,
    ) -> ApiResult<(AgentId, Advertised, Prefs)> {
        let (agent, prefs) = self.ask(session_id, SessionCmd::Prefs).await?;
        let advertised = self.ctx.advertised(&agent).await;
        Ok((agent, advertised, prefs))
    }

    async fn do_list_agents(&self) -> Vec<AgentInfo> {
        let mut out = Vec::new();
        for spec in &self.ctx.config.agents {
            out.push(AgentInfo {
                id: spec.id.clone(),
                display_name: spec.display_name.clone(),
                availability: spec.probe(),
                quota: self.ctx.quota.get(&spec.id),
                runtime: self.ctx.pool.runtime_state(&spec.id).await,
            });
        }
        out
    }

    pub(crate) async fn do_create_session(
        &self,
        request: CreateSession,
    ) -> ApiResult<SessionSummary> {
        self.create_inner(request, None, None).await
    }

    pub(crate) async fn create_router_session(
        &self,
        request: CreateSession,
        source_id: &str,
    ) -> ApiResult<SessionSummary> {
        self.create_inner(request, None, Some(source_id.to_owned()))
            .await
    }

    /// A subagent session of `parent` (§9.8): registered as a child before `SessionCreated` is emitted, so no
    /// client ever lists it as a top-level session.
    pub(crate) async fn create_child_session(
        &self,
        request: CreateSession,
        parent: &SessionId,
    ) -> ApiResult<SessionSummary> {
        self.create_inner(request, Some(parent.clone()), None).await
    }

    #[allow(clippy::too_many_lines)]
    async fn create_inner(
        &self,
        request: CreateSession,
        parent: Option<SessionId>,
        router: Option<String>,
    ) -> ApiResult<SessionSummary> {
        self.check_open()?;
        let key = WorkspaceKey::resolve(
            request.workspace_identity.as_deref(),
            &request.workspace_path,
        )
        .map_err(|e| ApiError::InvalidArgument(e.to_string()))?;
        let agent = match request.agent {
            Some(agent) => {
                // Several API keys of one service: sessions take them in turn. The rotation only picks
                // members that are installed, so the availability check comes after it.
                let agent = self.ctx.rotated(agent);
                self.ctx.require_available(&agent)?;
                agent
            }
            None => self.ctx.rotated(
                self.ctx
                    .fallback_order()
                    .into_iter()
                    .find(|a| self.ctx.is_installed(a) && self.ctx.quota.is_usable(a))
                    .or_else(|| self.ctx.fallback_order().into_iter().next())
                    .ok_or_else(|| ApiError::AgentUnavailable("no agents configured".into()))?,
            ),
        };
        let now = now_ms();
        let child = parent.as_ref().map(|p| {
            crate::delegate_server::new_view(
                &SessionId::new(new_id("ses")),
                p,
                &agent,
                request.title.as_deref().unwrap_or_default(),
            )
        });
        let id = child
            .as_ref()
            .map_or_else(|| SessionId::new(new_id("ses")), |c| c.session_id.clone());
        let mut workspace_path = request.workspace_path;
        let worktree = match request.worktree.as_ref().filter(|_| parent.is_none()) {
            Some(wanted) => {
                let created = crate::worktree::create(
                    &self.ctx.config.home,
                    std::path::Path::new(&workspace_path),
                    id.as_str(),
                    wanted,
                )
                .await
                .map_err(ApiError::InvalidArgument)?;
                workspace_path = created.cwd.to_string_lossy().into_owned();
                Some(created.info)
            }
            None => None,
        };
        let row = SessionRow {
            id,
            title: request.title.unwrap_or_else(|| "New session".into()),
            workspace_path,
            workspace_key: key.as_str().to_owned(),
            active_agent: agent,
            created_at: now,
            updated_at: now,
        };
        // A new session in a removed project restores the project (phase-9-parity.md §9.1.5a).
        let stored_child = child.clone();
        let router_source = router.clone();
        let (summary, restored) = self
            .ctx
            .store
            .run(move |c| {
                let restored = router_source.is_none()
                    && store::set_project_removed(c, &row.workspace_key, None)?;
                store::insert_session(c, &row)?;
                if let Some(source) = &router_source {
                    c.execute(
                        "INSERT INTO inbox_routers(source_id, session_id, agent, batches) VALUES(?1, ?2, ?3, 0)
                         ON CONFLICT(source_id) DO UPDATE SET session_id=?2, agent=?3, batches=0",
                        rusqlite::params![source, row.id.as_str(), row.active_agent.as_str()],
                    )?;
                }
                if let Some(info) = &worktree {
                    crate::store_chat::set_worktree(c, &row.id, info)?;
                }
                if let Some(view) = &stored_child {
                    crate::delegate::save(c, view)?;
                }
                // New sessions start read (phase-9-parity.md §9.2.1 set_unread).
                crate::store_chat::mark_read(c, &row.id, now)?;
                Ok((store::summary(c, &row)?, restored))
            })
            .await?;
        if restored {
            self.ctx.events.emit(None, EventKind::SessionsChanged);
        }
        if let Some(view) = child {
            self.ctx.put_subagent(view).await;
        }
        if let Some(source) = &router {
            self.ctx.routers.add(&summary.id, source);
        }
        self.ctx.events.emit(
            Some(&summary.id),
            EventKind::SessionCreated {
                summary: summary.clone(),
            },
        );
        Ok(summary)
    }

    async fn do_set_fallback(&self, order: Vec<AgentId>) -> ApiResult<()> {
        if order.is_empty() {
            return Err(ApiError::InvalidArgument("fallback order is empty".into()));
        }
        if let Some(unknown) = order.iter().find(|a| !self.ctx.specs.contains_key(*a)) {
            return Err(ApiError::InvalidArgument(format!(
                "unknown agent {unknown}"
            )));
        }
        let json = serde_json::to_string(&order).map_err(|e| ApiError::Internal(e.to_string()))?;
        self.ctx
            .store
            .run(move |c| store::set_setting(c, FALLBACK_KEY, &json))
            .await?;
        if let Ok(mut current) = self.ctx.fallback.lock() {
            *current = order;
        }
        Ok(())
    }

    async fn do_shutdown(&self) -> ApiResult<()> {
        if self.shut_down.swap(true, Ordering::SeqCst) {
            return Ok(());
        }
        self.scheduler.stop();
        self.inbox.stop();
        self.board_sync.stop();
        if let Some(task) = self.delegate_task.lock().ok().and_then(|mut t| t.take()) {
            task.abort();
        }
        let actors: Vec<ActorTx> = self.actors.lock().await.drain().map(|(_, tx)| tx).collect();
        for tx in actors {
            let (reply, rx) = oneshot::channel();
            if tx.send(ActorMsg::Cmd(SessionCmd::Shutdown(reply))).is_ok() {
                let _ = rx.await;
            }
        }
        self.ctx.pool.shutdown_all().await;
        Ok(())
    }
}

impl HarnessCore for Hub {
    fn list_agents(&self) -> BoxFuture<'_, ApiResult<Vec<AgentInfo>>> {
        Box::pin(async move { Ok(self.do_list_agents().await) })
    }

    fn get_fallback_order(&self) -> BoxFuture<'_, ApiResult<Vec<AgentId>>> {
        Box::pin(async move { Ok(self.ctx.fallback_order()) })
    }

    fn set_fallback_order(&self, order: Vec<AgentId>) -> BoxFuture<'_, ApiResult<()>> {
        Box::pin(self.do_set_fallback(order))
    }

    fn create_session(&self, request: CreateSession) -> BoxFuture<'_, ApiResult<SessionSummary>> {
        Box::pin(self.do_create_session(request))
    }

    fn list_sessions(
        &self,
        request: ListSessions,
    ) -> BoxFuture<'_, ApiResult<Vec<SessionSummary>>> {
        Box::pin(async move {
            self.check_open()?;
            let limit = request.limit.unwrap_or(200).min(10_000);
            Ok(self
                .ctx
                .store
                .run(move |c| {
                    store::list_sessions(
                        c,
                        request.workspace_key.as_deref(),
                        request.query.as_deref(),
                        limit,
                    )
                })
                .await?)
        })
    }

    fn open_session(&self, session_id: SessionId) -> BoxFuture<'_, ApiResult<SessionDetail>> {
        Box::pin(async move { self.ask(&session_id, SessionCmd::Snapshot).await })
    }

    fn send_message(&self, request: SendMessage) -> BoxFuture<'_, ApiResult<SendAccepted>> {
        Box::pin(async move {
            let id = request.session_id.clone();
            self.check_open()?;
            self.unarchive_for_send(&id).await?;
            self.ask(&id, |reply| SessionCmd::Send(request, reply))
                .await
        })
    }

    fn set_active_agent(
        &self,
        session_id: SessionId,
        agent: AgentId,
    ) -> BoxFuture<'_, ApiResult<()>> {
        Box::pin(async move {
            self.ask(&session_id, |reply| SessionCmd::SetAgent(agent, reply))
                .await
        })
    }

    fn stop(&self, session_id: SessionId) -> BoxFuture<'_, ApiResult<()>> {
        Box::pin(async move {
            // §9.8: stopping a parent leaves its subagents running; their results reach the parent later
            // (delegate_server::push). A subagent is stopped from its own session.
            self.ask(&session_id, SessionCmd::Stop).await
        })
    }
    fn resumable_run(
        &self,
        session_id: SessionId,
    ) -> BoxFuture<'_, ApiResult<Option<ResumableRun>>> {
        Box::pin(self.do_resumable_run(session_id))
    }

    fn resume_run(&self, session_id: SessionId) -> BoxFuture<'_, ApiResult<SendAccepted>> {
        Box::pin(self.do_resume_run(session_id))
    }

    fn discard_run(&self, session_id: SessionId) -> BoxFuture<'_, ApiResult<()>> {
        Box::pin(self.do_discard_run(session_id))
    }

    fn cancel_queued(
        &self,
        session_id: SessionId,
        turn_id: TurnId,
    ) -> BoxFuture<'_, ApiResult<()>> {
        Box::pin(async move {
            self.ask(&session_id, |reply| {
                SessionCmd::CancelQueued(turn_id, reply)
            })
            .await
        })
    }

    fn edit_queued(
        &self,
        session_id: SessionId,
        turn_id: TurnId,
        text: String,
    ) -> BoxFuture<'_, ApiResult<()>> {
        Box::pin(async move {
            self.ask(&session_id, |reply| {
                SessionCmd::EditQueued(turn_id, text, reply)
            })
            .await
        })
    }

    fn reorder_queued(
        &self,
        session_id: SessionId,
        turn_ids: Vec<TurnId>,
    ) -> BoxFuture<'_, ApiResult<()>> {
        Box::pin(async move {
            self.ask(&session_id, |reply| {
                SessionCmd::ReorderQueued(turn_ids, reply)
            })
            .await
        })
    }

    fn run_queued_now(
        &self,
        session_id: SessionId,
        turn_id: TurnId,
    ) -> BoxFuture<'_, ApiResult<()>> {
        Box::pin(async move {
            self.ask(&session_id, |reply| {
                SessionCmd::RunQueuedNow(turn_id, reply)
            })
            .await
        })
    }

    fn steer(
        &self,
        request: SendMessage,
    ) -> BoxFuture<'_, ApiResult<harness_core_api::SteerAccepted>> {
        Box::pin(async move {
            let session = request.session_id.clone();
            self.ask(&session, |reply| SessionCmd::Steer(request, reply))
                .await
        })
    }

    fn stop_background_task(
        &self,
        session_id: SessionId,
        task_id: String,
    ) -> BoxFuture<'_, ApiResult<()>> {
        Box::pin(async move {
            self.ask(&session_id, |reply| {
                SessionCmd::StopBackgroundTask(task_id, reply)
            })
            .await
        })
    }

    fn respond_permission(
        &self,
        session_id: SessionId,
        request_id: PermissionRequestId,
        option_id: String,
    ) -> BoxFuture<'_, ApiResult<()>> {
        Box::pin(async move {
            self.ask(&session_id, |reply| {
                SessionCmd::RespondPermission(request_id, option_id, reply)
            })
            .await
        })
    }

    fn memory_read(&self, scope: MemoryScope) -> BoxFuture<'_, ApiResult<String>> {
        Box::pin(async move {
            let memory = self.ctx.memory.clone();
            let target = Self::memory_target(&scope);
            tokio::task::spawn_blocking(move || memory.read(&target))
                .await
                .map_err(|e| ApiError::Internal(e.to_string()))?
                .map_err(|e| ApiError::Storage(e.to_string()))
        })
    }

    fn memory_write(
        &self,
        scope: MemoryScope,
        content: String,
        mode: MemoryWriteMode,
    ) -> BoxFuture<'_, ApiResult<()>> {
        Box::pin(async move {
            let memory = self.ctx.memory.clone();
            let target = Self::memory_target(&scope);
            let append = mode == MemoryWriteMode::Append;
            tokio::task::spawn_blocking(move || memory.write(&target, &content, append))
                .await
                .map_err(|e| ApiError::Internal(e.to_string()))?
                .map_err(|e| ApiError::Storage(e.to_string()))?;
            self.ctx
                .events
                .emit(None, EventKind::MemoryChanged { scope });
            Ok(())
        })
    }

    fn turn_diff(
        &self,
        session_id: SessionId,
        turn_id: TurnId,
    ) -> BoxFuture<'_, ApiResult<TurnDiff>> {
        Box::pin(self.do_turn_diff(session_id, turn_id))
    }

    fn agent_options(&self, agent: AgentId) -> BoxFuture<'_, ApiResult<AgentOptions>> {
        Box::pin(async move {
            self.check_open()?;
            if !self.ctx.specs.contains_key(&agent) {
                return Err(ApiError::NotFound(format!("agent {agent}")));
            }
            self.ctx.probe_advertised(&agent).await;
            let advertised = self.ctx.advertised(&agent).await;
            let defaults = self.ctx.config.agent_defaults.get(&agent);
            let none = Prefs::default();
            Ok(AgentOptions {
                modes: options::modes_view(agent.clone(), &advertised, &none, defaults),
                models: options::models_view(agent.clone(), &advertised, &none, defaults),
                config: options::config_view(agent, &advertised, &none, defaults),
            })
        })
    }

    fn list_modes(&self, session_id: SessionId) -> BoxFuture<'_, ApiResult<SessionModes>> {
        Box::pin(async move {
            let (agent, advertised, prefs) = self.session_options(&session_id).await?;
            let defaults = self.ctx.config.agent_defaults.get(&agent);
            Ok(options::modes_view(
                agent.clone(),
                &advertised,
                &prefs,
                defaults,
            ))
        })
    }

    fn list_models(&self, session_id: SessionId) -> BoxFuture<'_, ApiResult<SessionModels>> {
        Box::pin(async move {
            let (agent, advertised, prefs) = self.session_options(&session_id).await?;
            let defaults = self.ctx.config.agent_defaults.get(&agent);
            Ok(options::models_view(
                agent.clone(),
                &advertised,
                &prefs,
                defaults,
            ))
        })
    }

    fn list_config_options(
        &self,
        session_id: SessionId,
    ) -> BoxFuture<'_, ApiResult<SessionConfigOptions>> {
        Box::pin(async move {
            let (agent, advertised, prefs) = self.session_options(&session_id).await?;
            let defaults = self.ctx.config.agent_defaults.get(&agent);
            Ok(options::config_view(
                agent.clone(),
                &advertised,
                &prefs,
                defaults,
            ))
        })
    }

    fn set_mode(&self, session_id: SessionId, mode_id: String) -> BoxFuture<'_, ApiResult<()>> {
        Box::pin(async move {
            self.ask(&session_id, |reply| {
                SessionCmd::SetPref(PrefChange::Mode(mode_id), reply)
            })
            .await
        })
    }

    fn set_model(&self, session_id: SessionId, model_id: String) -> BoxFuture<'_, ApiResult<()>> {
        Box::pin(async move {
            self.ask(&session_id, |reply| {
                SessionCmd::SetPref(PrefChange::Model(model_id), reply)
            })
            .await
        })
    }

    fn set_config_option(
        &self,
        session_id: SessionId,
        config_id: String,
        value: String,
    ) -> BoxFuture<'_, ApiResult<()>> {
        Box::pin(async move {
            self.ask(&session_id, |reply| {
                SessionCmd::SetPref(PrefChange::Config(config_id, value), reply)
            })
            .await
        })
    }

    fn set_machine(&self, session_id: SessionId, on: bool) -> BoxFuture<'_, ApiResult<()>> {
        Box::pin(async move {
            self.ctx
                .machine
                .set(session_id.as_str(), on)
                .await
                .map_err(|error| ApiError::Internal(format!("machine mode: {error}")))
        })
    }

    fn machine(&self, session_id: SessionId) -> BoxFuture<'_, ApiResult<bool>> {
        Box::pin(async move { Ok(self.ctx.machine.is_on(session_id.as_str())) })
    }

    fn agents_overview(&self) -> BoxFuture<'_, ApiResult<Vec<AgentOverview>>> {
        Box::pin(self.do_agents_overview())
    }

    fn setup_status(&self) -> BoxFuture<'_, ApiResult<harness_core_api::SetupStatus>> {
        Box::pin(async move {
            self.check_open()?;
            Ok(self.do_setup_status())
        })
    }

    fn install_gh(&self) -> BoxFuture<'_, ApiResult<()>> {
        Box::pin(self.do_install_gh())
    }

    fn install_adapter(
        &self,
        agent: AgentId,
    ) -> BoxFuture<'_, ApiResult<harness_core_api::AdapterInstallResult>> {
        Box::pin(async move {
            self.check_open()?;
            self.do_install_adapter(agent).await
        })
    }

    fn test_agent(&self, agent: AgentId) -> BoxFuture<'_, ApiResult<AgentTestResult>> {
        Box::pin(self.do_test_agent(agent))
    }

    fn list_accounts(&self) -> BoxFuture<'_, ApiResult<Vec<AccountInfo>>> {
        Box::pin(async move {
            self.check_open()?;
            let loaded = |id: &AgentId| self.ctx.specs.contains_key(id);
            crate::accounts::list(&self.ctx.config, &loaded).await
        })
    }

    fn add_account(&self, request: AddAccount) -> BoxFuture<'_, ApiResult<AccountInfo>> {
        Box::pin(async move {
            self.check_open()?;
            let loaded = |id: &AgentId| self.ctx.specs.contains_key(id);
            crate::accounts::add(&self.ctx.config, request, &loaded).await
        })
    }

    fn remove_account(&self, id: AgentId) -> BoxFuture<'_, ApiResult<()>> {
        Box::pin(async move {
            self.check_open()?;
            crate::accounts::remove(&self.ctx.config, &id).await
        })
    }

    fn agy_accounts(
        &self,
        op: harness_core_api::AgyAccountOp,
    ) -> BoxFuture<'_, ApiResult<harness_core_api::AgyAccounts>> {
        Box::pin(async move {
            self.check_open()?;
            crate::agy_ops::run(&self.ctx, op).await
        })
    }

    fn list_providers(&self) -> BoxFuture<'_, ApiResult<Vec<ProviderInfo>>> {
        Box::pin(async move {
            self.check_open()?;
            let loaded = |id: &AgentId| self.ctx.specs.contains_key(id);
            crate::providers::list(&self.ctx.config, &loaded).await
        })
    }

    fn add_provider(&self, request: AddProvider) -> BoxFuture<'_, ApiResult<ProviderInfo>> {
        Box::pin(async move {
            self.check_open()?;
            let loaded = |id: &AgentId| self.ctx.specs.contains_key(id);
            crate::providers::add(&self.ctx.config, request, &loaded).await
        })
    }

    fn update_provider(&self, request: UpdateProvider) -> BoxFuture<'_, ApiResult<ProviderInfo>> {
        Box::pin(async move {
            self.check_open()?;
            let loaded = |id: &AgentId| self.ctx.specs.contains_key(id);
            crate::providers::update(&self.ctx.config, request, &loaded).await
        })
    }

    fn remove_provider(&self, id: AgentId) -> BoxFuture<'_, ApiResult<()>> {
        Box::pin(async move {
            self.check_open()?;
            crate::providers::remove(&self.ctx.config, &id).await
        })
    }

    fn memory_files(&self) -> BoxFuture<'_, ApiResult<Vec<MemoryFileInfo>>> {
        Box::pin(self.do_memory_files())
    }

    fn memory_doc(&self, scope: MemoryScope) -> BoxFuture<'_, ApiResult<MemoryDoc>> {
        Box::pin(self.do_memory_doc(scope))
    }

    fn memory_plan(
        &self,
        scope: MemoryScope,
        content: String,
    ) -> BoxFuture<'_, ApiResult<MemoryPlan>> {
        Box::pin(self.do_memory_plan(scope, content))
    }

    fn memory_apply(&self, plan: MemoryPlan) -> BoxFuture<'_, ApiResult<()>> {
        Box::pin(self.do_memory_apply(plan))
    }

    fn list_projects(&self, request: ListProjects) -> BoxFuture<'_, ApiResult<Vec<ProjectInfo>>> {
        Box::pin(self.do_list_projects(request))
    }

    fn remove_projects(
        &self,
        keys: Vec<String>,
        delete_data: bool,
    ) -> BoxFuture<'_, ApiResult<()>> {
        Box::pin(self.do_remove_projects(keys, delete_data))
    }

    fn restore_projects(&self, keys: Vec<String>) -> BoxFuture<'_, ApiResult<()>> {
        Box::pin(self.do_restore_projects(keys))
    }

    fn archive_sessions(&self, ids: Vec<SessionId>) -> BoxFuture<'_, ApiResult<()>> {
        Box::pin(self.do_archive_sessions(ids))
    }

    fn list_archived(&self, request: ListArchived) -> BoxFuture<'_, ApiResult<ArchivePage>> {
        Box::pin(self.do_list_archived(request))
    }

    fn unarchive(&self, selection: SessionSelection) -> BoxFuture<'_, ApiResult<u32>> {
        Box::pin(self.do_unarchive(selection))
    }

    fn delete_archived(&self, selection: SessionSelection) -> BoxFuture<'_, ApiResult<u32>> {
        Box::pin(self.do_delete_archived(selection))
    }

    // ---- API 0.5.0 (phase-9-parity.md §9.2.1; bodies in hub_chat.rs) ------------------------------------

    fn rename_session(&self, session_id: SessionId, title: String) -> BoxFuture<'_, ApiResult<()>> {
        Box::pin(self.do_rename(session_id, title))
    }

    fn set_pinned(
        &self,
        session_ids: Vec<SessionId>,
        pinned: bool,
    ) -> BoxFuture<'_, ApiResult<()>> {
        Box::pin(self.do_set_pinned(session_ids, pinned))
    }

    fn set_unread(&self, session_id: SessionId, unread: bool) -> BoxFuture<'_, ApiResult<()>> {
        Box::pin(self.do_set_unread(session_id, unread))
    }

    fn list_groups(&self) -> BoxFuture<'_, ApiResult<Vec<SessionGroup>>> {
        Box::pin(self.do_list_groups())
    }

    fn create_group(
        &self,
        name: String,
        color: GroupColor,
    ) -> BoxFuture<'_, ApiResult<SessionGroup>> {
        Box::pin(self.do_create_group(name, color))
    }

    fn update_group(&self, request: UpdateGroup) -> BoxFuture<'_, ApiResult<SessionGroup>> {
        Box::pin(self.do_update_group(request))
    }

    fn delete_group(&self, group_id: String) -> BoxFuture<'_, ApiResult<()>> {
        Box::pin(self.do_delete_group(group_id))
    }

    fn reorder_groups(&self, group_ids: Vec<String>) -> BoxFuture<'_, ApiResult<()>> {
        Box::pin(self.do_reorder_groups(group_ids))
    }

    fn move_sessions(
        &self,
        session_ids: Vec<SessionId>,
        group_id: Option<String>,
    ) -> BoxFuture<'_, ApiResult<()>> {
        Box::pin(self.do_move_sessions(session_ids, group_id))
    }

    fn fork_session(&self, request: ForkSession) -> BoxFuture<'_, ApiResult<SessionSummary>> {
        Box::pin(self.do_fork(request))
    }

    fn rewind_session(&self, request: Rewind) -> BoxFuture<'_, ApiResult<RewindResult>> {
        Box::pin(self.do_rewind(request))
    }

    fn edit_and_resend(&self, request: EditResend) -> BoxFuture<'_, ApiResult<SendAccepted>> {
        Box::pin(self.do_edit_and_resend(request))
    }

    fn retry_turn(&self, request: Retry) -> BoxFuture<'_, ApiResult<SendAccepted>> {
        Box::pin(self.do_retry(request))
    }

    fn plan_file_changes(
        &self,
        session_id: SessionId,
        turn_id: TurnId,
        direction: ChangeDirection,
    ) -> BoxFuture<'_, ApiResult<FileChangePlan>> {
        Box::pin(self.do_plan_files(session_id, turn_id, direction))
    }

    fn apply_file_changes(
        &self,
        request: ApplyFileChanges,
    ) -> BoxFuture<'_, ApiResult<FileRestoreReport>> {
        Box::pin(self.do_apply_files(request))
    }

    fn find_in_session(&self, request: FindInSession) -> BoxFuture<'_, ApiResult<Vec<FindHit>>> {
        Box::pin(self.do_find(request))
    }

    fn resolve_context(
        &self,
        session_id: SessionId,
        items: Vec<ContextItem>,
    ) -> BoxFuture<'_, ApiResult<Vec<ResolvedItem>>> {
        Box::pin(self.do_resolve_context(session_id, items))
    }

    fn mention_candidates(
        &self,
        request: MentionQuery,
    ) -> BoxFuture<'_, ApiResult<Vec<MentionCandidate>>> {
        Box::pin(self.do_mentions(request))
    }

    fn list_commands(&self, session_id: SessionId) -> BoxFuture<'_, ApiResult<CommandList>> {
        Box::pin(self.do_list_commands(session_id))
    }

    fn command_search(
        &self,
        request: CommandSearch,
    ) -> BoxFuture<'_, ApiResult<CommandSearchResult>> {
        Box::pin(self.do_command_search(request))
    }

    fn import_scan(&self, request: ImportScan) -> BoxFuture<'_, ApiResult<Vec<ImportCandidate>>> {
        Box::pin(self.do_import_scan(request))
    }

    fn import_sessions(&self, request: ImportRequest) -> BoxFuture<'_, ApiResult<ImportReport>> {
        Box::pin(self.do_import(request))
    }

    fn schedules_enabled(&self) -> BoxFuture<'_, ApiResult<bool>> {
        Box::pin(self.do_schedules_enabled())
    }

    fn get_orchestra(&self) -> BoxFuture<'_, ApiResult<harness_core_api::Orchestra>> {
        Box::pin(self.do_get_orchestra())
    }

    fn set_orchestra(
        &self,
        orchestra: harness_core_api::Orchestra,
    ) -> BoxFuture<'_, ApiResult<()>> {
        Box::pin(self.do_set_orchestra(orchestra))
    }

    fn set_schedules_enabled(&self, enabled: bool) -> BoxFuture<'_, ApiResult<()>> {
        Box::pin(self.do_set_schedules_enabled(enabled))
    }

    fn list_schedules(&self) -> BoxFuture<'_, ApiResult<Vec<ScheduledPrompt>>> {
        Box::pin(self.do_list_schedules())
    }

    fn create_schedule(&self, request: NewSchedule) -> BoxFuture<'_, ApiResult<ScheduledPrompt>> {
        Box::pin(self.do_create_schedule(request))
    }

    fn update_schedule(
        &self,
        request: UpdateSchedule,
    ) -> BoxFuture<'_, ApiResult<ScheduledPrompt>> {
        Box::pin(self.do_update_schedule(request))
    }

    fn delete_schedule(&self, schedule_id: String) -> BoxFuture<'_, ApiResult<()>> {
        Box::pin(self.do_delete_schedule(schedule_id))
    }

    fn run_schedule_now(&self, schedule_id: String) -> BoxFuture<'_, ApiResult<ScheduleRun>> {
        Box::pin(self.do_run_schedule_now(schedule_id))
    }

    fn board(&self, workspace_key: String) -> BoxFuture<'_, ApiResult<harness_core_api::Board>> {
        Box::pin(self.do_board(workspace_key))
    }

    fn create_card(
        &self,
        request: harness_core_api::NewCard,
    ) -> BoxFuture<'_, ApiResult<harness_core_api::CardView>> {
        Box::pin(self.do_create_card(request))
    }

    fn update_card(
        &self,
        request: harness_core_api::UpdateCard,
    ) -> BoxFuture<'_, ApiResult<harness_core_api::CardView>> {
        Box::pin(self.do_update_card(request))
    }

    fn move_card(
        &self,
        card_id: String,
        lane: harness_core_api::BoardLane,
        index: u32,
    ) -> BoxFuture<'_, ApiResult<harness_core_api::CardView>> {
        Box::pin(self.do_move_card(card_id, lane, index))
    }

    fn delete_card(&self, card_id: String) -> BoxFuture<'_, ApiResult<()>> {
        Box::pin(self.do_delete_card(card_id))
    }

    fn start_card(&self, card_id: String) -> BoxFuture<'_, ApiResult<harness_core_api::CardView>> {
        Box::pin(self.do_start_card(card_id))
    }

    fn link_board(
        &self,
        workspace_key: String,
    ) -> BoxFuture<'_, ApiResult<harness_core_api::Board>> {
        Box::pin(self.do_link_board(workspace_key))
    }

    fn unlink_board(&self, workspace_key: String) -> BoxFuture<'_, ApiResult<()>> {
        Box::pin(self.do_unlink_board(workspace_key))
    }

    fn sync_board(
        &self,
        workspace_key: String,
    ) -> BoxFuture<'_, ApiResult<harness_core_api::Board>> {
        Box::pin(self.do_sync_board(workspace_key))
    }

    fn inbox_status(&self) -> BoxFuture<'_, ApiResult<harness_core_api::InboxStatus>> {
        Box::pin(self.do_inbox_status())
    }

    fn set_inbox_enabled(&self, enabled: bool) -> BoxFuture<'_, ApiResult<()>> {
        Box::pin(self.do_set_inbox_enabled(enabled))
    }

    fn save_inbox_source(
        &self,
        source: harness_core_api::InboxSourceInput,
    ) -> BoxFuture<'_, ApiResult<harness_core_api::InboxSource>> {
        Box::pin(self.do_save_inbox_source(source))
    }

    fn delete_inbox_source(&self, source_id: String) -> BoxFuture<'_, ApiResult<()>> {
        Box::pin(self.do_delete_inbox_source(source_id))
    }

    fn poll_inbox_now(&self, source_id: String) -> BoxFuture<'_, ApiResult<()>> {
        Box::pin(self.do_poll_inbox_now(source_id))
    }

    fn list_tickets(&self) -> BoxFuture<'_, ApiResult<Vec<harness_core_api::Ticket>>> {
        Box::pin(self.do_list_tickets())
    }

    fn list_triggers(
        &self,
        limit: u32,
    ) -> BoxFuture<'_, ApiResult<Vec<harness_core_api::TriggerRecord>>> {
        Box::pin(self.do_list_triggers(limit))
    }

    fn post_ticket(&self, ticket_id: String) -> BoxFuture<'_, ApiResult<()>> {
        Box::pin(self.do_post_ticket(ticket_id))
    }

    fn stop_ticket(&self, ticket_id: String) -> BoxFuture<'_, ApiResult<()>> {
        Box::pin(self.do_stop_ticket(ticket_id))
    }

    fn discard_ticket(&self, ticket_id: String) -> BoxFuture<'_, ApiResult<()>> {
        Box::pin(self.do_discard_ticket(ticket_id))
    }

    fn subscribe(&self) -> EventStream {
        self.ctx.events.subscribe()
    }

    fn shutdown(&self) -> BoxFuture<'_, ApiResult<()>> {
        Box::pin(self.do_shutdown())
    }
}
