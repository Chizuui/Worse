//! Settings worker (spec phase-9-parity.md §6.3): runs on the `harness-core` tokio runtime, owns the
//! `AgentConfigService` and the write plan awaiting confirmation, and talks to the shared `SettingsStore`.
//! Requests are served one at a time; results go back to the UI thread through `invoke_from_event_loop`.
//!
//! The UI never writes a file itself: every agent-config / document write is planned here, shown as a diff, and
//! applied only after `Req::Confirm` (with the plan's conflict guard).

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::PathBuf;
use std::sync::Arc;

use harness_agent_config::{
    launch, skill_catalog, AgentConfigService, AgentKind, AuthState, Concept, ConceptSupport,
    ConfigError, ConfigPaths, ConfigTarget, EntryBody, FileExistenceProbe, McpImport, Scope,
    WritePlan,
};
use harness_core_api as api;
use harness_settings::{relocation, Settings, SettingsStore};
use serde_json::Value;
use tokio::sync::{mpsc, oneshot};

use super::{values, State};
use crate::core::{Command, Reply, SettingsCall};

/// A write the user must confirm.
pub enum PlanOp {
    Upsert {
        target: ConfigTarget,
        body: Box<EntryBody>,
        previous: Option<String>,
    },
    Delete {
        concept: Concept,
        target: ConfigTarget,
        name: String,
    },
    Toggle {
        concept: Concept,
        target: ConfigTarget,
        name: String,
        on: bool,
    },
    Copy {
        concept: Concept,
        from: ConfigTarget,
        to: ConfigTarget,
        name: String,
    },
    Raw {
        path: PathBuf,
        text: String,
    },
    Instructions {
        target: ConfigTarget,
        path: PathBuf,
        text: String,
    },
    Memory {
        scope: Scope,
        text: String,
    },
    ImportMcp(Vec<AgentKind>),
    /// Installs a catalog skill at user scope for every agent that supports it (spec skill-marketplace.md §2).
    InstallCatalog {
        name: String,
        skill: Box<harness_agent_config::Skill>,
        /// `(relative path, content)` of the skill's extra files.
        extra: Vec<(String, String)>,
    },
    /// Removes an installed skill from every agent that has it.
    RemoveCatalog {
        name: String,
    },
}

pub enum DocKind {
    Instructions(AgentKind),
    Memory,
}

pub enum Req {
    Patch {
        path: String,
        value: Value,
    },
    Shortcuts(BTreeMap<String, Vec<String>>),
    Relocate(PathBuf),
    CancelRelocation,
    List {
        seq: u64,
        concept: Concept,
        target: ConfigTarget,
    },
    ReadRaw {
        seq: u64,
        path: PathBuf,
    },
    Plan {
        title: &'static str,
        op: PlanOp,
    },
    Confirm,
    Cancel,
    Doc {
        seq: u64,
        kind: DocKind,
        scope: Scope,
    },
    Agents(Vec<String>),
    /// Projects page list (`removed` lists the restore candidates).
    Projects {
        seq: u64,
        query: String,
    },
    RemoveProjects {
        keys: Vec<String>,
        delete_data: bool,
    },
    RestoreProjects(Vec<String>),
    /// Archive page; `offset > 0` appends the next page.
    Archived {
        seq: u64,
        query: String,
        offset: u32,
    },
    Unarchive(String),
    DeleteArchived(String),
    /// Names of the installed skills at user scope, per agent id (skill marketplace).
    SkillsInstalled {
        seq: u64,
    },
    /// Skill trigger health report over the installed skills (spec `skill-health.md`).
    SkillsHealth {
        seq: u64,
    },
}

enum Pending {
    Plan(WritePlan),
    Import(WritePlan, String),
    Memory(api::MemoryPlan),
}

/// Archive page size (hub maximum is 200).
const ARCHIVE_PAGE: u32 = 50;

/// Agents the skill marketplace installs to (user scope only, spec skill-marketplace.md §1).
const MARKET_AGENTS: [AgentKind; 3] = [AgentKind::ClaudeCode, AgentKind::Codex, AgentKind::Harness];

/// Runs `f` on the UI thread with the settings state.
fn post(f: impl FnOnce(&mut State) + Send + 'static) {
    // 事件循环已退出（关停阶段）时丢弃结果是正确行为。
    let _ = slint::invoke_from_event_loop(move || super::with_state(f));
}

fn message(error: &ConfigError) -> String {
    error.to_string()
}

fn plan_paths(plan: &WritePlan) -> String {
    plan.changes
        .iter()
        .map(|c| c.path.display().to_string())
        .collect::<Vec<_>>()
        .join(", ")
}

