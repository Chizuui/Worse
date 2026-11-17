//! Phase 9.2 wiring of the agent A events and chat commands (spec `docs/specs/harness/phase-9-parity.md` §7):
//! tool detail and thinking rows (R8), deny with feedback (R9), session actions (R10), groups (R11), the
//! changed-files summary (R14), plan / context meter, agent slash commands (R13) and scheduled prompts (R18).
//!
//! Every reply of a `ChatCall` arrives on a tokio oneshot and is applied in a later `with_state` turn
//! (`slint::spawn_local`), never inside the borrow that sent it.

use std::collections::HashSet;
use std::time::{SystemTime, UNIX_EPOCH};

use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel};

use harness_core_api::{
    ApiError, BackgroundTask, ChangeDirection, ContextUsage, FileAction, FileChangePlan,
    GroupColor, NewSchedule, PlanEntry, ScheduleKind, ScheduleTarget, ScheduledPrompt,
    SessionGroup, SessionSelection, SubagentStatus, SubagentView, ToolContent, ToolLocation,
    UpdateGroup, UpdateSchedule,
};

use super::{item_id, with_state, UiState};
use crate::core::{ChatCall, Command, Item, ItemId, Reply, SessionId, SettingsCall};
use crate::ui::{
    BackgroundTaskRow, ChangeRow, ChatItem, ChatUi, GroupRow, OutputRow, PlanRow, ScheduleRow,
    SessionRow, SubagentRow, ToolPart, UsagePart,
};

/// Longest diff body shown in a tool card (lines); the rest is summarised in one line.
const MAX_DIFF_LINES: usize = 400;

/// What the shared text dialog (`ChatUi.dialog-*`) submits to.
#[derive(Clone, Default)]
pub(super) enum DialogTarget {
    #[default]
    None,
    Rename(SessionId),
    Edit {
        session: SessionId,
        item: ItemId,
    },
    Rewind {
        session: SessionId,
        item: ItemId,
    },
    Deny {
        session: SessionId,
        request: u64,
    },
    Delete(SessionId),
    NewGroup,
    RenameGroup(String),
    DeleteGroup(String),
    NewSchedule,
}

/// Phase 9.2 view state kept next to `UiState` (thinking rows, groups, schedules, change plans).
#[derive(Default)]
pub(super) struct TurnState {
    /// Live thinking row: (synthetic id, row index). Synthetic ids are negative (-2, -3, …).
    thought: Option<(i32, usize)>,
    thought_rows: std::collections::HashMap<i32, usize>,
    next_thought: i32,
    pub(super) groups: Vec<SessionGroup>,
    collapsed: HashSet<String>,
    schedules: Vec<ScheduledPrompt>,
    /// Turn id → assistant item that shows its change summary (the last item of the turn).
    change_turns: std::collections::HashMap<String, ItemId>,
    dialog: DialogTarget,
    /// Session the user marked unread while it was open: not auto-cleared until another session is shown.
    pub(super) keep_unread: Option<String>,
    compact_due: bool,
    compacted: bool,
    outputs: Vec<String>,
    memory_tokens: Option<u64>,
    memory_asked: bool,
    last_usage: Option<(u64, u64)>,
}

const AUTO_COMPACT_AT: f32 = 0.85;

fn measure_memory(workspace: std::path::PathBuf) {
    std::thread::spawn(move || {
        let mut files = vec![
            workspace.join("CLAUDE.md"),
            workspace.join("AGENTS.md"),
            workspace.join(".claude").join("CLAUDE.md"),
        ];
        if let Some(dir) = crate::plan_usage::default_dir() {
            files.push(dir.join("CLAUDE.md"));
        }
        let total = tokio::runtime::Builder::new_current_thread()
            .build()
            .map(|rt| {
                rt.block_on(async {
                    let mut bytes = 0_u64;
                    for f in files {
                        if let Ok(meta) = tokio::fs::metadata(&f).await {
                            bytes += meta.len();
                        }
                    }
                    bytes / 4
                })
            })
            .unwrap_or(0);
        let _ = slint::invoke_from_event_loop(move || {
            with_state(|s| {
                s.turn.memory_tokens = Some(total);
                s.render_parts();
            });
        });
    });
}

fn sv(text: &str) -> SharedString {
    SharedString::from(text)
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|d| i64::try_from(d.as_millis()).ok())
        .unwrap_or_default()
}

/// Seconds since the epoch, for the elapsed times of the subagent panel (relative to `ChatUi.now-s`).
pub(super) fn now_s() -> i32 {
    i32::try_from(now_ms() / 1000).unwrap_or(i32::MAX)
}

pub(super) fn subagent_row(s: &SubagentView) -> SubagentRow {
    let status = match s.status {
        SubagentStatus::Running => 0,
        SubagentStatus::NeedsApproval => 1,
        SubagentStatus::Done => 2,
        SubagentStatus::Failed => 3,
        SubagentStatus::Cancelled => 4,
        SubagentStatus::Interrupted => 5,
    };
    let secs = |ms: i64| i32::try_from(ms / 1000).unwrap_or(i32::MAX);
    SubagentRow {
        id: sv(s.session_id.as_str()),
        agent: sv(s.agent.as_str()),
        title: sv(&s.title),
        status,
        activity: sv(&s.activity),
        preview: sv(&s.result_preview),
        started_s: secs(s.started_at),
        // 0 = still running: the panel measures against `now-s`.
        finished_s: s.finished_at.map_or(0, secs),
        agent_name: sv(s.agent.as_str()),
    }
}

fn background_row(t: &BackgroundTask) -> BackgroundTaskRow {
    let secs = |ms: i64| i32::try_from(ms / 1000).unwrap_or(i32::MAX);
    BackgroundTaskRow {
        id: sv(&t.id),
        agent: sv(t.agent.as_str()),
        name: sv(&t.name),
        description: sv(&t.description),
        summary: sv(&t.summary),
        state: sv(&t.state),
        running: t.is_running(),
        can_stop: t.can_stop,
        started_s: secs(t.started_at),
        finished_s: t.finished_at.map_or(0, secs),
    }
}

fn part(kind: i32, text: &str) -> ToolPart {
    ToolPart {
        kind,
        text: sv(&crate::markdown::expand_tabs(text)),
    }
}

fn fenced_body(text: &str) -> Option<&str> {
    let trimmed = text.trim();
    let rest = trimmed.strip_prefix("```")?;
    let (_, body) = rest.split_once('\n')?;
    let body = body.strip_suffix("```")?;
    let body = body.strip_suffix('\n').unwrap_or(body);
    let body = body.strip_suffix('\r').unwrap_or(body);
    if body
        .lines()
        .any(|line| line.trim_start().starts_with("```"))
    {
        return None;
    }
    Some(body)
}

