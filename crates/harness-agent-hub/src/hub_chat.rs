//! Hub side of API 0.5.0 (phase-9-parity.md §9.2.1): session organisation, groups, fork, turn actions, file
//! undo, find, context items and mentions, slash commands and the command-center search.

use std::collections::HashMap;
use std::sync::Mutex;

use harness_agent_config::{
    AgentConfigService, Concept, ConfigTarget, EntryBody, Scope as ConfigScope,
};
use harness_core_api::{
    ActionEntry, AgentId, ApiError, ApiResult, ApplyFileChanges, ChangeDirection, CommandList,
    CommandSearch, CommandSearchResult, CommandSource, ContextItem, EditResend, EventKind,
    FileChangePlan, FileRestoreReport, FindField, FindHit, FindInSession, ForkOrigin, ForkSession,
    GroupColor, MentionCandidate, MentionKind, MentionQuery, ResolvedItem, Retry, Rewind,
    RewindResult, ScoredId, SearchScope, SendAccepted, SessionGroup, SessionHit, SessionId,
    SessionSummary, SlashCommand, TurnId, UpdateGroup,
};

use harness_core_api::workspace::FileHit;

use crate::context::{self, Caps};
use crate::hub::{Hub, HubCtx};
use crate::session::{Resend, RewindCmd, SessionCmd};
use crate::store::{self, SessionRow};
use crate::store_chat;
use crate::util::{new_id, now_ms};

const COMMANDS_PREFIX: &str = "agent_commands:";

pub(crate) struct HarnessSkill {
    pub(crate) name: String,
    pub(crate) description: String,
    pub(crate) body: String,
    pub(crate) path: std::path::PathBuf,
}

pub(crate) async fn harness_skills(ctx: &HubCtx) -> Vec<HarnessSkill> {
    let Some(paths) = &ctx.config.config_paths else {
        return Vec::new();
    };
    let service = AgentConfigService::new(paths.clone(), ctx.config.auth_probe.0.clone());
    let target = ConfigTarget {
        agent: harness_agent_config::AgentKind::Harness,
        scope: ConfigScope::User,
    };
    let Ok(entries) = service.list(Concept::Skill, target).await else {
        return Vec::new();
    };
    entries
        .into_iter()
        .filter_map(|entry| match entry.body {
            EntryBody::Skill(skill) => Some(HarnessSkill {
                name: skill.name,
                description: skill.description.unwrap_or_default(),
                body: skill.body,
                path: entry.source_path,
            }),
            _ => None,
        })
        .collect()
}

pub(crate) fn skill_prompt(skill: &HarnessSkill, input: &str) -> String {
    let dir = skill
        .path
        .parent()
        .map_or_else(String::new, |d| d.display().to_string());
    let input = match input.trim() {
        "" => "(no extra input)",
        text => text,
    };
    format!(
        "<harness-skill name=\"{name}\" dir=\"{dir}\">\n{body}\n</harness-skill>\nThe skill's other files are in {dir}; read them when the skill refers to them.\n\nFollow the Harness skill \"{name}\" above for this request: {input}",
        name = skill.name,
        body = skill.body.trim(),
    )
}

pub(crate) fn skill_call(text: &str) -> Option<(&str, &str)> {
    let rest = text.trim_start().strip_prefix('/')?;
    let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
    let (name, input) = rest.split_at(end);
    (!name.is_empty()).then_some((name, input))
}

/// Lazy chat caches shared by the hub and its actors.
#[derive(Default)]
pub(crate) struct ChatState {
    /// `promptCapabilities` per agent, learned when a native session is ready.
    prompt_caps: Mutex<HashMap<AgentId, Caps>>,
    /// Last `available_commands_update` per agent (cache of store keys `agent_commands:<id>`).
    commands: Mutex<HashMap<AgentId, Option<Vec<SlashCommand>>>>,
}

impl ChatState {
    pub(crate) fn set_caps(&self, agent: &AgentId, caps: Caps) {
        if let Ok(mut map) = self.prompt_caps.lock() {
            map.insert(agent.clone(), caps);
        }
    }

