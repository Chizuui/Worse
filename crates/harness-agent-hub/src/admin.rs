//! Agents overview, MEMORY.md plan/apply, projects and archive (phase-9-parity.md §9.1.5a).
//!
//! Owners: the store for rows; session actors for live state. Archive/delete/remove-with-data hold the actor
//! registry lock while they retire the affected actors and change the store, so no actor is re-spawned in between.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use harness_agent_config::{AgentKind, AuthState, ConfigPaths};
use harness_core_api::{
    AgentId, AgentOverview, AgentTestResult, ApiError, ApiResult, ArchivePage, ArchivedSession,
    AuthPresence, EventKind, ListArchived, ListProjects, MemoryDoc, MemoryFileInfo, MemoryPlan,
    MemoryScope, ProjectInfo, SessionId, SessionSelection,
};
use serde_json::json;
use tokio::sync::oneshot;

use crate::acp::AcpConnection;
use crate::agents::resolve_program;
use crate::hub::Hub;
use crate::memory::fingerprint;
use crate::session::{ActorMsg, ActorTx, SessionCmd};
use crate::store;
use crate::util::now_ms;

/// ACP `initialize` budget of the Test button.
const TEST_TIMEOUT: Duration = Duration::from_secs(20);
/// A project without a session update for this long is "unused".
const UNUSED_AFTER_MS: i64 = 30 * 24 * 60 * 60 * 1000;
const INFO_PREFIX: &str = "agent_info:";
const ARCHIVE_PAGE_MAX: u32 = 200;

fn login_hint(kind: AgentKind) -> Option<String> {
    match kind {
        AgentKind::ClaudeCode => {
            Some("Run `claude` in a terminal and sign in (or set ANTHROPIC_API_KEY).".into())
        }
        AgentKind::Codex => Some("Run `codex login` in a terminal (or set OPENAI_API_KEY).".into()),
        AgentKind::Agy => {
            Some("Run `agy` in a terminal and sign in (or set GEMINI_API_KEY).".into())
        }
        AgentKind::Harness => None,
    }
}

fn presence(state: AuthState) -> AuthPresence {
    match state {
        AuthState::Present => AuthPresence::Present,
        AuthState::Absent => AuthPresence::Absent,
        AuthState::Unknown => AuthPresence::Unknown,
    }
}

impl Hub {
    pub(crate) fn agent_kind(&self, agent: &AgentId) -> Option<AgentKind> {
        self.ctx
            .config
            .agent_kinds
            .get(agent)
            .copied()
            .or_else(|| AgentKind::from_agent_id(agent.as_str()))
            .filter(|k| *k != AgentKind::Harness)
    }

    async fn cached_info(&self, agent: &AgentId) -> (Option<String>, Option<String>) {
        let key = format!("{INFO_PREFIX}{agent}");
        let stored = self
            .ctx
            .store
            .run(move |c| store::get_setting(c, &key))
            .await
            .ok()
            .flatten()
            .and_then(|json| serde_json::from_str::<serde_json::Value>(&json).ok());
        let field = |k: &str| {
            stored
                .as_ref()
                .and_then(|v| v.get(k))
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
        };
        (field("name"), field("version"))
    }

    async fn record_info(&self, agent: &AgentId, info: &(Option<String>, Option<String>)) {
        if info.1.is_none() || &self.cached_info(agent).await == info {
            return;
        }
        let key = format!("{INFO_PREFIX}{agent}");
        let json = json!({"name": info.0, "version": info.1}).to_string();
        if let Err(error) = self
            .ctx
            .store
            .run(move |c| store::set_setting(c, &key, &json))
            .await
        {
            tracing::warn!(%error, "saving agent info failed");
        }
    }