/// True when `agent` accepts skills at user scope.
fn market_supports(svc: &AgentConfigService, agent: AgentKind) -> bool {
    svc.capabilities(agent)
        .into_iter()
        .any(|c: ConceptSupport| c.concept == Concept::Skill && c.user)
}

fn import_summary(import: &McpImport) -> String {
    let mut text = format!(
        "Imported {}, skipped {}",
        import.imported.len(),
        import.skipped
    );
    if !import.failed.is_empty() {
        let names = import
            .failed
            .iter()
            .map(|a| match a {
                AgentKind::ClaudeCode => "Claude Code",
                AgentKind::Codex => "Codex",
                AgentKind::Agy => "agy",
                AgentKind::Harness => "Harness",
            })
            .collect::<Vec<_>>()
            .join(", ");
        let _ = write!(text, " (could not read {names})");
    }
    text
}

pub struct Worker {
    pub store: Option<Arc<SettingsStore>>,
    /// Core command channel: MEMORY.md, projects and archive go through the hub (its single writer), never a
    /// second file writer here.
    pub core: mpsc::UnboundedSender<Command>,
    pub base: PathBuf,
    pub data_dir: PathBuf,
    pub workspace: PathBuf,
}

impl Worker {
    // One flat match over the request kinds; each arm is short.
    #[allow(clippy::too_many_lines)]
    pub async fn serve(self, mut rx: mpsc::UnboundedReceiver<Req>) {
        let paths = match ConfigPaths::from_env(&self.data_dir, Some(self.workspace.clone())) {
            Ok(paths) => paths,
            Err(_) => {
                ConfigPaths::for_home(&self.data_dir, &self.data_dir, Some(self.workspace.clone()))
            }
        };
        let svc = AgentConfigService::new(paths.clone(), Arc::new(FileExistenceProbe));
        let mut pending: Option<Pending> = None;
        let mut watch = self.store.as_ref().map(|s| s.subscribe());
        if let Some(target) = relocation::pending(&self.base).await {
            let target = target.display().to_string();
            post(move |s| s.set_pending_dir(&target));
        }
        loop {
            let req = if let Some(rx_watch) = watch.as_mut() {
                tokio::select! {
                    req = rx.recv() => req,
                    changed = rx_watch.changed() => {
                        if changed.is_err() {
                            watch = None;
                            continue;
                        }
                        let snapshot = rx_watch.borrow_and_update().clone();
                        post(move |s| s.apply_snapshot(snapshot));
                        continue;
                    }
                }
            } else {
                rx.recv().await
            };
            let Some(req) = req else {
                return;
            };
            match req {
                Req::Patch { path, value } => self.patch(&path, value).await,
                Req::Shortcuts(map) => {
                    self.save(move |s| s.shortcuts = map, "saveFailed").await;
                }
                Req::Relocate(target) => {
                    let result = relocation::schedule(&self.base, &target).await;
                    let shown = target.display().to_string();
                    post(move |s| match result {
                        Ok(()) => s.set_pending_dir(&shown),
                        Err(error) => s.status(&error.to_string(), true),
                    });
                }
                Req::CancelRelocation => {
                    let result = relocation::cancel_pending(&self.base).await;
                    post(move |s| match result {
                        Ok(()) => s.set_pending_dir(""),
                        Err(error) => s.status(&error.to_string(), true),
                    });
                }
                Req::List {
                    seq,
                    concept,
                    target,
                } => {
                    let support = svc
                        .capabilities(target.agent)
                        .into_iter()
                        .find(|c| c.concept == concept);
                    let result = svc.list(concept, target).await.map_err(|e| message(&e));
                    post(move |s| s.config_listed(seq, support, result));
                }
                Req::SkillsInstalled { seq } => {
                    let mut installed = Vec::new();
                    for agent in MARKET_AGENTS {
                        let target = ConfigTarget::new(agent, Scope::User);
                        let names = svc.list(Concept::Skill, target).await.map_or_else(
                            |_| Vec::new(),
                            |entries| entries.into_iter().map(|e| e.name).collect(),
                        );
                        installed.push((agent.agent_id().to_owned(), names));
                    }
                    crate::skills::installed_loaded(seq, installed);
                }
                Req::SkillsHealth { seq } => {
                    let report = harness_agent_config::skill_health::report_for_scope(
                        &paths,
                        &MARKET_AGENTS,
                        Scope::User,
                    )
                    .await
                    .map_err(|e| e.to_string());
                    // 实时检查默认关闭（user 的配额）：只有设置里显式打开时才生成可复制的命令，
                    // Harness 自身从不启动 skill-reach（spec skill-health.md §Live check）。
                    let live = self.store.as_ref().and_then(|store| {
                        let flag = store.snapshot().skill_health.clone();
                        MARKET_AGENTS
                            .iter()
                            .find_map(|agent| {
                                let dir = harness_agent_config::skill_health::skills_dir(
                                    &paths,
                                    *agent,
                                    Scope::User,
                                )?;
                                harness_agent_config::skill_health::LiveCheck::build(
                                    flag.live_check,
                                    &flag.binary,
                                    *agent,
                                    &dir,
                                    None,
                                )
                            })
                            .map(|command| command.to_command_line())
                    });
                    crate::skills::health_loaded(seq, report, live);
                }
                Req::ReadRaw { seq, path } => {
                    // 凭据文件（auth.json、.credentials.json 等）一律不读，只读 list 返回的配置文件。
                    let text = if harness_settings::fsx::is_forbidden(&path) {
                        Err("refused: credentials file".to_string())
                    } else {
                        harness_settings::fsx::read_to_string_opt(&path)
                            .await
                            .map(Option::unwrap_or_default)
                            .map_err(|e| e.to_string())
                    };
                    post(move |s| s.raw_loaded(seq, text));
                }
                Req::Plan {
                    title,
                    op: PlanOp::ImportMcp(sources),
                } => {
                    pending = None;
                    match svc.plan_import_mcp(&sources).await {
                        Ok(import) => {
                            let summary = import_summary(&import);
                            match import.plan {
                                Some(plan) => {
                                    let path = plan_paths(&plan);
                                    let diff = plan.diff();
                                    let no_op = plan.is_no_op();
                                    pending = Some(Pending::Import(plan, summary));
                                    post(move |s| s.show_diff(title, &path, &diff, no_op));
                                }
                                None => post(move |s| s.status(&summary, false)),
                            }
                        }
                        Err(error) => {
                            let error = message(&error);
                            post(move |s| s.plan_failed(&error));
                        }
                    }
                }
                Req::Plan { title, op } => {
                    let result = self.plan(&svc, op).await;
                    pending = None;
                    match result {
                        Ok((next, diff, path, no_op)) => {
                            pending = Some(next);
                            post(move |s| s.show_diff(title, &path, &diff, no_op));
                        }
                        Err(error) => post(move |s| s.plan_failed(&error)),
                    }
                }
                Req::Cancel => pending = None,
                Req::Confirm => {
                    let mut toast = None;
                    let result = match pending.take() {
                        Some(Pending::Plan(plan)) => {
                            svc.apply(&plan).await.map(|_| ()).map_err(|e| message(&e))
                        }
                        Some(Pending::Import(plan, summary)) => {
                            let result =
                                svc.apply(&plan).await.map(|_| ()).map_err(|e| message(&e));
                            if result.is_ok() {
                                toast = Some(summary);
                            }
                            result
                        }
                        // 记忆文件经 hub 写入（带基线指纹的冲突检查），不再由界面侧直接写盘。
                        Some(Pending::Memory(plan)) => {
                            self.call(|reply| SettingsCall::MemoryApply(plan, reply))
                                .await
                        }
                        None => Ok(()),
                    };
                    post(move |s| {
                        s.applied(result);
                        if let Some(text) = toast {
                            s.status(&text, false);
                        }
                    });
                }
                Req::Doc { seq, kind, scope } => {
                    let loaded = self.load_doc(&svc, kind, scope).await;
                    post(move |s| s.doc_loaded(seq, loaded));
                }
                Req::Agents(ids) => {
                    let mut auth = Vec::new();
                    for id in &ids {
                        if let Some(kind) = AgentKind::from_agent_id(id) {
                            let state = match svc.auth_state(kind).await {
                                AuthState::Present => 0,
                                AuthState::Absent => 1,
                                AuthState::Unknown => 2,
                            };
                            auth.push((id.clone(), state));
                        }
                    }
                    let launch = launch::list(&self.data_dir)
                        .await
                        .unwrap_or_default()
                        .into_iter()
                        .map(|o| {
                            // 环境变量只显示名字，值可能是密钥。
                            let env = o.env.keys().map(|k| format!("{k}=…")).collect::<Vec<_>>();
                            (o.id, o.program, o.args.join(" "), env.join(" "))
                        })
                        .collect::<Vec<_>>();
                    post(move |s| s.agents_loaded(auth, launch));
                }
                Req::Projects { seq, query } => {
                    let query = Some(query).filter(|q| !q.trim().is_empty());
                    let request = api::ListProjects {
                        query,
                        removed: false,
                    };
                    let result = self
                        .call(|reply| SettingsCall::ListProjects(request, reply))
                        .await;
                    post(move |s| s.projects_listed(seq, result));
                }
                Req::RemoveProjects { keys, delete_data } => {
                    let removed = keys.clone();
                    let result = self
                        .call(|reply| SettingsCall::RemoveProjects {
                            keys,
                            delete_data,
                            reply,
                        })
                        .await;
                    post(move |s| s.projects_removed(removed, delete_data, result));
                }
                Req::RestoreProjects(keys) => {
                    let result = self
                        .call(|reply| SettingsCall::RestoreProjects(keys, reply))
                        .await;
                    post(move |s| s.projects_restored(result));
                }
                Req::Archived { seq, query, offset } => {
                    let request = api::ListArchived {
                        query: Some(query).filter(|q| !q.trim().is_empty()),
                        workspace_key: None,
                        sort: api::ArchiveSort::ArchivedNewest,
                        offset,
                        limit: Some(ARCHIVE_PAGE),
                    };
                    let result = self
                        .call(|reply| SettingsCall::ListArchived(request, reply))
                        .await;
                    post(move |s| s.archive_listed(seq, offset, result));
                }
                Req::Unarchive(id) => {
                    let selection = api::SessionSelection::Sessions(vec![api::SessionId::new(id)]);
                    let result = self
                        .call(|reply| SettingsCall::Unarchive(selection, reply))
                        .await
                        .map(|_| ());
                    post(move |s| s.archive_changed(result));
                }
                Req::DeleteArchived(id) => {
                    let selection = api::SessionSelection::Sessions(vec![api::SessionId::new(id)]);
                    let result = self
                        .call(|reply| SettingsCall::DeleteArchived(selection, reply))
                        .await
                        .map(|_| ());
                    post(move |s| s.archive_changed(result));
                }
            }
        }
    }