    pub(crate) fn caps(&self, agent: &AgentId) -> Caps {
        self.prompt_caps
            .lock()
            .ok()
            .and_then(|m| m.get(agent).copied())
            .unwrap_or(Caps::UNKNOWN)
    }

    /// The agent's advertised commands; `None` when it never advertised any.
    pub(crate) async fn agent_commands(
        &self,
        ctx: &HubCtx,
        agent: &AgentId,
    ) -> Option<Vec<SlashCommand>> {
        if let Some(cached) = self
            .commands
            .lock()
            .ok()
            .and_then(|m| m.get(agent).cloned())
        {
            return cached;
        }
        let key = format!("{COMMANDS_PREFIX}{agent}");
        let stored = ctx
            .store
            .run(move |c| store::get_setting(c, &key))
            .await
            .ok()
            .flatten()
            .and_then(|json| serde_json::from_str::<Vec<SlashCommand>>(&json).ok());
        if let Ok(mut map) = self.commands.lock() {
            map.insert(agent.clone(), stored.clone());
        }
        stored
    }

    /// Single write path of the command cache: memory, store, global `CommandsChanged`.
    pub(crate) async fn record_commands(
        &self,
        ctx: &HubCtx,
        agent: &AgentId,
        commands: Vec<SlashCommand>,
    ) {
        if self.agent_commands(ctx, agent).await.as_ref() == Some(&commands) {
            return;
        }
        if let Ok(mut map) = self.commands.lock() {
            map.insert(agent.clone(), Some(commands.clone()));
        }
        let key = format!("{COMMANDS_PREFIX}{agent}");
        match serde_json::to_string(&commands) {
            Ok(json) => {
                if let Err(error) = ctx
                    .store
                    .run(move |c| store::set_setting(c, &key, &json))
                    .await
                {
                    tracing::warn!(%error, "saving agent commands failed");
                }
            }
            Err(error) => tracing::warn!(%error, "encoding agent commands failed"),
        }
        ctx.events.emit(
            None,
            EventKind::CommandsChanged {
                agent: agent.clone(),
            },
        );
    }
}

/// Case-insensitive rank: exact > prefix > word prefix > substring > subsequence.
pub(crate) fn score(query: &str, text: &str) -> Option<u32> {
    let query = query.trim().to_lowercase();
    let text = text.to_lowercase();
    if query.is_empty() {
        return Some(0);
    }
    if text == query {
        return Some(1000);
    }
    if text.starts_with(&query) {
        return Some(800);
    }
    if text
        .split(|c: char| !c.is_alphanumeric())
        .any(|w| w.starts_with(&query))
    {
        return Some(600);
    }
    if text.contains(&query) {
        return Some(400);
    }
    let mut chars = text.chars();
    query.chars().all(|q| chars.any(|t| t == q)).then_some(200)
}

/// Lower-cases char by char, keeping one char per char (offsets stay valid).
fn fold(text: &str, case_sensitive: bool) -> Vec<char> {
    text.chars()
        .map(|c| {
            if case_sensitive {
                c
            } else {
                c.to_lowercase().next().unwrap_or(c)
            }
        })
        .collect()
}

fn find_all(text: &str, query: &[char], case_sensitive: bool) -> Vec<(usize, String)> {
    let hay = fold(text, case_sensitive);
    let original: Vec<char> = text.chars().collect();
    let mut out = Vec::new();
    if query.is_empty() || hay.len() < query.len() {
        return out;
    }
    let mut start = 0;
    while start + query.len() <= hay.len() {
        if hay[start..start + query.len()] == *query {
            let from = start.saturating_sub(15);
            let to = (start + query.len() + 25).min(original.len());
            out.push((start, original[from..to].iter().collect()));
            start += query.len();
        } else {
            start += 1;
        }
    }
    out
}

fn as_u32(n: usize) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

impl Hub {
    pub(crate) async fn load_row(&self, id: &SessionId) -> ApiResult<SessionRow> {
        let lookup = id.clone();
        self.ctx
            .store
            .run(move |c| store::load_session(c, &lookup))
            .await?
            .ok_or_else(|| ApiError::NotFound(format!("session {id}")))
    }