    pub(crate) async fn do_agents_overview(&self) -> ApiResult<Vec<AgentOverview>> {
        self.check_open()?;
        let paths = ConfigPaths::from_env(&self.ctx.config.home, None).ok();
        let mut out = Vec::new();
        for spec in &self.ctx.config.agents {
            let availability = spec.probe();
            let adapter_package = crate::setup::known_cli(&spec.program)
                .and_then(|k| k.package)
                .map(str::to_owned);
            let (installed, command_path, install_hint, adapter_missing) = match availability {
                harness_core_api::Availability::Available { command_path } => {
                    (true, Some(command_path), None, false)
                }
                harness_core_api::Availability::NotInstalled { hint } => {
                    (false, None, Some(hint), false)
                }
                harness_core_api::Availability::AdapterMissing { hint, .. } => {
                    (false, None, Some(hint), true)
                }
            };
            if let Some(live) = self.ctx.pool.agent_info(&spec.id).await {
                self.record_info(&spec.id, &live).await;
            }
            let (_, version) = self.cached_info(&spec.id).await;
            let kind = self.agent_kind(&spec.id);
            let account_auth = crate::accounts::auth_for_spec(&self.ctx.config, spec).await;
            let auth =
                match (kind, &paths) {
                    // 账号 agent（§9.6）：看其 profile 目录里的登录文件，而不是主 ~/.claude。
                    _ if account_auth.is_some() => account_auth.unwrap_or(AuthPresence::Unknown),
                    // API provider（§9.7）：有密钥即视为已登录（只看是否为空，不读取或返回密钥）。
                    _ if crate::providers::is_provider(spec) => {
                        if spec.env.iter().any(|(k, v)| {
                            k == crate::providers::ENV_API_KEY && !v.trim().is_empty()
                        }) {
                            AuthPresence::Present
                        } else {
                            AuthPresence::Absent
                        }
                    }
                    // A Command Code key (providers.rs): the key in its environment is its login.
                    _ if spec.env.iter().any(|(k, v)| {
                        k == crate::providers::ENV_COMMAND_CODE_KEY && !v.trim().is_empty()
                    }) =>
                    {
                        AuthPresence::Present
                    }
                    (Some(kind), Some(paths)) => {
                        presence(self.ctx.config.auth_probe.0.probe(kind, paths).await)
                    }
                    _ => AuthPresence::Unknown,
                };
            out.push(AgentOverview {
                id: spec.id.clone(),
                display_name: spec.display_name.clone(),
                installed,
                command_path,
                install_hint,
                version,
                auth,
                login_hint: kind.and_then(login_hint),
                runtime: self.ctx.pool.runtime_state(&spec.id).await,
                quota: self.ctx.quota.get(&spec.id),
                adapter_missing,
                adapter_package,
            });
        }
        Ok(out)
    }

    pub(crate) async fn do_test_agent(&self, agent: AgentId) -> ApiResult<AgentTestResult> {
        self.check_open()?;
        let spec = self
            .ctx
            .specs
            .get(&agent)
            .cloned()
            .ok_or_else(|| ApiError::NotFound(format!("agent {agent}")))?;
        let failed = |latency_ms: u64, error: String| AgentTestResult {
            agent: agent.clone(),
            ok: false,
            latency_ms,
            agent_name: None,
            version: None,
            error: Some(error),
        };
        let Some(program) = resolve_program(&spec.program) else {
            return Ok(failed(
                0,
                format!(
                    "{} is not installed: {}",
                    spec.display_name, spec.install_hint
                ),
            ));
        };
        let started = Instant::now();
        // A separate process (generation 0), never the pooled one: the test must not disturb running turns.
        let connection =
            match AcpConnection::spawn(&program, &spec.args, &spec.env, 0, Box::new(|| {})) {
                Ok(connection) => connection,
                Err(error) => return Ok(failed(0, format!("failed to start: {error}"))),
            };
        let result = tokio::time::timeout(TEST_TIMEOUT, connection.initialize()).await;
        let latency_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        let info = connection.agent_info();
        connection.shutdown().await;
        match result {
            Ok(Ok(())) => {
                self.record_info(&agent, &info).await;
                Ok(AgentTestResult {
                    agent,
                    ok: true,
                    latency_ms,
                    agent_name: info.0,
                    version: info.1,
                    error: None,
                })
            }
            Ok(Err(error)) => Ok(failed(latency_ms, format!("initialize failed: {error}"))),
            Err(_) => Ok(failed(
                latency_ms,
                format!("no initialize answer within {}s", TEST_TIMEOUT.as_secs()),
            )),
        }
    }