const THOUGHT_PREVIEW_CHARS: usize = 200;

fn thought_preview(text: &str) -> String {
    let mut out = String::new();
    let mut count = 0;
    for word in text.split_whitespace() {
        if count >= THOUGHT_PREVIEW_CHARS {
            break;
        }
        if !out.is_empty() {
            out.push(' ');
            count += 1;
        }
        out.push_str(word);
        count += word.chars().count();
    }
    out
}

/// Line diff for a tool card: common head / tail lines are dropped, the middle shows as removed then added.
fn diff_parts(out: &mut Vec<ToolPart>, old: Option<&str>, new: &str) {
    let old_lines: Vec<&str> = old.map(|o| o.lines().collect()).unwrap_or_default();
    let new_lines: Vec<&str> = new.lines().collect();
    let head = old_lines
        .iter()
        .zip(&new_lines)
        .take_while(|(a, b)| a == b)
        .count();
    let tail = old_lines[head..]
        .iter()
        .rev()
        .zip(new_lines[head..].iter().rev())
        .take_while(|(a, b)| a == b)
        .count();
    let removed = &old_lines[head..old_lines.len() - tail];
    let added = &new_lines[head..new_lines.len() - tail];
    let mut shown = 0usize;
    for (kind, lines) in [(3, removed), (2, added)] {
        for line in lines {
            if shown == MAX_DIFF_LINES {
                out.push(part(
                    6,
                    &format!("… {} more lines", removed.len() + added.len() - shown),
                ));
                return;
            }
            out.push(part(kind, line));
            shown += 1;
        }
    }
}

/// Lines shown under a collapsed edit row.
const PEEK_LINES: usize = 6;

/// The first changed lines of a tool card (removed / added rows of its diffs), for the collapsed row; "…" when
/// there are more.
fn peek_parts(parts: &[ToolPart]) -> Vec<ToolPart> {
    let changed: Vec<&ToolPart> = parts.iter().filter(|p| p.kind == 2 || p.kind == 3).collect();
    let mut out: Vec<ToolPart> = changed.iter().take(PEEK_LINES).map(|p| (*p).clone()).collect();
    if changed.len() > PEEK_LINES {
        out.push(part(6, &format!("{} more lines", changed.len() - PEEK_LINES)));
    }
    out
}

/// `ToolPart` rows of an expanded tool card. Section labels carry an id ("input" / "output") translated in Slint.
fn tool_parts(
    content: &[ToolContent],
    locations: &[ToolLocation],
    raw_input: Option<&str>,
    raw_output: Option<&str>,
) -> Vec<ToolPart> {
    let mut out = Vec::new();
    for location in locations {
        let text = match location.line {
            Some(line) => format!("{}:{line}", location.path),
            None => location.path.clone(),
        };
        out.push(part(4, &text));
    }
    for item in content {
        match item {
            ToolContent::Text { text } => match fenced_body(text) {
                Some(body) => out.push(part(6, body.trim_end())),
                None => out.push(part(0, text.trim_end())),
            },
            ToolContent::Diff {
                path,
                old_text,
                new_text,
            } => {
                out.push(part(1, path));
                diff_parts(&mut out, old_text.as_deref(), new_text);
            }
            ToolContent::Terminal { .. } => {}
        }
    }
    for (label, raw) in [("input", raw_input), ("output", raw_output)] {
        if let Some(raw) = raw.filter(|r| !r.trim().is_empty()) {
            out.push(part(5, label));
            out.push(part(6, raw.trim_end()));
        }
    }
    out
}

fn plan_rows(entries: &[PlanEntry]) -> Vec<PlanRow> {
    entries
        .iter()
        .map(|e| PlanRow {
            text: sv(&e.content),
            status: match e.status.as_str() {
                "in_progress" => 1,
                "completed" => 2,
                _ => 0,
            },
            priority: sv(&e.priority),
        })
        .collect()
}

/// "12.3k / 200k" style token counts.
fn tokens(n: u64) -> String {
    #[allow(clippy::cast_precision_loss)]
    let value = n as f64;
    if n >= 1_000_000 {
        format!("{:.1}M", value / 1_000_000.0)
    } else if n >= 1000 {
        format!("{:.1}k", value / 1000.0)
    } else {
        n.to_string()
    }
}

fn schedule_row(s: &ScheduledPrompt, now: i64) -> ScheduleRow {
    let (kind, every_min, at) = match &s.kind {
        ScheduleKind::Interval { seconds } => (
            "interval",
            i32::try_from(seconds / 60).unwrap_or(i32::MAX),
            String::new(),
        ),
        ScheduleKind::Daily { minute_of_day, .. } => (
            "daily",
            0,
            format!("{:02}:{:02}", minute_of_day / 60, minute_of_day % 60),
        ),
    };
    let next_in = s.next_run.map_or(-1, |next| {
        i32::try_from(((next - now).max(0) + 59_999) / 60_000).unwrap_or(i32::MAX)
    });
    ScheduleRow {
        id: sv(&s.id),
        title: sv(&s.title),
        prompt: sv(&s.prompt),
        enabled: s.enabled,
        kind: sv(kind),
        every_min,
        at: sv(&at),
        next_in,
        last: sv(s.last_result.as_deref().unwrap_or_default()),
    }
}