    /// Emits `SessionUpdated` with fresh summaries.
    pub(crate) async fn emit_summaries(&self, ids: Vec<SessionId>) -> ApiResult<()> {
        let summaries = self
            .ctx
            .store
            .run(move |c| {
                let mut out = Vec::new();
                for id in &ids {
                    if let Some(row) = store::load_session(c, id)? {
                        out.push(store::summary(c, &row)?);
                    }
                }
                Ok(out)
            })
            .await?;
        for summary in summaries {
            self.ctx.events.emit(
                Some(&summary.id.clone()),
                EventKind::SessionUpdated { summary },
            );
        }
        Ok(())
    }

    // ---- organisation -------------------------------------------------------------------------------

    pub(crate) async fn do_rename(&self, id: SessionId, title: String) -> ApiResult<()> {
        self.ask(&id, |reply| SessionCmd::Rename(title, reply))
            .await
    }

    pub(crate) async fn do_set_pinned(&self, ids: Vec<SessionId>, pinned: bool) -> ApiResult<()> {
        self.check_open()?;
        let at = pinned.then(now_ms);
        let changed = self
            .ctx
            .store
            .run(move |c| store_chat::set_pinned(c, &ids, at))
            .await?;
        self.emit_summaries(changed).await
    }

    pub(crate) async fn do_set_unread(&self, id: SessionId, unread: bool) -> ApiResult<()> {
        self.check_open()?;
        let lookup = id.clone();
        let found = self
            .ctx
            .store
            .run(move |c| {
                if unread {
                    store_chat::mark_unread(c, &lookup)
                } else {
                    store_chat::mark_read(c, &lookup, now_ms())
                }
            })
            .await?;
        if !found {
            return Err(ApiError::NotFound(format!("session {id}")));
        }
        self.emit_summaries(vec![id]).await
    }

    pub(crate) async fn do_list_groups(&self) -> ApiResult<Vec<SessionGroup>> {
        self.check_open()?;
        Ok(self.ctx.store.run(store_chat::list_groups).await?)
    }

    fn group_name(name: &str) -> ApiResult<String> {
        let name = name.trim();
        let count = name.chars().count();
        if count == 0 || count > 60 {
            return Err(ApiError::InvalidArgument(
                "a group name has 1 to 60 characters".into(),
            ));
        }
        Ok(name.to_owned())
    }

    pub(crate) async fn do_create_group(
        &self,
        name: String,
        color: GroupColor,
    ) -> ApiResult<SessionGroup> {
        self.check_open()?;
        let name = Self::group_name(&name)?;
        let group = self
            .ctx
            .store
            .run(move |c| {
                let position = store_chat::list_groups(c)?
                    .iter()
                    .map(|g| g.position + 1)
                    .max()
                    .unwrap_or(0);
                let group = SessionGroup {
                    id: new_id("grp"),
                    name,
                    color,
                    position,
                };
                store_chat::insert_group(c, &group)?;
                Ok(group)
            })
            .await?;
        self.ctx.events.emit(None, EventKind::GroupsChanged);
        Ok(group)
    }

    pub(crate) async fn do_update_group(&self, request: UpdateGroup) -> ApiResult<SessionGroup> {
        self.check_open()?;
        let name = request.name.as_deref().map(Self::group_name).transpose()?;
        let group = self
            .ctx
            .store
            .run(move |c| {
                let Some(mut group) = store_chat::list_groups(c)?
                    .into_iter()
                    .find(|g| g.id == request.id)
                else {
                    return Err(crate::HubError::NotFound(format!("group {}", request.id)));
                };
                if let Some(name) = name {
                    group.name = name;
                }
                if let Some(color) = request.color {
                    group.color = color;
                }
                store_chat::update_group(c, &group)?;
                Ok(group)
            })
            .await?;
        self.ctx.events.emit(None, EventKind::GroupsChanged);
        Ok(group)
    }

