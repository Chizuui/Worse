//! Fake answers for the Agents / Memory / Projects / Archive pages (phase-9-parity.md §9.1.5a). In-memory fixtures:
//! no CLI is run, no MEMORY.md is written; only the fake core's own session files are deleted on delete.
//! One fixture project (`FAKE_PROJECT`) holds every fake session.

use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};

use harness_core_api as api;

use super::{emit, now_ms, FakeCore};
use crate::core::{Event, SessionSummary, SettingsCall};

pub(super) const FAKE_PROJECT: &str = "fake-workspace";

#[derive(Default)]
pub(super) struct FakeAdmin {
    memory: HashMap<String, String>,
    pub(super) archived: Vec<(SessionSummary, i64)>,
    /// Sessions hidden while the fixture project is removed.
    hidden: Vec<SessionSummary>,
    removed_at: Option<i64>,
    tested: HashSet<String>,
}

fn now() -> i64 {
    i64::try_from(now_ms()).unwrap_or(i64::MAX)
}

fn scope_key(scope: &api::MemoryScope) -> String {
    match scope {
        api::MemoryScope::Global => "global".into(),
        api::MemoryScope::Project { workspace_path } => workspace_path.clone(),
    }
}

fn fingerprint(content: Option<&str>) -> String {
    content.map_or_else(
        || "absent".into(),
        |text| {
            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            text.hash(&mut hasher);
            format!("{}:{:016x}", text.len(), hasher.finish())
        },
    )
}

fn api_summary(s: &SessionSummary) -> api::SessionSummary {
    let at = i64::try_from(s.updated_ms).unwrap_or(0);
    api::SessionSummary {
        id: api::SessionId::new(s.id.clone()),
        title: s.title.clone(),
        workspace_path: FAKE_PROJECT.into(),
        workspace_key: FAKE_PROJECT.into(),
        active_agent: api::AgentId::new(s.agents_used.first().cloned().unwrap_or_default()),
        created_at: at,
        updated_at: at,
        turn_count: 0,
        parent_id: None,
        worktree: None,
        agents_used: s
            .agents_used
            .iter()
            .map(|a| api::AgentId::new(a.clone()))
            .collect(),
        pinned: false,
        group_id: None,
        order: 0,
        unread: false,
        forked_from: None,
        imported_from: None,
    }
}

impl FakeCore {
    pub(super) fn emit_overview(&self) {
        let rows = self
            .agents
            .iter()
            .map(|a| {
                let installed = a.state != crate::core::AgentState::Unavailable;
                api::AgentOverview {
                    id: api::AgentId::new(a.id.clone()),
                    display_name: a.name.clone(),
                    installed,
                    command_path: installed.then(|| format!("fake://{}", a.id)),
                    install_hint: (!installed).then(|| format!("install {}", a.name)),
                    version: self.admin.tested.contains(&a.id).then(|| "0.1.0".into()),
                    auth: match a.id.as_str() {
                        "claude-code" => api::AuthPresence::Present,
                        "codex" => api::AuthPresence::Absent,
                        _ => api::AuthPresence::Unknown,
                    },
                    login_hint: Some(format!("sign in to {}", a.name)),
                    runtime: api::AgentRuntimeState::Stopped,
                    quota: match &a.state {
                        crate::core::AgentState::Exhausted { .. } => api::QuotaState::Exhausted {
                            resets_at: None,
                            reason: "fake quota".into(),
                        },
                        _ => api::QuotaState::Ok,
                    },
                    adapter_missing: false,
                    adapter_package: None,
                }
            })
            .collect();
        emit(&self.ev, Event::AgentsOverview(rows));
    }

    pub(super) fn test_agent(&mut self, agent: &str) {
        let installed = self
            .agents
            .iter()
            .any(|a| a.id == agent && a.state != crate::core::AgentState::Unavailable);
        if installed {
            self.admin.tested.insert(agent.to_owned());
        }
        let result = api::AgentTestResult {
            agent: api::AgentId::new(agent),
            ok: installed,
            latency_ms: 5,
            agent_name: installed.then(|| agent.to_owned()),
            version: installed.then(|| "0.1.0".into()),
            error: (!installed).then(|| format!("{agent} is not installed")),
        };
        emit(&self.ev, Event::AgentTested(result));
        self.emit_overview();
    }

    fn busy(&self, ids: &[String]) -> Option<String> {
        ids.iter()
            .find(|id| {
                self.open
                    .get(*id)
                    .is_some_and(|s| s.run.is_some() || !s.queue.is_empty())
            })
            .cloned()
    }