    // ---- memory ---------------------------------------------------------------------------------------

    fn memory_path(&self, scope: &MemoryScope) -> String {
        self.ctx
            .memory
            .path(&Self::memory_target(scope))
            .to_string_lossy()
            .into_owned()
    }

    async fn memory_read_opt(&self, scope: &MemoryScope) -> ApiResult<Option<String>> {
        let memory = self.ctx.memory.clone();
        let target = Self::memory_target(scope);
        tokio::task::spawn_blocking(move || memory.read_opt(&target))
            .await
            .map_err(|e| ApiError::Internal(e.to_string()))?
            .map_err(|e| ApiError::Storage(e.to_string()))
    }

    pub(crate) async fn do_memory_files(&self) -> ApiResult<Vec<MemoryFileInfo>> {
        let mut scopes = vec![MemoryScope::Global];
        for project in self.do_list_projects(ListProjects::default()).await? {
            scopes.push(MemoryScope::Project {
                workspace_path: project.workspace_path,
            });
        }
        let mut out = Vec::new();
        for scope in scopes {
            let path = self.memory_path(&scope);
            let meta = tokio::fs::metadata(&path).await.ok();
            out.push(MemoryFileInfo {
                exists: meta.is_some(),
                bytes: meta.as_ref().map_or(0, std::fs::Metadata::len),
                modified_ms: meta
                    .and_then(|m| m.modified().ok())
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .and_then(|d| i64::try_from(d.as_millis()).ok()),
                scope,
                path,
            });
        }
        Ok(out)
    }

    pub(crate) async fn do_memory_doc(&self, scope: MemoryScope) -> ApiResult<MemoryDoc> {
        self.check_open()?;
        let current = self.memory_read_opt(&scope).await?;
        Ok(MemoryDoc {
            path: self.memory_path(&scope),
            exists: current.is_some(),
            content: current.unwrap_or_default(),
            scope,
        })
    }

    pub(crate) async fn do_memory_plan(
        &self,
        scope: MemoryScope,
        content: String,
    ) -> ApiResult<MemoryPlan> {
        self.check_open()?;
        let current = self.memory_read_opt(&scope).await?;
        let before = current.as_deref().unwrap_or_default();
        let no_op = current.as_deref() == Some(content.as_str());
        let path = self.memory_path(&scope);
        let diff = if no_op {
            String::new()
        } else {
            harness_agent_config::unified_diff(&path, before, &content)
        };
        Ok(MemoryPlan {
            base: fingerprint(current.as_deref()),
            scope,
            path,
            content,
            diff,
            no_op,
        })
    }

    pub(crate) async fn do_memory_apply(&self, plan: MemoryPlan) -> ApiResult<()> {
        self.check_open()?;
        if plan.no_op {
            return Ok(());
        }
        let memory = self.ctx.memory.clone();
        let target = Self::memory_target(&plan.scope);
        let (base, content) = (plan.base.clone(), plan.content.clone());
        let written =
            tokio::task::spawn_blocking(move || memory.write_checked(&target, &base, &content))
                .await
                .map_err(|e| ApiError::Internal(e.to_string()))?
                .map_err(|e| ApiError::Storage(e.to_string()))?;
        if !written {
            return Err(ApiError::Conflict(format!(
                "{} changed since the plan was made; review it again",
                plan.path
            )));
        }
        self.ctx
            .events
            .emit(None, EventKind::MemoryChanged { scope: plan.scope });
        Ok(())
    }

    // ---- projects -------------------------------------------------------------------------------------