    pub(crate) async fn do_delete_group(&self, id: String) -> ApiResult<()> {
        self.check_open()?;
        let members = self
            .ctx
            .store
            .run(move |c| {
                if !store_chat::list_groups(c)?.iter().any(|g| g.id == id) {
                    return Err(crate::HubError::NotFound(format!("group {id}")));
                }
                store_chat::delete_group(c, &id)
            })
            .await?;
        self.ctx.events.emit(None, EventKind::GroupsChanged);
        self.emit_summaries(members).await
    }

    pub(crate) async fn do_reorder_groups(&self, ids: Vec<String>) -> ApiResult<()> {
        self.check_open()?;
        self.ctx
            .store
            .run(move |c| {
                let mut current: Vec<String> = store_chat::list_groups(c)?
                    .into_iter()
                    .map(|g| g.id)
                    .collect();
                let mut given = ids.clone();
                current.sort();
                given.sort();
                if current != given {
                    return Err(crate::HubError::Invalid(
                        "the order must list every group exactly once".into(),
                    ));
                }
                store_chat::set_group_positions(c, &ids)
            })
            .await?;
        self.ctx.events.emit(None, EventKind::GroupsChanged);
        Ok(())
    }

    pub(crate) async fn do_move_sessions(
        &self,
        ids: Vec<SessionId>,
        group: Option<String>,
    ) -> ApiResult<()> {
        self.check_open()?;
        let moved = ids.clone();
        self.ctx
            .store
            .run(move |c| {
                if let Some(group) = &group {
                    if !store_chat::list_groups(c)?.iter().any(|g| &g.id == group) {
                        return Err(crate::HubError::NotFound(format!("group {group}")));
                    }
                }
                for id in &ids {
                    if !store_chat::session_exists(c, id)? {
                        return Err(crate::HubError::NotFound(format!("session {id}")));
                    }
                }
                store_chat::move_sessions(c, &ids, group.as_deref(), now_ms())
            })
            .await?;
        self.emit_summaries(moved).await
    }

    // ---- fork and turn actions ----------------------------------------------------------------------

    pub(crate) async fn do_fork(&self, request: ForkSession) -> ApiResult<SessionSummary> {
        self.check_open()?;
        let source = self.load_row(&request.session_id).await?;
        let up_to = request.up_to_turn.clone();
        let source_id = source.id.clone();
        let now = now_ms();
        let row = SessionRow {
            id: SessionId::new(new_id("ses")),
            title: request
                .title
                .map(|t| t.trim().to_owned())
                .filter(|t| !t.is_empty())
                .unwrap_or_else(|| format!("{} (fork)", source.title)),
            workspace_path: source.workspace_path.clone(),
            workspace_key: source.workspace_key.clone(),
            active_agent: source.active_agent.clone(),
            created_at: now,
            updated_at: now,
        };
        let summary = self
            .ctx
            .store
            .run(move |c| {
                let turns = store::load_turns(c, &source_id)?;
                let finished = |t: &harness_core_api::TurnView| {
                    !matches!(
                        t.status,
                        harness_core_api::TurnStatus::Queued
                            | harness_core_api::TurnStatus::Running
                            | harness_core_api::TurnStatus::Paused
                    )
                };
                let seq = match &up_to {
                    Some(turn) => {
                        turns
                            .iter()
                            .find(|(t, _)| &t.id == turn)
                            .ok_or_else(|| crate::HubError::NotFound(format!("turn {turn}")))?
                            .0
                            .seq
                    }
                    None => turns
                        .iter()
                        .filter(|(t, _)| finished(t))
                        .map(|(t, _)| t.seq)
                        .max()
                        .unwrap_or(0),
                };
                store::insert_session(c, &row)?;
                store_chat::mark_read(c, &row.id, now)?;
                store_chat::copy_turns(c, &source_id, &row.id, seq, &mut || new_id("turn"))?;
                store_chat::set_fork_origin(
                    c,
                    &row.id,
                    &ForkOrigin {
                        session_id: source_id.clone(),
                        turn_seq: seq,
                    },
                )?;
                store::summary(c, &row)
            })
            .await?;
        self.ctx.events.emit(
            Some(&summary.id),
            EventKind::SessionCreated {
                summary: summary.clone(),
            },
        );
        Ok(summary)
    }