impl UiState {
    pub(super) fn ui(&self) -> ChatUi<'_> {
        self.window.global::<ChatUi>()
    }

    pub(super) fn fail(&self, error: &str) {
        self.window.set_error_text(sv(error));
    }

    /// Sends a command whose reply is applied later on the UI thread (`done`), errors go to the banner.
    pub(super) fn request<T: 'static>(
        &self,
        make: impl FnOnce(Reply<T>) -> Command,
        done: impl FnOnce(&mut UiState, T) + 'static,
    ) {
        let (reply, answer) = tokio::sync::oneshot::channel();
        self.send(make(reply));
        let spawned = slint::spawn_local(async move {
            match answer.await {
                Ok(Ok(value)) => with_state(|s| done(s, value)),
                Ok(Err(error)) => with_state(|s| s.fail(&error.to_string())),
                Err(_) => with_state(|s| s.fail(&ApiError::ShutDown.to_string())),
            }
        });
        if spawned.is_err() {
            self.fail("event loop unavailable");
        }
    }

    pub(super) fn chat_call<T: 'static>(
        &self,
        make: impl FnOnce(Reply<T>) -> ChatCall,
        done: impl FnOnce(&mut UiState, T) + 'static,
    ) {
        self.request(|reply| Command::Chat(make(reply)), done);
    }

    fn chat_call_quiet<T: 'static>(
        &self,
        make: impl FnOnce(Reply<T>) -> ChatCall,
        done: impl FnOnce(&mut UiState, T) + 'static,
    ) {
        let (reply, answer) = tokio::sync::oneshot::channel();
        self.send(Command::Chat(make(reply)));
        let _ = slint::spawn_local(async move {
            if let Ok(Ok(value)) = answer.await {
                with_state(|s| done(s, value));
            }
        });
    }

    /// Session of an action: the given id, or the open session when it is empty.
    fn target_session(&self, id: &str) -> Option<SessionId> {
        if id.is_empty() {
            self.active.clone()
        } else {
            Some(id.to_string())
        }
    }

    // ---- per-session reset -------------------------------------------------------------------------

    /// A session was opened or left: drops the live thinking row, plan, usage and change plans.
    pub(super) fn reset_turn_view(&mut self) {
        crate::live::clear(&self.window);
        self.turn.thought = None;
        self.turn.thought_rows.clear();
        self.turn.next_thought = -2;
        self.turn.change_turns.clear();
        let ui = self.ui();
        ui.set_plan(ModelRc::new(VecModel::<PlanRow>::default()));
        ui.set_subagents(ModelRc::new(VecModel::<SubagentRow>::default()));
        ui.set_subagents_running(0);
        ui.set_subagents_waiting(0);
        ui.set_subagents_active(ModelRc::new(VecModel::<SubagentRow>::default()));
        ui.set_background_tasks(ModelRc::new(VecModel::<BackgroundTaskRow>::default()));
        ui.set_background_tasks_running(0);
        ui.set_parent_id(SharedString::new());
        ui.set_parent_title(SharedString::new());
        ui.set_usage_known(false);
        ui.set_worktree_branch(SharedString::new());
        ui.set_worktree_path(SharedString::new());
        ui.set_session_cost(SharedString::new());
        ui.set_last_assistant(-1);
        self.turn.compact_due = false;
        self.turn.compacted = false;
        self.turn.outputs.clear();
        self.turn.last_usage = None;
        self.ui()
            .set_outputs(ModelRc::new(VecModel::<OutputRow>::default()));
    }

    /// After `SessionOpened`: unread cleared, agent commands, change plans of the finished assistant turns.
    pub(super) fn session_opened(&mut self, items: &[(ItemId, Item)], unread: bool) {
        let Some(session) = self.active.clone() else {
            return;
        };
        if unread && self.turn.keep_unread.as_deref() != Some(session.as_str()) {
            self.set_unread(&session, false);
        }
        self.load_commands();
        for (id, item) in items {
            if let Item::Assistant { .. } = item {
                self.plan_changes(*id, ChangeDirection::Undo);
            }
        }
    }

    pub(super) fn note_item(&mut self, id: ItemId, item: &Item) {
        self.finish_thought();
        if let Item::Assistant { .. } = item {
            self.ui().set_last_assistant(item_id(id));
        }
    }

    pub(super) fn turn_ended(&mut self, id: ItemId) {
        self.finish_thought();
        self.auto_compact();
        if self.rows.contains_key(&id) {
            self.plan_changes(id, ChangeDirection::Undo);
        }
    }

    pub(super) fn set_unread(&self, session: &str, unread: bool) {
        let session = session.to_string();
        self.chat_call(
            |reply| ChatCall::SetUnread {
                session,
                unread,
                reply,
            },
            |_, ()| {},
        );
    }

    // ---- thinking rows (R8) ------------------------------------------------------------------------

    pub(super) fn thought(&mut self, text: &str) {
        if let Some((_, index)) = self.turn.thought {
            if let Some(mut row) = self.chat.row_data(index) {
                let mut full = row.text.to_string();
                full.push_str(text);
                if row.title.chars().count() < THOUGHT_PREVIEW_CHARS {
                    row.title = sv(&thought_preview(&full));
                }
                row.text = sv(&full);
                self.chat.set_row_data(index, row);
                return;
            }
        }
        let id = self.turn.next_thought;
        self.turn.next_thought -= 1;
        let index = self.chat.row_count();
        self.chat.push(ChatItem {
            id,
            kind: 5,
            text: sv(text),
            title: sv(&thought_preview(text)),
            status: 0,
            ..ChatItem::default()
        });
        self.turn.thought = Some((id, index));
        self.turn.thought_rows.insert(id, index);
    }

    fn finish_thought(&mut self) {
        if let Some((_, index)) = self.turn.thought.take() {
            if let Some(mut row) = self.chat.row_data(index) {
                row.status = 1;
                self.chat.set_row_data(index, row);
            }
        }
    }

    fn row_index(&self, id: i32) -> Option<usize> {
        if id < 0 {
            self.turn.thought_rows.get(&id).copied()
        } else {
            u64::try_from(id)
                .ok()
                .and_then(|id| self.rows.get(&id).copied())
        }
    }

    fn toggle_detail(&self, id: i32) {
        let Some(index) = self.row_index(id) else {
            return;
        };
        if let Some(mut row) = self.chat.row_data(index) {
            row.expanded = !row.expanded;
            self.chat.set_row_data(index, row);
        }
    }

    // ---- tool detail, plan, usage --------------------------------------------------------------------

    pub(super) fn tool_detail(
        &self,
        id: ItemId,
        kind: Option<&str>,
        content: &[ToolContent],
        locations: &[ToolLocation],
        raw_input: Option<&str>,
        raw_output: Option<&str>,
    ) {
        let parts = tool_parts(content, locations, raw_input, raw_output);
        let peek = peek_parts(&parts);
        let file = crate::live::row_file(kind, content, locations);
        self.update_row(id, |row| {
            row.tool_kind = sv(kind.unwrap_or_default());
            row.parts = ModelRc::new(VecModel::from(parts));
            row.peek = ModelRc::new(VecModel::from(peek));
            row.file = sv(&file.path);
            row.file_line = file.line;
            row.range = sv(&file.range);
        });
    }

    pub(super) fn plan(&self, entries: &[PlanEntry]) {
        self.ui()
            .set_plan(ModelRc::new(VecModel::from(plan_rows(entries))));
    }

    /// Subagent panel rows (§9.8); the elapsed clock restarts from now.
    pub(super) fn session_meta(
        &self,
        worktree: Option<&harness_core_api::WorktreeInfo>,
        cost: Option<&harness_core_api::SessionCost>,
    ) {
        let ui = self.ui();
        ui.set_worktree_branch(sv(worktree.map_or("", |w| w.branch.as_str())));
        ui.set_worktree_path(sv(worktree.map_or("", |w| w.path.as_str())));
        let money = |amount: &str| {
            amount
                .trim()
                .parse::<f64>()
                .map_or_else(|_| amount.to_owned(), |v| format!("${v:.2}"))
        };
        let text = cost.map_or_else(String::new, |c| match &c.subagents {
            Some(sub) => format!(
                "Session cost {}  ·  incl. subagents {}",
                money(&c.total.amount),
                money(&sub.amount)
            ),
            None => format!("Session cost {}", money(&c.total.amount)),
        });
        ui.set_session_cost(sv(&text));
    }

    pub(super) fn subagents(&self, items: &[SubagentView]) {
        let ui = self.ui();
        ui.set_now_s(now_s());
        let running = items.iter().filter(|s| s.status.is_active()).count();
        ui.set_subagents_running(i32::try_from(running).unwrap_or(i32::MAX));
        let waiting = items.iter().filter(|s| matches!(s.status, SubagentStatus::NeedsApproval)).count();
        ui.set_subagents_waiting(i32::try_from(waiting).unwrap_or(i32::MAX));
        let rows: Vec<SubagentRow> = items
            .iter()
            .map(|s| SubagentRow {
                agent_name: sv(&self.agent_name(s.agent.as_str())),
                ..subagent_row(s)
            })
            .collect();
        let active: Vec<SubagentRow> = items
            .iter()
            .zip(&rows)
            .filter(|(s, _)| s.status.is_active())
            .map(|(_, r)| r.clone())
            .collect();
        ui.set_subagents_active(ModelRc::new(VecModel::from(active)));
        ui.set_subagents(ModelRc::new(VecModel::from(rows)));
    }

    pub(super) fn background_tasks(&self, tasks: &[BackgroundTask]) {
        let ui = self.ui();
        ui.set_now_s(now_s());
        let running = tasks.iter().filter(|t| t.is_running()).count();
        ui.set_background_tasks_running(i32::try_from(running).unwrap_or(i32::MAX));
        ui.set_background_tasks(ModelRc::new(VecModel::from(
            tasks.iter().map(background_row).collect::<Vec<_>>(),
        )));
    }

    fn stop_background_task(&self, id: &str) {
        if let Some(session) = self.active.clone() {
            self.send(Command::StopBackgroundTask {
                session,
                id: id.to_owned(),
            });
        }
    }

    pub(super) fn parent_link(&self, parent: Option<(String, String)>) {
        let ui = self.ui();
        let (id, title) = parent.unwrap_or_default();
        ui.set_parent_id(sv(&id));
        ui.set_parent_title(sv(&title));
    }

    fn subagent_action(&self, action: &str, id: &str) {
        match action {
            "open" | "back" if !id.is_empty() => self.send(Command::OpenSession {
                session: id.to_owned(),
            }),
            "tick" => self.ui().set_now_s(now_s()),
            // Orchestra live view: stop the subagent's running turn (same as Stop in its own session).
            "stop" if !id.is_empty() => self.send(Command::Cancel {
                session: id.to_owned(),
            }),
            _ => {}
        }
    }

    fn auto_compact(&mut self) {
        if !self.turn.compact_due || !self.ui().get_auto_compact() {
            return;
        }
        let Some(session) = self.active.clone() else {
            return;
        };
        self.turn.compact_due = false;
        self.turn.compacted = true;
        self.send_message(session, "/compact".into(), Vec::new());
    }

    pub(super) fn usage(&mut self, usage: &ContextUsage) {
        let ui = self.ui();
        ui.set_usage_known(usage.size > 0);
        #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
        let ratio = if usage.size == 0 {
            0.0
        } else {
            (usage.used as f64 / usage.size as f64).clamp(0.0, 1.0) as f32
        };
        ui.set_usage_ratio(ratio);
        let auto = ui.get_auto_compact();
        let buffer = usage
            .size
            .saturating_sub(usage.size.saturating_mul(85) / 100);
        ui.set_usage_used_text(sv(&tokens(usage.used)));
        ui.set_usage_buffer_text(sv(&tokens(buffer)));
        ui.set_usage_free_text(sv(&tokens(usage.size.saturating_sub(usage.used))));
        ui.set_usage_free_auto_text(sv(&tokens(
            usage.size.saturating_sub(usage.used).saturating_sub(buffer),
        )));
        ui.set_usage_text(sv(&format!(
            "{} / {}",
            tokens(usage.used),
            tokens(usage.size)
        )));
        ui.set_usage_cost(sv(&usage
            .cost
            .as_ref()
            .map(|c| format!("{} {}", c.amount, c.currency))
            .unwrap_or_default()));
        if ratio < AUTO_COMPACT_AT {
            self.turn.compact_due = false;
            self.turn.compacted = false;
        } else if auto && !self.turn.compacted {
            self.turn.compact_due = true;
        }
        self.turn.last_usage = (usage.size > 0).then_some((usage.used, usage.size));
        if !self.turn.memory_asked {
            self.turn.memory_asked = true;
            measure_memory(self.workspace.clone());
        }
        self.render_parts();
    }

    pub(super) fn render_parts(&self) {
        let Some((used, size)) = self.turn.last_usage else {
            return;
        };
        let ui = self.ui();
        let auto = ui.get_auto_compact();
        let skills = u64::try_from(crate::chat::skill_count()).unwrap_or(0);
        let estimates = [
            ("Skills", 1, skills.saturating_mul(45)),
            ("System tools", 2, 14_100),
            ("Memory files", 3, self.turn.memory_tokens.unwrap_or(0)),
            ("System prompt", 4, 2_900),
            ("MCP tools", 5, 2_000),
        ];
        let overhead: u64 = estimates.iter().map(|e| e.2).sum();
        #[allow(
            clippy::cast_precision_loss,
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss
        )]
        let scale = |n: u64| -> u64 {
            if overhead <= used || overhead == 0 {
                n
            } else {
                (n as f64 * used as f64 / overhead as f64) as u64
            }
        };
        let fixed: Vec<(&str, i32, u64)> =
            estimates.iter().map(|e| (e.0, e.1, scale(e.2))).collect();
        let messages = used.saturating_sub(fixed.iter().map(|e| e.2).sum());
        let buffer = if auto {
            size.saturating_sub(size.saturating_mul(85) / 100)
        } else {
            0
        };
        let free = size.saturating_sub(used).saturating_sub(buffer);
        let mut parts = vec![("Messages", 0, messages)];
        parts.extend(fixed);
        if auto {
            parts.push((
                "Autocompact buffer",
                6,
                buffer.min(size.saturating_sub(used)),
            ));
        }
        parts.push(("Free space", 7, free));
        let mut start = 0.0_f32;
        let rows: Vec<UsagePart> = parts
            .into_iter()
            .filter(|p| p.2 > 0 || p.1 == 7)
            .map(|(label, tone, n)| {
                #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
                let share = (n as f64 / size.max(1) as f64) as f32;
                let row = UsagePart {
                    label: sv(label),
                    tone,
                    amount: sv(&tokens(n)),
                    share,
                    start,
                };
                start += share;
                row
            })
            .collect();
        ui.set_usage_parts(ModelRc::new(VecModel::from(rows)));
    }

    // ---- agent slash commands (R13) ------------------------------------------------------------------

    pub(super) fn load_commands(&self) {
        let Some(session) = self.active.clone() else {
            return;
        };
        self.chat_call(
            |reply| ChatCall::Commands { session, reply },
            |_, list| {
                crate::chat::agent_commands(
                    list.commands
                        .into_iter()
                        .map(|c| {
                            let detail = match c.input_hint {
                                Some(hint) if !hint.is_empty() => {
                                    format!("{} · {hint}", c.description)
                                }
                                _ => c.description,
                            };
                            (c.name, detail)
                        })
                        .collect(),
                );
            },
        );
    }

    // ---- changed files (R14) -------------------------------------------------------------------------

    fn plan_changes(&self, item: ItemId, direction: ChangeDirection) {
        self.plan_changes_with(item, direction, None);
    }

    fn plan_changes_with(
        &self,
        item: ItemId,
        direction: ChangeDirection,
        undo: Option<FileChangePlan>,
    ) {
        let Some(session) = self.active.clone() else {
            return;
        };
        let asked = session.clone();
        self.chat_call_quiet(
            |reply| ChatCall::PlanFiles {
                session,
                item,
                direction,
                reply,
            },
            move |s, plan| s.changes_planned(&asked, item, plan, undo),
        );
    }

    /// `undo` = the Undo plan already fetched when this is the follow-up Reapply plan.
    fn changes_planned(
        &mut self,
        session: &str,
        item: ItemId,
        plan: FileChangePlan,
        undo: Option<FileChangePlan>,
    ) {
        if !self.is_active(session) || !self.rows.contains_key(&item) {
            return;
        }
        let usable = plan.available && !plan.files.is_empty();
        if plan.direction == ChangeDirection::Undo {
            // 修复：撤销之后 hub 仍返回可用的撤销计划（每个文件都标为不安全），卡片一直显示“撤销”。
            // 早先用“没有安全文件”猜测撤销已生效；现在按 hub 的 `applied`（文件已等于撤销目标）判断，
            // 已撤销时改显示“重新应用”计划（phase-9-parity.md §9.2.5 R14 follow-up）。
            if !usable || plan.applied {
                let keep = (usable && !plan.applied).then_some(plan);
                self.plan_changes_with(item, ChangeDirection::Reapply, keep);
                return;
            }
        } else if !usable || plan.applied {
            match undo {
                Some(undo) => self.show_changes(item, undo),
                None => self.update_row(item, |row| {
                    row.changes = ModelRc::default();
                    row.change_dir = 0;
                }),
            }
            return;
        }
        self.show_changes(item, plan);
    }

    fn show_changes(&mut self, item: ItemId, plan: FileChangePlan) {
        // 同一轮次（failover）只在最后一个 assistant 条目上显示一张卡片。
        let turn = plan.turn_id.to_string();
        match self.turn.change_turns.get(&turn).copied() {
            Some(other) if other > item => return,
            Some(other) if other != item => self.update_row(other, |row| {
                row.changes = ModelRc::default();
                row.change_dir = 0;
            }),
            _ => {}
        }
        self.turn.change_turns.insert(turn, item);
        let mut grew = false;
        for f in &plan.files {
            if !self.turn.outputs.contains(&f.path) {
                self.turn.outputs.push(f.path.clone());
                grew = true;
            }
        }
        if grew {
            let outputs: Vec<OutputRow> = self
                .turn
                .outputs
                .iter()
                .map(|path| OutputRow {
                    name: sv(path.rsplit(['/', '\\']).next().unwrap_or(path)),
                    path: sv(path),
                })
                .collect();
            self.ui().set_outputs(ModelRc::new(VecModel::from(outputs)));
        }
        let rows: Vec<ChangeRow> = plan
            .files
            .iter()
            .map(|f| ChangeRow {
                path: sv(&f.path),
                action: match f.action {
                    FileAction::Restore => 0,
                    FileAction::Delete => 1,
                },
                safe: f.safe,
                reason: sv(f.reason.as_deref().unwrap_or_default()),
            })
            .collect();
        let dir = if plan.direction == ChangeDirection::Undo {
            1
        } else {
            2
        };
        let note = plan.reason.unwrap_or_default();
        self.update_row(item, |row| {
            row.changes = ModelRc::new(VecModel::from(rows));
            row.change_dir = dir;
            row.change_note = sv(&note);
        });
    }

    pub(super) fn files_restored(&self, session: &str, item: Option<ItemId>) {
        if !self.is_active(session) {
            return;
        }
        match item {
            Some(item) => self.plan_changes(item, ChangeDirection::Undo),
            None => {
                for &item in self.turn.change_turns.values() {
                    self.plan_changes(item, ChangeDirection::Undo);
                }
            }
        }
    }

    fn apply_changes(&self, item: ItemId, direction: ChangeDirection) {
        let Some(session) = self.active.clone() else {
            return;
        };
        self.chat_call(
            |reply| ChatCall::ApplyFiles {
                session,
                item,
                direction,
                paths: None,
                include_unsafe: false,
                reply,
            },
            move |s, report| {
                crate::chat::toast_label(if report.skipped.is_empty() {
                    "filesRestored"
                } else {
                    "filesSkipped"
                });
                s.plan_changes(item, ChangeDirection::Undo);
            },
        );
    }

    // ---- turn actions (R10) --------------------------------------------------------------------------

    fn turn_action(&mut self, action: &str, id: i32) {
        let (Some(session), Ok(item)) = (self.active.clone(), u64::try_from(id)) else {
            return;
        };
        match action {
            "edit" => {
                let text = self
                    .rows
                    .get(&item)
                    .and_then(|&i| self.chat.row_data(i))
                    .map(|r| r.text.to_string())
                    .unwrap_or_default();
                self.open_dialog(DialogTarget::Edit { session, item }, &text, "", true);
            }
            "rewind" => self.open_dialog(DialogTarget::Rewind { session, item }, "", "", false),
            "fork" => self.fork(session, Some(item)),
            "retry" => self.chat_call(
                |reply| ChatCall::Retry {
                    session,
                    item,
                    agent: None,
                    restore_files: false,
                    reply,
                },
                |_, ()| {},
            ),
            "undo" => self.apply_changes(item, ChangeDirection::Undo),
            "reapply" => self.apply_changes(item, ChangeDirection::Reapply),
            _ => {}
        }
    }

    fn fork(&self, session: SessionId, item: Option<ItemId>) {
        self.chat_call(
            |reply| ChatCall::Fork {
                session,
                item,
                reply,
            },
            |s, summary| {
                s.send(Command::OpenSession {
                    session: summary.id,
                });
            },
        );
    }

    // ---- shared text dialog ----------------------------------------------------------------------------

    fn open_dialog(&mut self, target: DialogTarget, text: &str, text2: &str, multiline: bool) {
        let kind = match &target {
            DialogTarget::None => return,
            DialogTarget::Rename(_) => "rename",
            DialogTarget::Edit { .. } => "edit",
            DialogTarget::Rewind { .. } => "rewind",
            DialogTarget::Deny { .. } => "deny",
            DialogTarget::Delete(_) => "delete",
            DialogTarget::NewGroup => "new-group",
            DialogTarget::RenameGroup(_) => "rename-group",
            DialogTarget::DeleteGroup(_) => "delete-group",
            DialogTarget::NewSchedule => "new-schedule",
        };
        // 确认类对话框（回退、删除）没有输入框：用一个只读说明 + 勾选/确认按钮。
        let confirm_only = matches!(
            target,
            DialogTarget::Rewind { .. } | DialogTarget::Delete(_) | DialogTarget::DeleteGroup(_)
        );
        let destructive = confirm_only;
        self.turn.dialog = target;
        let ui = self.ui();
        ui.set_dialog_kind(sv(kind));
        ui.set_dialog_text(sv(text));
        ui.set_dialog_text2(sv(text2));
        ui.set_dialog_check(false);
        ui.set_dialog_multiline(multiline);
        ui.set_dialog_destructive(destructive);
        ui.set_dialog_confirm_only(confirm_only);
        ui.set_dialog_open(true);
    }

    fn dialog_submit(&mut self) {
        let (text, text2, check) = {
            let ui = self.ui();
            (
                ui.get_dialog_text().to_string(),
                ui.get_dialog_text2().to_string(),
                ui.get_dialog_check(),
            )
        };
        let target = std::mem::take(&mut self.turn.dialog);
        let needs_text = matches!(
            target,
            DialogTarget::Rename(_)
                | DialogTarget::Edit { .. }
                | DialogTarget::Deny { .. }
                | DialogTarget::NewGroup
                | DialogTarget::RenameGroup(_)
                | DialogTarget::NewSchedule
        );
        if needs_text && text.trim().is_empty() {
            self.turn.dialog = target;
            return;
        }
        self.ui().set_dialog_open(false);
        match target {
            DialogTarget::None => {}
            DialogTarget::Rename(session) => self.chat_call(
                |reply| ChatCall::Rename {
                    session,
                    title: text.trim().to_string(),
                    reply,
                },
                |_, ()| {},
            ),
            DialogTarget::Edit { session, item } => self.chat_call(
                |reply| ChatCall::EditResend {
                    session,
                    item,
                    text,
                    context: None,
                    agent: None,
                    restore_files: check,
                    reply,
                },
                |_, ()| {},
            ),
            DialogTarget::Rewind { session, item } => self.chat_call(
                |reply| ChatCall::Rewind {
                    session,
                    item,
                    restore_files: check,
                    reply,
                },
                |_, result| crate::chat::restore_composer(&result.user_text, result.context),
            ),
            DialogTarget::Deny { session, request } => {
                self.deny_with_feedback(session, request, text);
            }
            DialogTarget::Delete(session) => self.delete_session(session),
            DialogTarget::NewGroup => self.chat_call(
                |reply| ChatCall::CreateGroup {
                    name: text.trim().to_string(),
                    color: GroupColor::Gray,
                    reply,
                },
                |s, _| s.load_groups(),
            ),
            DialogTarget::RenameGroup(id) => self.update_group(UpdateGroup {
                id,
                name: Some(text.trim().to_string()),
                color: None,
            }),
            DialogTarget::DeleteGroup(id) => {
                self.chat_call(
                    |reply| ChatCall::DeleteGroup(id, reply),
                    |s, ()| s.load_groups(),
                );
            }
            DialogTarget::NewSchedule => self.create_schedule(&text, &text2),
        }
    }

    // ---- permission: deny with feedback (R9) -----------------------------------------------------------

    fn deny_with_feedback(&self, session: SessionId, request: u64, text: String) {
        use harness_core_api::PermissionOptionKind as K;
        let option = self
            .perm_choices
            .iter()
            .find(|(_, kind)| *kind == K::RejectOnce)
            .or_else(|| {
                self.perm_choices
                    .iter()
                    .find(|(_, kind)| *kind == K::RejectAlways)
            });
        let Some((option_id, _)) = option else {
            // 选项尚未送达时无法拒绝（RespondPermission 兜底已移除）；保留文本，提示稍后再试。
            self.fail("Permission options are still loading");
            return;
        };
        self.send(Command::ChoosePermission {
            session: session.clone(),
            request,
            option_id: option_id.clone(),
        });
        // 拒绝后同一轮仍在运行：反馈作为下一条消息排队（CommandInbox 串行接收）。
        self.send(Command::Send {
            session,
            agent: self.sticky.clone(),
            text,
        });
    }

    // ---- sessions (R10 / R11) ------------------------------------------------------------------------

    pub(super) fn session_action(&mut self, action: &str, id: &str) {
        let Some(session) = self.target_session(id) else {
            return;
        };
        let title = self
            .summaries
            .iter()
            .find(|s| s.id == session)
            .map(|s| s.title.clone())
            .unwrap_or_default();
        match action {
            "rename" => self.open_dialog(DialogTarget::Rename(session), &title, "", false),
            "pin" | "unpin" => self.chat_call(
                |reply| ChatCall::SetPinned {
                    sessions: vec![session],
                    pinned: action == "pin",
                    reply,
                },
                |_, ()| {},
            ),
            "unread" => {
                self.turn.keep_unread = Some(session.clone());
                self.set_unread(&session, true);
            }
            "toggle-pin" => {
                let pinned = self.summaries.iter().any(|s| s.id == session && s.pinned);
                self.session_action(if pinned { "unpin" } else { "pin" }, &session);
            }
            "fork" => self.fork(session, None),
            "export" => self.export_session(session),
            "delete" => self.open_dialog(DialogTarget::Delete(session), "", "", false),
            "ungroup" => self.move_sessions(session, None),
            other => {
                if let Some(group) = other.strip_prefix("group:") {
                    self.move_sessions(session, Some(group.to_string()));
                }
            }
        }
    }

    fn move_sessions(&self, session: SessionId, group: Option<String>) {
        self.chat_call(
            |reply| ChatCall::MoveSessions {
                sessions: vec![session],
                group,
                reply,
            },
            |_, ()| {},
        );
    }

    /// Delete = archive, then delete the archived copy (hub keeps deletion behind the archive).
    fn delete_session(&self, session: SessionId) {
        let again = session.clone();
        self.request(
            |reply| Command::Settings(SettingsCall::ArchiveSessions(vec![session], reply)),
            move |s, ()| {
                s.request(
                    |reply| {
                        Command::Settings(SettingsCall::DeleteArchived(
                            SessionSelection::Sessions(vec![harness_core_api::SessionId::new(
                                again,
                            )]),
                            reply,
                        ))
                    },
                    |_, _count: u32| crate::settings::sessions_changed(),
                );
            },
        );
    }

    pub(super) fn load_groups(&self) {
        self.chat_call(ChatCall::ListGroups, |s, mut groups: Vec<SessionGroup>| {
            groups.sort_by_key(|g| g.position);
            s.turn.groups = groups;
            let rows: Vec<GroupRow> = s
                .turn
                .groups
                .iter()
                .map(|g| GroupRow {
                    id: sv(&g.id),
                    name: sv(&g.name),
                    color: sv(g.color.as_str()),
                })
                .collect();
            s.ui().set_groups(ModelRc::new(VecModel::from(rows)));
            s.refresh_sessions();
        });
    }

    fn update_group(&self, update: UpdateGroup) {
        self.chat_call(
            |reply| ChatCall::UpdateGroup(update, reply),
            |s, _| s.load_groups(),
        );
    }

    fn group_action(&mut self, action: &str, id: &str) {
        if action == "new-session" {
            if !id.is_empty() {
                self.new_session(false, Some(std::path::PathBuf::from(id)));
            }
            return;
        }
        let name = self
            .turn
            .groups
            .iter()
            .find(|g| g.id == id)
            .map(|g| g.name.clone())
            .unwrap_or_default();
        match action {
            "view" => self.refresh_sessions(),
            "new" => self.open_dialog(DialogTarget::NewGroup, "", "", false),
            "rename" => {
                self.open_dialog(DialogTarget::RenameGroup(id.to_string()), &name, "", false);
            }
            "delete" => self.open_dialog(DialogTarget::DeleteGroup(id.to_string()), "", "", false),
            "toggle" => {
                if !self.turn.collapsed.remove(id) {
                    self.turn.collapsed.insert(id.to_string());
                }
                self.refresh_sessions();
            }
            "up" | "down" => {
                let mut order: Vec<String> =
                    self.turn.groups.iter().map(|g| g.id.clone()).collect();
                let Some(from) = order.iter().position(|g| g == id) else {
                    return;
                };
                let to = if action == "up" {
                    from.checked_sub(1)
                } else {
                    Some(from + 1).filter(|&t| t < order.len())
                };
                if let Some(to) = to {
                    order.swap(from, to);
                    self.chat_call(
                        |reply| ChatCall::ReorderGroups(order, reply),
                        |s, ()| s.load_groups(),
                    );
                }
            }
            other => {
                if let Some(color) = other.strip_prefix("color:") {
                    self.update_group(UpdateGroup {
                        id: id.to_string(),
                        name: None,
                        color: Some(GroupColor::parse(color)),
                    });
                }
            }
        }
    }

    /// Drag a session onto a group header (or the session list): moves it there.
    fn drop_session(&self, session: &str, group: &str) {
        let group = (!group.is_empty()).then(|| group.to_string());
        self.move_sessions(session.to_string(), group);
    }

    /// Projects tree of the sidebar: the given sessions by `project` (workspace key) in first-seen order, each with
    /// its header row (folder name, session count; collapse is UI-local).
    fn project_sections<'a>(
        &self,
        sessions: Vec<&'a crate::core::SessionSummary>,
    ) -> Vec<(SessionRow, Vec<&'a crate::core::SessionSummary>)> {
        let mut projects: Vec<(&str, Vec<&crate::core::SessionSummary>)> = Vec::new();
        for s in sessions {
            match projects.iter_mut().find(|(key, _)| *key == s.project) {
                Some((_, list)) => list.push(s),
                None => projects.push((s.project.as_str(), vec![s])),
            }
        }
        projects
            .into_iter()
            .map(|(key, members)| {
                let id = format!("project:{key}");
                let folder = members
                    .iter()
                    .find(|s| s.branch.is_none() && !s.workspace_path.is_empty())
                    .map_or(key, |s| s.workspace_path.as_str());
                let name = folder
                    .rsplit(['/', '\\'])
                    .find(|part| !part.is_empty())
                    .unwrap_or(folder);
                let header = SessionRow {
                    header: sv("project"),
                    header_id: sv(&id),
                    header_label: sv(name),
                    header_color: sv(folder),
                    header_count: i32::try_from(members.len()).unwrap_or(i32::MAX),
                    collapsed: self.turn.collapsed.contains(&id),
                    ..SessionRow::default()
                };
                (header, members)
            })
            .collect()
    }

    /// Sidebar rows: Pinned, then the user groups (by position; collapsed ones show their header only), then the
    /// timeline (Today / Yesterday / …). Returns the rows and the displayed session order.
    pub(super) fn session_rows(&self) -> (Vec<SessionRow>, Vec<(String, String)>) {
        let mut rows = Vec::new();
        let mut order = Vec::new();
        let base = |s: &crate::core::SessionSummary| {
            let (_, unit, ago) = crate::chat::timeline(s.updated_ms);
            SessionRow {
                id: sv(&s.id),
                title: sv(&s.title),
                agents: sv(&s
                    .agents_used
                    .iter()
                    .map(|a| self.agent_name(a))
                    .collect::<Vec<_>>()
                    .join(" · ")),
                active: self.is_active(&s.id),
                ago_unit: sv(unit),
                ago_n: ago,
                pinned: s.pinned,
                unread: s.unread,
                worktree: s.branch.is_some(),
                ..SessionRow::default()
            }
        };
        let known_group = |g: &Option<String>| {
            g.as_ref()
                .is_some_and(|id| self.turn.groups.iter().any(|x| &x.id == id))
        };
        let mut push = |rows: &mut Vec<SessionRow>,
                        list: Vec<&crate::core::SessionSummary>,
                        header: SessionRow| {
            let mut first = Some(header);
            for s in list {
                let mut row = base(s);
                if let Some(h) = first.take() {
                    row.header = h.header;
                    row.header_id = h.header_id;
                    row.header_label = h.header_label;
                    row.header_color = h.header_color;
                    row.header_count = h.header_count;
                }
                order.push((s.id.clone(), s.title.clone()));
                rows.push(row);
            }
        };
        let pinned: Vec<_> = self.summaries.iter().filter(|s| s.pinned).collect();
        push(
            &mut rows,
            pinned,
            SessionRow {
                header: sv("pinned"),
                ..SessionRow::default()
            },
        );
        for group in &self.turn.groups {
            let collapsed = self.turn.collapsed.contains(&group.id);
            let header = SessionRow {
                header: sv("group"),
                header_id: sv(&group.id),
                header_label: sv(&group.name),
                header_color: sv(group.color.as_str()),
                collapsed,
                ..SessionRow::default()
            };
            let members: Vec<_> = self
                .summaries
                .iter()
                .filter(|s| !s.pinned && s.group_id.as_deref() == Some(group.id.as_str()))
                .collect();
            if collapsed || members.is_empty() {
                rows.push(header);
            } else {
                push(&mut rows, members, header);
            }
        }
        let rest: Vec<&crate::core::SessionSummary> = self
            .summaries
            .iter()
            .filter(|s| !s.pinned && !known_group(&s.group_id))
            .collect();
        if self.ui().get_by_project() {
            // 项目树：其余会话按 `project`（工作区 key）分组，组内保持 hub 的顺序；折叠状态是界面局部状态。
            for (header, members) in self.project_sections(rest) {
                if header.collapsed {
                    rows.push(header);
                } else {
                    push(&mut rows, members, header);
                }
            }
            return (rows, order);
        }
        let mut last_group = "";
        for s in rest {
            let (group, _, _) = crate::chat::timeline(s.updated_ms);
            let mut row = base(s);
            if group != last_group {
                row.header = sv(group);
            }
            last_group = group;
            order.push((s.id.clone(), s.title.clone()));
            rows.push(row);
        }
        (rows, order)
    }

    // ---- scheduled prompts (R18) -----------------------------------------------------------------------

    pub(super) fn load_schedules(&self) {
        self.chat_call(ChatCall::SchedulesEnabled, |s, on| {
            s.ui().set_schedules_enabled(on);
        });
        self.chat_call(ChatCall::ListSchedules, |s, list: Vec<ScheduledPrompt>| {
            let now = now_ms();
            let rows: Vec<ScheduleRow> = list.iter().map(|p| schedule_row(p, now)).collect();
            s.turn.schedules = list;
            s.ui().set_schedules(ModelRc::new(VecModel::from(rows)));
        });
    }

    fn create_schedule(&self, prompt: &str, minutes: &str) {
        let minutes: u32 = minutes.trim().parse().unwrap_or(60).max(1);
        let title: String = prompt
            .lines()
            .next()
            .unwrap_or_default()
            .trim()
            .chars()
            .take(40)
            .collect();
        let target = match self.active.clone() {
            Some(session) => ScheduleTarget::Session {
                session_id: harness_core_api::SessionId::new(session),
            },
            None => ScheduleTarget::NewSession {
                workspace_path: self.workspace.to_string_lossy().into_owned(),
                agent: (!self.sticky.is_empty())
                    .then(|| harness_core_api::AgentId::new(self.sticky.as_str())),
            },
        };
        let schedule = NewSchedule {
            title,
            prompt: prompt.trim().to_string(),
            target,
            kind: ScheduleKind::Interval {
                seconds: minutes.saturating_mul(60),
            },
            enabled: true,
        };
        self.chat_call(
            |reply| ChatCall::CreateSchedule(schedule, reply),
            |s, _| s.load_schedules(),
        );
    }

    pub(super) fn schedule_action(&mut self, action: &str, id: &str) {
        let id = id.to_string();
        match action {
            "open" => {
                self.ui().set_schedules_open(true);
                self.load_schedules();
            }
            "enable" | "disable" => {
                let on = action == "enable";
                self.chat_call(
                    |reply| ChatCall::SetSchedulesEnabled(on, reply),
                    |s, ()| s.load_schedules(),
                );
            }
            "new" => self.open_dialog(DialogTarget::NewSchedule, "", "60", true),
            "toggle" => {
                let enabled = self
                    .turn
                    .schedules
                    .iter()
                    .find(|p| p.id == id)
                    .is_some_and(|p| !p.enabled);
                let update = UpdateSchedule {
                    id,
                    title: None,
                    prompt: None,
                    target: None,
                    kind: None,
                    enabled: Some(enabled),
                };
                self.chat_call(
                    |reply| ChatCall::UpdateSchedule(update, reply),
                    |s, _| s.load_schedules(),
                );
            }
            "run" => self.chat_call(
                |reply| ChatCall::RunScheduleNow(id, reply),
                |s, run| {
                    crate::chat::toast_label("scheduleStarted");
                    s.load_schedules();
                    s.send(Command::OpenSession {
                        session: run.session_id.to_string(),
                    });
                },
            ),
            "delete" => self.chat_call(
                |reply| ChatCall::DeleteSchedule(id, reply),
                |s, ()| s.load_schedules(),
            ),
            _ => {}
        }
    }
}