    /// Sends one settings call to the core and waits for its reply.
    async fn call<T>(&self, make: impl FnOnce(Reply<T>) -> SettingsCall) -> Result<T, String> {
        let (tx, rx) = oneshot::channel();
        if self.core.send(Command::Settings(make(tx))).is_err() {
            return Err(api::ApiError::ShutDown.to_string());
        }
        match rx.await {
            Ok(result) => result.map_err(|e| e.to_string()),
            Err(_) => Err(api::ApiError::ShutDown.to_string()),
        }
    }

    fn memory_scope(&self, scope: Scope) -> api::MemoryScope {
        match scope {
            Scope::User => api::MemoryScope::Global,
            Scope::Workspace => api::MemoryScope::Project {
                workspace_path: self.workspace.display().to_string(),
            },
        }
    }

    async fn save(&self, edit: impl FnOnce(&mut Settings), error_code: &'static str) {
        let Some(store) = &self.store else {
            post(|s| s.status("settings store unavailable", true));
            return;
        };
        if let Err(error) = store.patch(edit).await {
            let text = format!("{error_code}: {error}");
            post(move |s| s.status(&text, true));
        }
    }

    async fn patch(&self, path: &str, value: Value) {
        let path_owned = path.to_string();
        let mut failure = None;
        self.save(
            |s| {
                if let Err(error) = values::set_path(s, &path_owned, value) {
                    failure = Some(error);
                }
            },
            "settings not saved",
        )
        .await;
        if let Some(error) = failure {
            post(move |s| s.status(&error, true));
        }
    }