    pub(crate) async fn do_rewind(&self, request: Rewind) -> ApiResult<RewindResult> {
        let command = RewindCmd {
            turn: request.turn_id,
            restore_files: request.restore_files,
            resend: Resend::None,
        };
        let outcome = self
            .ask(&request.session_id, |reply| {
                SessionCmd::Rewind(command, reply)
            })
            .await?;
        Ok(outcome.result)
    }

    async fn rewind_and_send(
        &self,
        session: &SessionId,
        command: RewindCmd,
    ) -> ApiResult<SendAccepted> {
        self.unarchive_for_send(session).await?;
        let outcome = self
            .ask(session, |reply| SessionCmd::Rewind(command, reply))
            .await?;
        outcome
            .accepted
            .ok_or_else(|| ApiError::Internal("rewind did not resend".into()))
    }

    pub(crate) async fn do_edit_and_resend(&self, request: EditResend) -> ApiResult<SendAccepted> {
        let command = RewindCmd {
            turn: request.turn_id,
            restore_files: request.restore_files,
            resend: Resend::Edit {
                text: request.text,
                context: request.context,
                agent: request.agent,
            },
        };
        self.rewind_and_send(&request.session_id, command).await
    }

    pub(crate) async fn do_retry(&self, request: Retry) -> ApiResult<SendAccepted> {
        let command = RewindCmd {
            turn: request.turn_id,
            restore_files: request.restore_files,
            resend: Resend::Retry {
                agent: request.agent,
            },
        };
        self.rewind_and_send(&request.session_id, command).await
    }

    pub(crate) async fn do_plan_files(
        &self,
        session: SessionId,
        turn: TurnId,
        direction: ChangeDirection,
    ) -> ApiResult<FileChangePlan> {
        self.check_open()?;
        let row = self.load_row(&session).await?;
        let lookup = turn.clone();
        let owner = self
            .ctx
            .store
            .run(move |c| store::turn_session(c, &lookup))
            .await?;
        if owner.as_ref() != Some(&session) {
            return Err(ApiError::NotFound(format!(
                "turn {turn} in session {session}"
            )));
        }
        let workspace = std::path::PathBuf::from(&row.workspace_path);
        crate::files::plan_turn(&self.ctx, &workspace, &turn, direction).await
    }

    pub(crate) async fn do_apply_files(
        &self,
        request: ApplyFileChanges,
    ) -> ApiResult<FileRestoreReport> {
        let session = request.session_id.clone();
        self.ask(&session, |reply| SessionCmd::ApplyFiles(request, reply))
            .await
    }

    // ---- find, context, mentions ---------------------------------------------------------------------