    pub(crate) async fn do_list_projects(
        &self,
        request: ListProjects,
    ) -> ApiResult<Vec<ProjectInfo>> {
        self.check_open()?;
        let rows = self.ctx.store.run(store::list_projects).await?;
        let now = now_ms();
        let query = request
            .query
            .as_deref()
            .map(|q| q.trim().to_lowercase())
            .filter(|q| !q.is_empty());
        let mut out: Vec<ProjectInfo> = rows
            .into_iter()
            .filter(|row| row.5.is_some() == request.removed)
            .filter(|row| {
                query
                    .as_ref()
                    .map_or(true, |q| row.1.to_lowercase().contains(q.as_str()))
            })
            .map(
                |(key, path, sessions, archived, last_used, removed_at)| ProjectInfo {
                    unused: last_used.map_or(true, |t| now - t > UNUSED_AFTER_MS),
                    workspace_key: key,
                    workspace_path: path,
                    sessions,
                    archived,
                    last_used,
                    removed_at,
                },
            )
            .collect();
        out.sort_by(|a, b| {
            b.last_used
                .cmp(&a.last_used)
                .then(a.workspace_path.cmp(&b.workspace_path))
        });
        Ok(out)
    }

    /// Asks each live actor of `ids` to retire (registry lock held by the caller). The first busy one aborts.
    async fn retire(actors: &mut HashMap<SessionId, ActorTx>, ids: &[SessionId]) -> ApiResult<()> {
        for id in ids {
            let Some(tx) = actors.get(id).cloned() else {
                continue;
            };
            let (reply, rx) = oneshot::channel();
            if tx.send(ActorMsg::Cmd(SessionCmd::Retire(reply))).is_err() {
                actors.remove(id);
                continue;
            }
            match rx.await {
                Ok(Err(error)) => return Err(error),
                Ok(Ok(())) | Err(_) => {
                    actors.remove(id);
                }
            }
        }
        Ok(())
    }

    pub(crate) async fn do_remove_projects(
        &self,
        keys: Vec<String>,
        delete_data: bool,
    ) -> ApiResult<()> {
        self.check_open()?;
        let mut actors = self.actors.lock().await;
        if delete_data {
            let lookup = keys.clone();
            let ids = self
                .ctx
                .store
                .run(move |c| {
                    let mut ids = Vec::new();
                    for key in &lookup {
                        ids.extend(store::session_ids_of_project(c, key)?);
                    }
                    Ok(ids)
                })
                .await?;
            Self::retire(&mut actors, &ids).await?;
            self.delete_rows(ids).await?;
        }
        let now = now_ms();
        self.ctx
            .store
            .run(move |c| {
                for key in &keys {
                    store::set_project_removed(c, key, Some(now))?;
                }
                Ok(())
            })
            .await?;
        drop(actors);
        self.ctx.events.emit(None, EventKind::SessionsChanged);
        Ok(())
    }

    pub(crate) async fn do_restore_projects(&self, keys: Vec<String>) -> ApiResult<()> {
        self.check_open()?;
        let changed = self
            .ctx
            .store
            .run(move |c| {
                let mut changed = false;
                for key in &keys {
                    changed |= store::set_project_removed(c, key, None)?;
                }
                Ok(changed)
            })
            .await?;
        if changed {
            self.ctx.events.emit(None, EventKind::SessionsChanged);
        }
        Ok(())
    }

    // ---- archive --------------------------------------------------------------------------------------

    pub(crate) async fn do_archive_sessions(&self, ids: Vec<SessionId>) -> ApiResult<()> {
        self.check_open()?;
        let mut actors = self.actors.lock().await;
        let lookup = ids.clone();
        let missing = self
            .ctx
            .store
            .run(move |c| {
                let mut missing = Vec::new();
                for id in &lookup {
                    if store::load_session(c, id)?.is_none() {
                        missing.push(id.clone());
                    }
                }
                Ok(missing)
            })
            .await?;
        if let Some(id) = missing.first() {
            return Err(ApiError::NotFound(format!("session {id}")));
        }
        Self::retire(&mut actors, &ids).await?;
        let now = now_ms();
        let changed = self
            .ctx
            .store
            .run(move |c| store::set_archived(c, &ids, Some(now)))
            .await?;
        drop(actors);
        if changed > 0 {
            self.ctx.events.emit(None, EventKind::SessionsChanged);
        }
        Ok(())
    }

