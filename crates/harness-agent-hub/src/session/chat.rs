//! Session actor parts of API 0.5.0 (phase-9-parity.md §9.2.1): rename, rewind / edit / retry, file undo,
//! plan / usage / command updates, tool content, and the decorated snapshot. Same single owner as the parent
//! module: every method runs inside the actor loop.

use harness_core_api::{
    AgentId, ApiError, ApiResult, ApplyFileChanges, ChangeDirection, ContextItem, ContextUsage,
    Cost, EventKind, FileRestoreReport, PlanEntry, RewindResult, SendAccepted, SendMessage,
    SessionDetail, SlashCommand, TokenUsage, ToolContent, ToolLocation, TurnId,
};
use serde_json::{json, Value};

use super::SessionActor;
use crate::{files, store_chat};

/// Text fields of tool content and `rawOutput` are capped at this many bytes.
const TEXT_CAP: usize = 64 * 1024;

/// What follows a rewind.
#[derive(Debug, Clone)]
pub(crate) enum Resend {
    None,
    /// Edit and resend: `context = None` keeps the old turn's items.
    Edit {
        text: String,
        context: Option<Vec<ContextItem>>,
        agent: Option<AgentId>,
    },
    /// Resend the same text and context (last turn only).
    Retry {
        agent: Option<AgentId>,
    },
}

/// `SessionCmd::Rewind` payload.
#[derive(Debug, Clone)]
pub(crate) struct RewindCmd {
    pub turn: TurnId,
    pub restore_files: bool,
    pub resend: Resend,
}

/// Result of a rewind (and of the resend, when one was asked).
#[derive(Debug, Clone)]
pub(crate) struct RewindOutcome {
    pub result: RewindResult,
    pub accepted: Option<SendAccepted>,
}

/// Caps a text at [`TEXT_CAP`] bytes (on a char boundary).
pub(super) fn capped(mut text: String) -> String {
    if text.len() > TEXT_CAP {
        let mut cut = TEXT_CAP;
        while !text.is_char_boundary(cut) {
            cut -= 1;
        }
        text.truncate(cut);
        text.push_str("…[truncated]");
    }
    text
}

fn str_of(value: &Value, key: &str) -> Option<String> {
    value.get(key).and_then(Value::as_str).map(str::to_owned)
}

/// ACP `ToolCallContent` list.
pub(super) fn tool_content(value: &Value) -> Vec<ToolContent> {
    let Some(items) = value.get("content").and_then(Value::as_array) else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(|item| match item.get("type").and_then(Value::as_str)? {
            "content" => {
                let inner = item.get("content")?;
                let text = match inner.get("type").and_then(Value::as_str) {
                    Some("text") => str_of(inner, "text")?,
                    Some(other) => format!("[{other} content]"),
                    None => return None,
                };
                Some(ToolContent::Text { text: capped(text) })
            }
            "diff" => Some(ToolContent::Diff {
                path: str_of(item, "path")?,
                old_text: str_of(item, "oldText").map(capped),
                new_text: capped(str_of(item, "newText").unwrap_or_default()),
            }),
            "terminal" => Some(ToolContent::Terminal {
                terminal_id: str_of(item, "terminalId")?,
            }),
            _ => None,
        })
        .collect()
}