/// Wires the `ChatUi` callbacks of Phase 9.2 R8–R18.
pub(super) fn wire(window: &crate::ui::AppWindow) {
    let ui = window.global::<ChatUi>();
    ui.on_toggle_detail(|id| with_state(|s| s.toggle_detail(id)));
    ui.on_turn_action(|action, id| with_state(|s| s.turn_action(&action, id)));
    ui.on_session_action(|action, id| with_state(|s| s.session_action(&action, &id)));
    ui.on_group_action(|action, id| with_state(|s| s.group_action(&action, &id)));
    ui.on_drop_session(|session, group| with_state(|s| s.drop_session(&session, &group)));
    ui.on_schedule_action(|action, id| with_state(|s| s.schedule_action(&action, &id)));
    ui.on_subagent_action(|action, id| with_state(|s| s.subagent_action(&action, &id)));
    ui.on_stop_background_task(|id| with_state(|s| s.stop_background_task(&id)));
    ui.on_usage_action(|action| match action.as_str() {
        "toggleAutoCompact" => crate::settings::run_command("toggleAutoCompact"),
        "refreshPlan" => crate::plan_usage::refresh(),
        other => crate::chat::command(other),
    });
    ui.on_dialog_submit(|| with_state(UiState::dialog_submit));
    ui.on_deny_feedback(|request| {
        with_state(|s| {
            if let (Some(session), Ok(request)) = (s.active.clone(), u64::try_from(request)) {
                s.open_dialog(DialogTarget::Deny { session, request }, "", "", true);
            }
        });
    });
    window.on_session_pinned(|id| {
        let mut pinned = false;
        with_state(|s| {
            pinned = s.summaries.iter().any(|x| x.id == id.as_str() && x.pinned);
        });
        pinned
    });
}