    pub(crate) async fn do_find(&self, request: FindInSession) -> ApiResult<Vec<FindHit>> {
        let detail = self.ask(&request.session_id, SessionCmd::Snapshot).await?;
        let limit = match request.limit {
            0 => 200,
            n => n.min(500),
        } as usize;
        let query = fold(&request.query, request.case_sensitive);
        let mut hits = Vec::new();
        let length = as_u32(query.len());
        'turns: for turn in &detail.turns {
            let mut fields: Vec<(FindField, Option<String>, &str)> = vec![
                (FindField::User, None, turn.user_text.as_str()),
                (FindField::Assistant, None, turn.assistant_text.as_str()),
            ];
            for tool in &turn.tool_calls {
                fields.push((
                    FindField::Tool,
                    Some(tool.tool_call_id.clone()),
                    tool.title.as_str(),
                ));
            }
            for (field, tool, text) in fields {
                for (start, snippet) in find_all(text, &query, request.case_sensitive) {
                    hits.push(FindHit {
                        turn_id: turn.id.clone(),
                        seq: turn.seq,
                        field,
                        tool_call_id: tool.clone(),
                        start: as_u32(start),
                        len: length,
                        snippet,
                    });
                    if hits.len() >= limit {
                        break 'turns;
                    }
                }
            }
        }
        Ok(hits)
    }

    pub(crate) async fn do_resolve_context(
        &self,
        session: SessionId,
        items: Vec<ContextItem>,
    ) -> ApiResult<Vec<ResolvedItem>> {
        self.check_open()?;
        let row = self.load_row(&session).await?;
        let caps = self.ctx.chat.caps(&row.active_agent);
        Ok(context::check(&self.ctx, &row.workspace_path, &session, &items, caps).await)
    }

    async fn search_files(
        &self,
        workspace: &str,
        query: &str,
        limit: u32,
    ) -> ApiResult<Option<Vec<FileHit>>> {
        let Some(source) = &self.ctx.config.file_search else {
            return Ok(None);
        };
        Ok(Some(
            source
                .0
                .search(workspace.to_owned(), query.to_owned(), limit)
                .await?,
        ))
    }

    pub(crate) async fn do_mentions(
        &self,
        request: MentionQuery,
    ) -> ApiResult<Vec<MentionCandidate>> {
        self.check_open()?;
        let row = self.load_row(&request.session_id).await?;
        let limit = match request.limit {
            0 => 20,
            n => n.min(50),
        };
        let wants = |k: MentionKind| request.kinds.is_empty() || request.kinds.contains(&k);
        let mut out = Vec::new();
        if wants(MentionKind::File) || wants(MentionKind::Folder) {
            let files = self
                .search_files(&row.workspace_path, &request.query, limit)
                .await?
                .unwrap_or_default();
            for hit in files {
                let kind = if hit.is_dir {
                    MentionKind::Folder
                } else {
                    MentionKind::File
                };
                if !wants(kind) {
                    continue;
                }
                let item = if hit.is_dir {
                    ContextItem::Folder {
                        path: hit.path.clone(),
                    }
                } else {
                    ContextItem::File {
                        path: hit.path.clone(),
                    }
                };
                out.push(MentionCandidate {
                    kind,
                    label: hit.name,
                    detail: hit.path,
                    item,
                    insert_text: None,
                });
            }
        }
        if wants(MentionKind::Session) {
            let current = request.session_id.clone();
            let query = request.query.trim().to_owned();
            let sessions = self
                .ctx
                .store
                .run(move |c| store::list_sessions(c, None, None, 2000))
                .await?;
            let mut ranked: Vec<(u32, SessionSummary)> = sessions
                .into_iter()
                .filter(|s| s.id != current)
                .filter_map(|s| score(&query, &s.title).map(|n| (n, s)))
                .collect();
            ranked.sort_by(|a, b| b.0.cmp(&a.0).then(b.1.updated_at.cmp(&a.1.updated_at)));
            for (_, summary) in ranked.into_iter().take(limit as usize) {
                out.push(MentionCandidate {
                    kind: MentionKind::Session,
                    label: summary.title.clone(),
                    detail: summary.workspace_path.clone(),
                    item: ContextItem::Session {
                        session_id: summary.id,
                    },
                    insert_text: None,
                });
            }
        }
        if wants(MentionKind::Skill) || wants(MentionKind::Subagent) {
            let mut config = self
                .config_mentions(
                    &row,
                    &request.query,
                    wants(MentionKind::Skill),
                    wants(MentionKind::Subagent),
                )
                .await;
            config.truncate(limit as usize);
            out.extend(config);
        }
        out.truncate(limit as usize);
        Ok(out)
    }

    /// Skills and subagents of the session's active agent kind from `harness-agent-config` (workspace scope first,
    /// then user; the first definition of a name wins), ranked by name (phase-9-parity.md §9.2.5 R13).
    async fn config_mentions(
        &self,
        row: &SessionRow,
        query: &str,
        skills: bool,
        subagents: bool,
    ) -> Vec<MentionCandidate> {
        let (Some(kind), Some(paths)) = (
            self.agent_kind(&row.active_agent),
            &self.ctx.config.config_paths,
        ) else {
            return Vec::new();
        };
        let mut paths = paths.clone();
        paths.workspace = Some(std::path::PathBuf::from(&row.workspace_path));
        let service = AgentConfigService::new(paths, self.ctx.config.auth_probe.0.clone());
        let query = query.trim();
        let mut ranked: Vec<(u32, MentionCandidate)> = Vec::new();
        let concepts = [(Concept::Skill, skills), (Concept::Subagent, subagents)];
        for (concept, wanted) in concepts {
            if !wanted {
                continue;
            }
            for scope in [ConfigScope::Workspace, ConfigScope::User] {
                // 不支持的概念或作用域（如 Codex 的 subagent）只是没有候选，不是错误。
                let Ok(entries) = service
                    .list(concept, ConfigTarget { agent: kind, scope })
                    .await
                else {
                    continue;
                };
                for entry in entries {
                    let (mention, name, description, insert_text) = match entry.body {
                        EntryBody::Skill(skill) => {
                            (MentionKind::Skill, skill.name, skill.description, None)
                        }
                        EntryBody::Subagent(agent) => {
                            let text = format!("@agent-{}", agent.name);
                            (
                                MentionKind::Subagent,
                                agent.name,
                                agent.description,
                                Some(text),
                            )
                        }
                        _ => continue,
                    };
                    if entry.enabled == Some(false)
                        || ranked
                            .iter()
                            .any(|(_, c)| c.kind == mention && c.label == name)
                    {
                        continue;
                    }
                    let Some(score) = score(query, &name) else {
                        continue;
                    };
                    ranked.push((
                        score,
                        MentionCandidate {
                            kind: mention,
                            label: name,
                            detail: description.unwrap_or_default(),
                            item: ContextItem::File {
                                path: entry.source_path.to_string_lossy().into_owned(),
                            },
                            insert_text,
                        },
                    ));
                }
            }
        }
        ranked.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.label.cmp(&b.1.label)));
        ranked.into_iter().map(|(_, c)| c).collect()
    }

    // ---- slash commands ------------------------------------------------------------------------------

    pub(crate) async fn do_list_commands(&self, session: SessionId) -> ApiResult<CommandList> {
        self.check_open()?;
        let row = self.load_row(&session).await?;
        let agent = row.active_agent.clone();
        let advertised = self.ctx.chat.agent_commands(&self.ctx, &agent).await;
        let mut merged: Vec<SlashCommand> = Vec::new();
        if let (Some(kind), Some(paths)) = (self.agent_kind(&agent), &self.ctx.config.config_paths)
        {
            let mut paths = paths.clone();
            paths.workspace = Some(std::path::PathBuf::from(&row.workspace_path));
            let service = AgentConfigService::new(paths, self.ctx.config.auth_probe.0.clone());
            for (scope, source) in [
                (ConfigScope::Workspace, CommandSource::Workspace),
                (ConfigScope::User, CommandSource::User),
            ] {
                // 不支持的作用域（如 Codex 工作区）或读失败只是没有该来源的命令，不影响列表。
                let Ok(entries) = service
                    .list(Concept::Command, ConfigTarget { agent: kind, scope })
                    .await
                else {
                    continue;
                };
                for entry in entries {
                    let EntryBody::Command(command) = entry.body else {
                        continue;
                    };
                    if merged.iter().any(|c| c.name == command.name) {
                        continue;
                    }
                    merged.push(SlashCommand {
                        name: command.name,
                        description: command.description.unwrap_or_default(),
                        input_hint: command.argument_hint,
                        source,
                        path: Some(entry.source_path.to_string_lossy().into_owned()),
                    });
                }
            }
        }
        if let Some(agent_commands) = &advertised {
            // 同名时 agent 广播的命令优先（spec §9.2.1）。
            merged.retain(|c| !agent_commands.iter().any(|a| a.name == c.name));
            merged.extend(agent_commands.iter().cloned());
        }
        let skills = harness_skills(&self.ctx).await;
        merged.retain(|c| !skills.iter().any(|s| s.name == c.name));
        merged.extend(skills.into_iter().map(|s| SlashCommand {
            name: s.name,
            description: s.description,
            input_hint: None,
            source: CommandSource::Harness,
            path: Some(s.path.to_string_lossy().into_owned()),
        }));
        merged.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(CommandList {
            agent,
            known: advertised.is_some(),
            commands: merged,
        })
    }

    // ---- command center ------------------------------------------------------------------------------

    pub(crate) async fn do_command_search(
        &self,
        request: CommandSearch,
    ) -> ApiResult<CommandSearchResult> {
        self.check_open()?;
        let limit = match request.limit {
            0 => 10,
            n => n.min(50),
        };
        let wants = |s: SearchScope| request.scopes.is_empty() || request.scopes.contains(&s);
        let query = request.query.trim().to_owned();
        let mut result = CommandSearchResult::default();
        if wants(SearchScope::Actions) {
            let mut ranked: Vec<ScoredId> = request
                .actions
                .iter()
                .filter_map(|a: &ActionEntry| {
                    let best = std::iter::once(score(&query, &a.title))
                        .chain(
                            a.keywords
                                .iter()
                                .map(|k| score(&query, k).map(|s| s.saturating_sub(50))),
                        )
                        .flatten()
                        .max()?;
                    Some(ScoredId {
                        id: a.id.clone(),
                        score: best,
                    })
                })
                .collect();
            ranked.sort_by_key(|r| std::cmp::Reverse(r.score));
            ranked.truncate(limit as usize);
            result.actions = ranked;
        }
        if wants(SearchScope::Sessions) {
            result.sessions = self.search_sessions(&query, limit).await?;
        }
        if wants(SearchScope::Files) {
            if let Some(workspace) = &request.workspace_path {
                if let Some(files) = self.search_files(workspace, &query, limit).await? {
                    result.files = files;
                    result.files_available = true;
                }
            } else {
                result.files_available = self.ctx.config.file_search.is_some();
            }
        }
        Ok(result)
    }

    async fn search_sessions(&self, query: &str, limit: u32) -> ApiResult<Vec<SessionHit>> {
        let all = self
            .ctx
            .store
            .run(|c| store::list_sessions(c, None, None, 2000))
            .await?;
        if query.is_empty() {
            return Ok(all
                .into_iter()
                .take(limit as usize)
                .map(|summary| SessionHit {
                    summary,
                    snippet: String::new(),
                    score: 0,
                })
                .collect());
        }
        let mut hits: Vec<SessionHit> = Vec::new();
        for summary in &all {
            if let Some(n) = score(query, &summary.title).filter(|n| *n >= 400) {
                hits.push(SessionHit {
                    summary: summary.clone(),
                    snippet: String::new(),
                    score: n,
                });
            }
        }
        let pattern = query.to_owned();
        let transcript = self
            .ctx
            .store
            .run(move |c| store_chat::transcript_matches(c, &pattern, 200))
            .await?;
        for (id, snippet) in transcript {
            if hits.iter().any(|h| h.summary.id == id) {
                continue;
            }
            if let Some(summary) = all.iter().find(|s| s.id == id) {
                hits.push(SessionHit {
                    summary: summary.clone(),
                    snippet,
                    score: 100,
                });
            }
        }
        hits.sort_by(|a, b| {
            b.score
                .cmp(&a.score)
                .then(b.summary.updated_at.cmp(&a.summary.updated_at))
        });
        hits.truncate(limit as usize);
        Ok(hits)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scorer_orders_match_kinds() {
        assert_eq!(score("open", "open"), Some(1000));
        assert_eq!(score("op", "Open file"), Some(800));
        assert_eq!(score("fi", "Open file"), Some(600));
        assert_eq!(score("pen", "Open file"), Some(400));
        assert_eq!(score("ofl", "Open file"), Some(200));
        assert_eq!(score("xyz", "Open file"), None);
    }

    #[test]
    fn find_reports_char_offsets() {
        let hits = find_all("Ärger und ärger", &fold("ärger", false), false);
        assert_eq!(hits.iter().map(|h| h.0).collect::<Vec<_>>(), vec![0, 10]);
    }
}