/// ACP tool call `locations`.
pub(super) fn tool_locations(value: &Value) -> Vec<ToolLocation> {
    value
        .get("locations")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|l| {
                    Some(ToolLocation {
                        path: str_of(l, "path")?,
                        line: l
                            .get("line")
                            .and_then(Value::as_u64)
                            .and_then(|n| u32::try_from(n).ok()),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// `session/prompt` result `usage` → `TurnView.usage`.
pub(super) fn record_prompt_usage(actor: &mut SessionActor, turn: &TurnId, value: &Value) {
    let Some(usage) = value.get("usage").filter(|u| u.is_object()) else {
        return;
    };
    let n = |key: &str| usage.get(key).and_then(Value::as_u64);
    let parsed = TokenUsage {
        input: n("inputTokens").unwrap_or(0),
        output: n("outputTokens").unwrap_or(0),
        total: n("totalTokens").unwrap_or(0),
        thought: n("thoughtTokens"),
        cached_read: n("cachedReadTokens"),
        cached_write: n("cachedWriteTokens"),
    };
    if let Some(turn) = actor.turn_mut(turn) {
        turn.usage = Some(parsed);
    }
}

fn plan_entries(update: &Value) -> Vec<PlanEntry> {
    update
        .get("entries")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|e| {
                    Some(PlanEntry {
                        content: str_of(e, "content")?,
                        priority: str_of(e, "priority").unwrap_or_else(|| "medium".into()),
                        status: str_of(e, "status").unwrap_or_else(|| "pending".into()),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

fn context_usage(update: &Value) -> Option<ContextUsage> {
    Some(ContextUsage {
        used: update.get("used").and_then(Value::as_u64)?,
        size: update.get("size").and_then(Value::as_u64)?,
        cost: update.get("cost").and_then(|c| {
            let amount = match c.get("amount")? {
                Value::Number(n) => n.to_string(),
                Value::String(s) => s.clone(),
                _ => return None,
            };
            Some(Cost {
                amount,
                currency: str_of(c, "currency").unwrap_or_default(),
            })
        }),
    })
}

fn commands_of(update: &Value) -> Vec<SlashCommand> {
    update
        .get("availableCommands")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|c| {
                    Some(SlashCommand {
                        name: str_of(c, "name")?.trim_start_matches('/').to_owned(),
                        description: str_of(c, "description").unwrap_or_default(),
                        input_hint: c
                            .get("input")
                            .and_then(|i| i.get("hint"))
                            .and_then(Value::as_str)
                            .map(str::to_owned),
                        source: harness_core_api::CommandSource::Agent,
                        path: None,
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

impl SessionActor {
    /// Running, queued or paused turn, or a pending permission.
    pub(super) fn busy(&self) -> bool {
        self.running.is_some()
            || self.paused.is_some()
            || !self.queue.is_empty()
            || !self.pending.is_empty()
            || self.steering.is_some()
    }

    fn busy_error(&self) -> ApiError {
        ApiError::Busy(format!(
            "session {} has a running, queued or paused turn",
            self.row.id
        ))
    }

    /// Snapshot with the 0.5.0 sidebar fields (from the store), the latest plan and context usage.
    pub(super) async fn full_snapshot(&self) -> SessionDetail {
        let mut detail = self.snapshot();
        detail.plan = detail
            .turns
            .iter()
            .rev()
            .find(|t| !t.plan.is_empty())
            .map(|t| t.plan.clone())
            .unwrap_or_default();
        detail.context_usage = detail
            .turns
            .iter()
            .rev()
            .find_map(|t| t.context_usage.clone());
        detail.subagents = self.ctx.subagents.of_parent(&self.row.id);
        let mut summary = detail.summary.clone();
        let decorated = self
            .ctx
            .store
            .run(move |c| {
                store_chat::decorate(c, &mut summary)?;
                let cost = store_chat::session_cost(c, &summary.id)?;
                Ok((summary, cost))
            })
            .await;
        match decorated {
            Ok((summary, cost)) => {
                detail.summary = summary;
                detail.cost = cost;
            }
            Err(error) => tracing::warn!(%error, "decorating session summary failed"),
        }
        detail
    }

    pub(super) async fn on_rename(&mut self, title: &str) -> ApiResult<()> {
        let title = title.trim();
        let count = title.chars().count();
        if count == 0 || count > 200 {
            return Err(ApiError::InvalidArgument(
                "a title has 1 to 200 characters".into(),
            ));
        }
        if self.row.title == title {
            return Ok(());
        }
        title.clone_into(&mut self.row.title);
        self.touch().await;
        Ok(())
    }

    /// Truncates at `command.turn` (that turn and every later one), drops all native handles, optionally
    /// restores files and resends (phase-9-parity.md §9.2.1 turn actions).
    pub(super) async fn on_rewind(&mut self, command: RewindCmd) -> ApiResult<RewindOutcome> {
        if self.busy() {
            return Err(self.busy_error());
        }
        let Some(index) = self.turns.iter().position(|t| t.id == command.turn) else {
            return Err(ApiError::NotFound(format!(
                "turn {} in session {}",
                command.turn, self.row.id
            )));
        };
        if matches!(command.resend, Resend::Retry { .. }) && index + 1 != self.turns.len() {
            return Err(ApiError::InvalidArgument(
                "only the last turn can be retried".into(),
            ));
        }
        let first = self.turns[index].clone();
        let resend = match command.resend {
            Resend::None => None,
            Resend::Edit {
                text,
                context,
                agent,
            } => Some((
                text,
                context.unwrap_or_else(|| first.context.clone()),
                agent,
            )),
            Resend::Retry { agent } => {
                Some((first.user_text.clone(), first.context.clone(), agent))
            }
        };
        // 先校验再截断：重发失败时不能留下已被删除的轮次（操作要么整体生效，要么不改任何东西）。
        if let Some((text, context, agent)) = &resend {
            if text.trim().is_empty() {
                return Err(ApiError::InvalidArgument("message text is empty".into()));
            }
            if let Some(agent) = agent {
                self.ctx.require_available(agent)?;
            }
            crate::context::validate(&self.ctx, &self.row.workspace_path, &self.row.id, context)
                .await?;
        }
        let removed: Vec<TurnId> = self.turns[index..].iter().map(|t| t.id.clone()).collect();
        let files = if command.restore_files {
            Some(self.restore_for_rewind(&removed).await?)
        } else {
            None
        };
        let (session, from_seq) = (self.row.id.clone(), first.seq);
        self.ctx
            .store
            .run(move |c| store_chat::truncate_turns(c, &session, from_seq))
            .await?;
        self.turns.truncate(index);
        self.handles.clear();
        self.client_ids.retain(|_, turn| !removed.contains(turn));
        for turn in &removed {
            self.requested.remove(turn);
            self.round_base.remove(turn);
            self.tool_marks.remove(turn);
            self.snapshotted.remove(turn);
        }
        self.emit(EventKind::TurnsRemoved { from_seq });
        self.touch().await;
        let accepted = match resend {
            Some((text, context, agent)) => Some(
                self.on_send(
                    SendMessage {
                        session_id: self.row.id.clone(),
                        text,
                        agent,
                        client_message_id: None,
                        context,
                    },
                    None,
                )
                .await?,
            ),
            None => None,
        };
        Ok(RewindOutcome {
            result: RewindResult {
                removed_turns: u32::try_from(removed.len()).unwrap_or(u32::MAX),
                user_text: first.user_text,
                context: first.context,
                files,
            },
            accepted,
        })
    }

    async fn restore_for_rewind(&self, removed: &[TurnId]) -> ApiResult<FileRestoreReport> {
        let workspace = std::path::PathBuf::from(&self.row.workspace_path);
        match files::rewind_trees(&self.ctx, &workspace, removed).await? {
            Ok(trees) => {
                let plan = files::plan(&workspace, &trees).await?;
                Ok(files::apply(&workspace, &trees, &plan, None, false).await)
            }
            Err(reason) => Ok(FileRestoreReport {
                applied: Vec::new(),
                skipped: vec![(String::new(), reason)],
            }),
        }
    }

    pub(super) async fn on_apply_files(
        &mut self,
        request: ApplyFileChanges,
    ) -> ApiResult<FileRestoreReport> {
        if self.busy() {
            return Err(self.busy_error());
        }
        if self.turn(&request.turn_id).is_none() {
            return Err(ApiError::NotFound(format!(
                "turn {} in session {}",
                request.turn_id, self.row.id
            )));
        }
        let workspace = std::path::PathBuf::from(&self.row.workspace_path);
        let trees =
            match files::turn_trees(&self.ctx, &workspace, &request.turn_id, request.direction)
                .await?
            {
                Ok(trees) => trees,
                Err(reason) => {
                    return Err(ApiError::InvalidArgument(format!(
                        "file changes are not available: {reason}"
                    )))
                }
            };
        let plan = files::plan(&workspace, &trees).await?;
        let report = files::apply(
            &workspace,
            &trees,
            &plan,
            request.paths.as_deref(),
            request.include_unsafe,
        )
        .await;
        let (turn, undone) = (
            request.turn_id.clone(),
            request.direction == ChangeDirection::Undo,
        );
        self.ctx
            .store
            .run(move |c| store_chat::set_undone(c, &turn, undone))
            .await?;
        self.emit(EventKind::FilesRestored {
            turn_id: request.turn_id,
            direction: request.direction,
        });
        Ok(report)
    }

    /// `available_commands_update` from any native session of this actor (also before Ready).
    pub(super) async fn on_commands_update(&mut self, native: &str, update: &Value) {
        let agent = self
            .handles
            .iter()
            .find(|(_, h)| h.native == native)
            .map(|(a, _)| a.clone())
            .or_else(|| {
                self.running
                    .as_ref()
                    .filter(|r| r.native.as_deref().map_or(true, |n| n == native))
                    .map(|r| r.agent.clone())
            });
        let Some(agent) = agent else {
            return;
        };
        self.ctx
            .chat
            .record_commands(&self.ctx, &agent, commands_of(update))
            .await;
    }

    /// ACP `plan` and `usage_update` of the running turn.
    pub(super) async fn on_progress_update(
        &mut self,
        turn_id: &TurnId,
        agent: &AgentId,
        update: &Value,
    ) {
        match update.get("sessionUpdate").and_then(Value::as_str) {
            Some("plan") => {
                let entries = plan_entries(update);
                if let Some(turn) = self.turn_mut(turn_id) {
                    turn.plan.clone_from(&entries);
                }
                self.append_event(turn_id, "plan", json!(entries)).await;
                self.emit(EventKind::PlanUpdated {
                    turn_id: turn_id.clone(),
                    agent: agent.clone(),
                    entries,
                });
            }
            Some("usage_update") => {
                let Some(usage) = context_usage(update) else {
                    return;
                };
                if let Some(turn) = self.turn_mut(turn_id) {
                    turn.context_usage = Some(usage.clone());
                }
                self.emit(EventKind::UsageUpdated {
                    agent: agent.clone(),
                    usage,
                });
            }
            _ => {}
        }
    }
}