    async fn plan(
        &self,
        svc: &AgentConfigService,
        op: PlanOp,
    ) -> Result<(Pending, String, String, bool), String> {
        let plan = match op {
            PlanOp::Upsert {
                target,
                body,
                previous,
            } => svc.plan_upsert(target, &body, previous.as_deref()).await,
            PlanOp::Delete {
                concept,
                target,
                name,
            } => svc.plan_delete(concept, target, &name).await,
            PlanOp::Toggle {
                concept,
                target,
                name,
                on,
            } => svc.plan_set_enabled(concept, target, &name, on).await,
            PlanOp::Copy {
                concept,
                from,
                to,
                name,
            } => svc.plan_copy(concept, from, to, &name).await,
            PlanOp::Raw { path, text } => svc.plan_raw(&path, &text).await,
            PlanOp::Instructions { target, path, text } => {
                svc.plan_instructions(target, &path, &text).await
            }
            PlanOp::Memory { scope, text } => {
                let scope = self.memory_scope(scope);
                let plan = self
                    .call(|reply| SettingsCall::MemoryPlan(scope, text, reply))
                    .await?;
                let (diff, shown, no_op) = (plan.diff.clone(), plan.path.clone(), plan.no_op);
                return Ok((Pending::Memory(plan), diff, shown, no_op));
            }
            PlanOp::ImportMcp(_) => {
                return Err("import is planned separately".to_owned());
            }
            PlanOp::InstallCatalog { name, skill, extra } => {
                let mut merged = WritePlan::default();
                let mut planned = 0_u32;
                for agent in MARKET_AGENTS {
                    if !market_supports(svc, agent) {
                        continue;
                    }
                    let target = ConfigTarget::new(agent, Scope::User);
                    let body = EntryBody::Skill((*skill).clone());
                    let plan = svc
                        .plan_upsert(target, &body, None)
                        .await
                        .map_err(|e| message(&e))?;
                    let plan = skill_catalog::with_extra_files(plan, &extra)
                        .await
                        .map_err(|e| message(&e))?;
                    planned += 1;
                    merged.changes.extend(plan.changes);
                }
                if planned == 0 {
                    return Err(format!("no agent accepts skills ({name})"));
                }
                let path = plan_paths(&merged);
                let diff = merged.diff();
                let no_op = merged.is_no_op();
                return Ok((Pending::Plan(merged), diff, path, no_op));
            }
            PlanOp::RemoveCatalog { name } => {
                let mut merged = WritePlan::default();
                for agent in MARKET_AGENTS {
                    if !market_supports(svc, agent) {
                        continue;
                    }
                    let target = ConfigTarget::new(agent, Scope::User);
                    match svc.plan_delete(Concept::Skill, target, &name).await {
                        Ok(plan) => merged.changes.extend(plan.changes),
                        Err(ConfigError::NotFound(_)) => {}
                        Err(error) => return Err(message(&error)),
                    }
                }
                if merged.changes.is_empty() {
                    return Err(format!("{name} is not installed"));
                }
                let path = plan_paths(&merged);
                let diff = merged.diff();
                let no_op = merged.is_no_op();
                return Ok((Pending::Plan(merged), diff, path, no_op));
            }
        }
        .map_err(|e| message(&e))?;
        let path = plan_paths(&plan);
        let diff = plan.diff();
        let no_op = plan.is_no_op();
        Ok((Pending::Plan(plan), diff, path, no_op))
    }