    fn project_info(&self, removed: bool) -> Option<api::ProjectInfo> {
        if self.admin.removed_at.is_some() != removed {
            return None;
        }
        let active = if removed {
            &self.admin.hidden
        } else {
            &self.summaries
        };
        let last_used = active
            .iter()
            .map(|s| i64::try_from(s.updated_ms).unwrap_or(0))
            .max();
        Some(api::ProjectInfo {
            workspace_key: FAKE_PROJECT.into(),
            workspace_path: FAKE_PROJECT.into(),
            sessions: u32::try_from(active.len()).unwrap_or(u32::MAX),
            archived: u32::try_from(self.admin.archived.len()).unwrap_or(u32::MAX),
            last_used,
            unused: last_used.map_or(true, |t| now() - t > 30 * 24 * 60 * 60 * 1000),
            removed_at: self.admin.removed_at,
        })
    }

    fn selected(&self, selection: &api::SessionSelection) -> Vec<String> {
        match selection {
            api::SessionSelection::Sessions(ids) => ids
                .iter()
                .filter(|id| self.admin.archived.iter().any(|(s, _)| s.id == id.as_str()))
                .map(|id| id.as_str().to_owned())
                .collect(),
            api::SessionSelection::Project(key) if key == FAKE_PROJECT => self
                .admin
                .archived
                .iter()
                .map(|(s, _)| s.id.clone())
                .collect(),
            api::SessionSelection::Project(_) => Vec::new(),
        }
    }

    async fn delete_files(&mut self, ids: &[String]) {
        for id in ids {
            self.open.remove(id);
            let path = self.sessions_dir.join(format!("{id}.json"));
            if let Err(error) = tokio::fs::remove_file(&path).await {
                if error.kind() != std::io::ErrorKind::NotFound {
                    emit(&self.ev, Event::Error(format!("delete {id}: {error}")));
                }
            }
        }
    }

    fn sessions_changed(&self) {
        emit(&self.ev, Event::Sessions(self.summaries.clone()));
    }