    pub(crate) async fn do_list_archived(&self, request: ListArchived) -> ApiResult<ArchivePage> {
        self.check_open()?;
        let limit = request.limit.unwrap_or(50).clamp(1, ARCHIVE_PAGE_MAX);
        let offset = request.offset;
        let (rows, total) = self
            .ctx
            .store
            .run(move |c| store::list_archived(c, &request, limit))
            .await?;
        let shown = offset.saturating_add(u32::try_from(rows.len()).unwrap_or(u32::MAX));
        Ok(ArchivePage {
            rows: rows
                .into_iter()
                .map(|(summary, archived_at)| ArchivedSession {
                    summary,
                    archived_at,
                })
                .collect(),
            total,
            has_more: shown < total,
        })
    }

    async fn archived_selection(&self, selection: SessionSelection) -> ApiResult<Vec<SessionId>> {
        Ok(self
            .ctx
            .store
            .run(move |c| match &selection {
                SessionSelection::Sessions(ids) => store::filter_archived(c, ids, true),
                SessionSelection::Project(key) => store::archived_of_project(c, key),
            })
            .await?)
    }

    pub(crate) async fn do_unarchive(&self, selection: SessionSelection) -> ApiResult<u32> {
        self.check_open()?;
        let ids = self.archived_selection(selection).await?;
        let changed = self
            .ctx
            .store
            .run(move |c| store::set_archived(c, &ids, None))
            .await?;
        if changed > 0 {
            self.ctx.events.emit(None, EventKind::SessionsChanged);
        }
        Ok(changed)
    }

    async fn delete_rows(&self, ids: Vec<SessionId>) -> ApiResult<u32> {
        let (deleted, worktrees) = self
            .ctx
            .store
            .run(move |c| {
                let worktrees = crate::store_chat::worktrees_of(c, &ids)?;
                Ok((store::delete_sessions(c, &ids)?, worktrees))
            })
            .await?;
        for info in worktrees {
            if let Err(error) = crate::worktree::remove(&info).await {
                tracing::warn!(%error, path = %info.path, "worktree kept");
            }
        }
        Ok(deleted)
    }

    pub(crate) async fn do_delete_archived(&self, selection: SessionSelection) -> ApiResult<u32> {
        self.check_open()?;
        let mut actors = self.actors.lock().await;
        let ids = self.archived_selection(selection).await?;
        Self::retire(&mut actors, &ids).await?;
        let deleted = self.delete_rows(ids).await?;
        drop(actors);
        if deleted > 0 {
            self.ctx.events.emit(None, EventKind::SessionsChanged);
        }
        Ok(deleted)
    }

    pub(crate) async fn delete_session_now(&self, id: SessionId) -> ApiResult<()> {
        let mut actors = self.actors.lock().await;
        let ids = vec![id];
        Self::retire(&mut actors, &ids).await?;
        let deleted = self.delete_rows(ids).await?;
        drop(actors);
        if deleted > 0 {
            self.ctx.events.emit(None, EventKind::SessionsChanged);
        }
        Ok(())
    }

    /// `send_message` to an archived session unarchives it first (§9.1.5a).
    pub(crate) async fn unarchive_for_send(&self, id: &SessionId) -> ApiResult<()> {
        let ids = vec![id.clone()];
        let changed = self
            .ctx
            .store
            .run(move |c| store::set_archived(c, &ids, None))
            .await?;
        if changed > 0 {
            self.ctx.events.emit(None, EventKind::SessionsChanged);
        }
        Ok(())
    }

    /// Store rows left for a session (gate support: verifies deletes).
    ///
    /// # Errors
    ///
    /// Store failures.
    pub async fn stored_rows(&self, id: SessionId) -> ApiResult<i64> {
        Ok(self
            .ctx
            .store
            .run(move |c| store::rows_of_session(c, &id))
            .await?)
    }
}