    async fn load_doc(
        &self,
        svc: &AgentConfigService,
        kind: DocKind,
        scope: Scope,
    ) -> Result<super::Doc, String> {
        match kind {
            DocKind::Memory => {
                let scope = self.memory_scope(scope);
                let doc = self
                    .call(|reply| SettingsCall::MemoryDoc(scope, reply))
                    .await?;
                Ok(super::Doc {
                    path: PathBuf::from(doc.path),
                    exists: doc.exists,
                    text: doc.content,
                    supported: true,
                    reason: String::new(),
                })
            }
            DocKind::Instructions(agent) => {
                let target = ConfigTarget::new(agent, scope);
                let support = svc
                    .capabilities(agent)
                    .into_iter()
                    .find(|c| c.concept == Concept::Instructions);
                let scope_ok = support.as_ref().is_some_and(|c| match scope {
                    Scope::User => c.user,
                    Scope::Workspace => c.workspace,
                });
                if !scope_ok {
                    return Ok(super::Doc {
                        path: PathBuf::new(),
                        exists: false,
                        text: String::new(),
                        supported: false,
                        reason: support.and_then(|c| c.reason).unwrap_or_default(),
                    });
                }
                let entries = svc
                    .list(Concept::Instructions, target)
                    .await
                    .map_err(|e| message(&e))?;
                // 优先已存在的候选文件，否则取第一个候选（新建）。
                let files: Vec<_> = entries
                    .into_iter()
                    .filter_map(|e| match e.body {
                        EntryBody::Instructions { file, .. } => Some(file),
                        _ => None,
                    })
                    .collect();
                let Some(file) = files
                    .iter()
                    .find(|f| f.exists)
                    .or_else(|| files.first())
                    .cloned()
                else {
                    return Ok(super::Doc {
                        path: PathBuf::new(),
                        exists: false,
                        text: String::new(),
                        supported: false,
                        reason: String::new(),
                    });
                };
                let text = svc
                    .read_instructions(target, &file.path)
                    .await
                    .map_err(|e| message(&e))?
                    .unwrap_or_default();
                Ok(super::Doc {
                    path: file.path,
                    exists: file.exists,
                    text,
                    supported: true,
                    reason: String::new(),
                })
            }
        }
    }
}