    #[allow(clippy::too_many_lines)] // one dispatch over the call enum
    pub(super) async fn settings_call(&mut self, call: SettingsCall) {
        match call {
            SettingsCall::MemoryDoc(scope, reply) => {
                let content = self.admin.memory.get(&scope_key(&scope)).cloned();
                let _ = reply.send(Ok(api::MemoryDoc {
                    path: format!("fake://{}/MEMORY.md", scope_key(&scope)),
                    exists: content.is_some(),
                    content: content.unwrap_or_default(),
                    scope,
                }));
            }
            SettingsCall::MemoryPlan(scope, content, reply) => {
                let current = self.admin.memory.get(&scope_key(&scope)).cloned();
                let no_op = current.as_deref() == Some(content.as_str());
                let path = format!("fake://{}/MEMORY.md", scope_key(&scope));
                let diff = if no_op {
                    String::new()
                } else {
                    harness_agent_config::unified_diff(
                        &path,
                        current.as_deref().unwrap_or_default(),
                        &content,
                    )
                };
                let _ = reply.send(Ok(api::MemoryPlan {
                    base: fingerprint(current.as_deref()),
                    scope,
                    path,
                    content,
                    diff,
                    no_op,
                }));
            }
            SettingsCall::MemoryApply(plan, reply) => {
                let key = scope_key(&plan.scope);
                let result = if plan.no_op {
                    Ok(())
                } else if fingerprint(self.admin.memory.get(&key).map(String::as_str)) != plan.base
                {
                    Err(api::ApiError::Conflict(format!("{} changed", plan.path)))
                } else {
                    self.admin.memory.insert(key, plan.content);
                    Ok(())
                };
                let _ = reply.send(result);
            }
            SettingsCall::ListProjects(request, reply) => {
                let query = request.query.unwrap_or_default().trim().to_lowercase();
                let rows = self
                    .project_info(request.removed)
                    .filter(|p| query.is_empty() || p.workspace_path.contains(&query))
                    .into_iter()
                    .collect();
                let _ = reply.send(Ok(rows));
            }
            SettingsCall::RemoveProjects {
                keys,
                delete_data,
                reply,
            } => {
                if !keys.iter().any(|k| k == FAKE_PROJECT) {
                    let _ = reply.send(Ok(()));
                    return;
                }
                let ids: Vec<String> = self.summaries.iter().map(|s| s.id.clone()).collect();
                if delete_data {
                    if let Some(id) = self.busy(&ids) {
                        let _ = reply
                            .send(Err(api::ApiError::Busy(format!("session {id} is running"))));
                        return;
                    }
                    let mut all = ids;
                    all.extend(self.admin.archived.drain(..).map(|(s, _)| s.id));
                    self.delete_files(&all).await;
                    self.summaries.clear();
                } else {
                    self.admin.hidden = std::mem::take(&mut self.summaries);
                }
                self.admin.removed_at = Some(now());
                self.sessions_changed();
                let _ = reply.send(Ok(()));
            }
            SettingsCall::RestoreProjects(keys, reply) => {
                if keys.iter().any(|k| k == FAKE_PROJECT) && self.admin.removed_at.is_some() {
                    self.admin.removed_at = None;
                    self.summaries.append(&mut self.admin.hidden);
                    self.sessions_changed();
                }
                let _ = reply.send(Ok(()));
            }
            SettingsCall::ArchiveSessions(ids, reply) => {
                if let Some(id) = ids
                    .iter()
                    .find(|id| !self.summaries.iter().any(|s| &s.id == *id))
                {
                    let _ = reply.send(Err(api::ApiError::NotFound(format!("session {id}"))));
                    return;
                }
                if let Some(id) = self.busy(&ids) {
                    let _ =
                        reply.send(Err(api::ApiError::Busy(format!("session {id} is running"))));
                    return;
                }
                let at = now();
                let (archived, kept): (Vec<_>, Vec<_>) = std::mem::take(&mut self.summaries)
                    .into_iter()
                    .partition(|s| ids.contains(&s.id));
                self.summaries = kept;
                self.admin
                    .archived
                    .extend(archived.into_iter().map(|s| (s, at)));
                self.sessions_changed();
                let _ = reply.send(Ok(()));
            }
            SettingsCall::ListArchived(request, reply) => {
                let query = request.query.unwrap_or_default().trim().to_lowercase();
                let mut rows: Vec<_> = self
                    .admin
                    .archived
                    .iter()
                    .filter(|(s, _)| query.is_empty() || s.title.to_lowercase().contains(&query))
                    .filter(|_| {
                        request
                            .workspace_key
                            .as_deref()
                            .map_or(true, |k| k == FAKE_PROJECT)
                    })
                    .map(|(s, at)| api::ArchivedSession {
                        summary: api_summary(s),
                        archived_at: *at,
                    })
                    .collect();
                match request.sort {
                    api::ArchiveSort::ArchivedNewest => {
                        rows.sort_by_key(|r| std::cmp::Reverse(r.archived_at));
                    }
                    api::ArchiveSort::ArchivedOldest => rows.sort_by_key(|r| r.archived_at),
                    api::ArchiveSort::Title => {
                        rows.sort_by_key(|r| r.summary.title.to_lowercase());
                    }
                }
                let total = u32::try_from(rows.len()).unwrap_or(u32::MAX);
                let limit = request.limit.unwrap_or(50).clamp(1, 200) as usize;
                let page: Vec<_> = rows
                    .into_iter()
                    .skip(request.offset as usize)
                    .take(limit)
                    .collect();
                let shown = request.offset as usize + page.len();
                let _ = reply.send(Ok(api::ArchivePage {
                    rows: page,
                    total,
                    has_more: shown < total as usize,
                }));
            }
            SettingsCall::Unarchive(selection, reply) => {
                let ids = self.selected(&selection);
                let (back, kept): (Vec<_>, Vec<_>) = std::mem::take(&mut self.admin.archived)
                    .into_iter()
                    .partition(|(s, _)| ids.contains(&s.id));
                self.admin.archived = kept;
                let n = u32::try_from(back.len()).unwrap_or(u32::MAX);
                self.summaries.extend(back.into_iter().map(|(s, _)| s));
                self.summaries
                    .sort_by_key(|s| std::cmp::Reverse(s.updated_ms));
                if n > 0 {
                    self.sessions_changed();
                }
                let _ = reply.send(Ok(n));
            }
            SettingsCall::DeleteArchived(selection, reply) => {
                let ids = self.selected(&selection);
                self.admin.archived.retain(|(s, _)| !ids.contains(&s.id));
                self.delete_files(&ids).await;
                let _ = reply.send(Ok(u32::try_from(ids.len()).unwrap_or(u32::MAX)));
            }
        }
    }
}
