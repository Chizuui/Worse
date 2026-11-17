//! UI-thread side: owns the Slint models, mirrors core events, turns callbacks into core commands.
//!
//! Lives in a `thread_local` on the UI thread; the core thread reaches it only through
//! `slint::invoke_from_event_loop` (spec §3 "Threading").

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;

use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel};
use tokio::sync::mpsc;

use crate::core::{
    AgentId, AgentState, AgentStatus, Command, Event, Item, ItemId, Notice, QueuedMessage,
    SessionId, SessionSummary, Settings, SettingsCall, ToolStatus, TurnStatus,
};
use crate::files::{icon_index, Entries, FileReq, ListDir};
use crate::markdown;
use crate::ui::{
    AgentRow, AppWindow, ChatItem, FallbackRow, FileRow, MdBlock, PanelNav, PermOption, QueueRow,
    SessionRow, Theme,
};
use harness_core_api::{ContextItem, PermissionOptionKind};

mod board;
mod dock;
mod export;
mod fleet;
mod thumbs;
mod turns;

struct UiState {
    window: AppWindow,
    cmd: mpsc::UnboundedSender<Command>,
    files_tx: mpsc::UnboundedSender<FileReq>,
    sessions: Rc<VecModel<SessionRow>>,
    agents: Rc<VecModel<AgentRow>>,
    fallback: Rc<VecModel<FallbackRow>>,
    chat: Rc<VecModel<ChatItem>>,
    files: Rc<VecModel<FileRow>>,
    queue: Rc<VecModel<QueueRow>>,
    file_paths: Vec<PathBuf>,
    active: Option<SessionId>,
    /// Title from the open session's own summary: a subagent session (§9.8) is not in `summaries`.
    opened_title: String,
    rows: HashMap<ItemId, usize>,
    texts: HashMap<ItemId, String>,
    blocks: HashMap<ItemId, Rc<VecModel<MdBlock>>>,
    agent_list: Vec<AgentStatus>,
    disabled: Vec<AgentId>,
    settings: Settings,
    summaries: Vec<SessionSummary>,
    sticky: AgentId,
    /// Message typed before any session existed: sent once the created session opens.
    pending_send: Option<(String, Vec<ContextItem>)>,
    workspace: PathBuf,
    /// Phase 9.2 view state (thinking rows, groups, schedules, change plans): `bridge/turns.rs`.
    turn: turns::TurnState,
    /// Options of the shown permission request (`Event::PermissionChoices`), in the agent's order.
    perm_choices: Vec<(String, PermissionOptionKind)>,
    /// Round 3 dock state (queue edit, effort option id, failure banner): `bridge/dock.rs`.
    dock_state: dock::DockState,
    board: board::BoardState,
    thumbs: thumbs::Thumbs,
}

thread_local! {
    static STATE: RefCell<Option<UiState>> = const { RefCell::new(None) };
}

fn with_state(f: impl FnOnce(&mut UiState)) {
    STATE.with(|cell| {
        if let Ok(mut guard) = cell.try_borrow_mut() {
            if let Some(state) = guard.as_mut() {
                f(state);
            }
        }
    });
}

pub fn set_disabled_agents(ids: Vec<AgentId>) {
    with_state(|s| {
        if s.disabled != ids {
            s.disabled = ids;
            s.refresh_agents();
        }
    });
}

fn turn_status(status: TurnStatus) -> i32 {
    match status {
        TurnStatus::Streaming => 0,
        TurnStatus::Done => 1,
        TurnStatus::Cancelled => 2,
        TurnStatus::Interrupted => 3,
        TurnStatus::Failed => 4,
    }
}

fn tool_status(status: ToolStatus) -> i32 {
    match status {
        ToolStatus::Running => 0,
        ToolStatus::AwaitingPermission => 1,
        ToolStatus::Done => 2,
        ToolStatus::Denied => 3,
        ToolStatus::Failed => 4,
    }
}

fn item_id(id: ItemId) -> i32 {
    i32::try_from(id).unwrap_or(i32::MAX)
}

impl UiState {
    fn send(&self, command: Command) {
        // core 线程已退出只会发生在关停阶段，此时丢弃命令是正确行为。
        let _ = self.cmd.send(command);
    }

    fn agent_name(&self, id: &str) -> String {
        self.agent_list
            .iter()
            .find(|a| a.id == id)
            .map_or_else(|| id.to_string(), |a| a.name.clone())
    }

    fn agent_row(agent: &AgentStatus) -> AgentRow {
        let (state, reset) = match &agent.state {
            AgentState::Available => (0, String::new()),
            AgentState::Unavailable => (1, String::new()),
            AgentState::AdapterMissing => (3, String::new()),
            AgentState::Exhausted { reset } => (2, reset.clone().unwrap_or_default()),
        };
        AgentRow {
            id: agent.id.as_str().into(),
            name: agent.name.as_str().into(),
            state,
            reset: reset.into(),
            claude: agent.id.as_str() == "claude-code"
                || agent.id.as_str().starts_with("claude-code-"),
        }
    }

    fn full_order(&self) -> Vec<AgentId> {
        let mut order: Vec<AgentId> = self
            .settings
            .fallback_order
            .iter()
            .filter(|id| self.agent_list.iter().any(|a| &a.id == *id))
            .cloned()
            .collect();
        for agent in &self.agent_list {
            if !order.contains(&agent.id) {
                order.push(agent.id.clone());
            }
        }
        order
    }

    fn refresh_agents(&mut self) {
        // 选择器与设置页都按 fallback 顺序展示，未出现在顺序里的 agent 追加在末尾。
        let mut ordered: Vec<&AgentStatus> = self
            .settings
            .fallback_order
            .iter()
            .filter_map(|id| self.agent_list.iter().find(|a| &a.id == id))
            .collect();
        for agent in &self.agent_list {
            if !ordered.iter().any(|a| a.id == agent.id) {
                ordered.push(agent);
            }
        }
        let rows: Vec<AgentRow> = ordered
            .into_iter()
            .filter(|a| !self.disabled.contains(&a.id))
            .map(Self::agent_row)
            .collect();
        if self.active.is_none() && self.disabled.contains(&self.sticky) {
            if let Some(first) = rows.iter().find(|r| r.state == 0) {
                self.sticky = first.id.to_string();
            }
        }
        self.fallback.set_vec(
            rows.iter()
                .map(|r| FallbackRow {
                    id: r.id.clone(),
                    name: r.name.clone(),
                    state: r.state,
                })
                .collect::<Vec<_>>(),
        );
        self.agents.set_vec(
            rows.into_iter()
                .filter(|r| r.state == 0 || r.state == 2)
                .collect::<Vec<_>>(),
        );
        self.window.set_sticky_agent(self.sticky.as_str().into());
        self.window
            .set_sticky_name(self.agent_name(&self.sticky).into());
    }

    fn refresh_sessions(&mut self) {
        // 侧栏分区（置顶、分组、时间线）由 turns::session_rows 生成；上一个/下一个会话按显示顺序。
        let (rows, order): (Vec<SessionRow>, _) = self.session_rows();
        self.sessions.set_vec(rows);
        crate::chat::sessions_changed(order, self.active.as_deref());
        let title = self
            .active
            .as_ref()
            .and_then(|id| self.summaries.iter().find(|s| &s.id == id))
            .map(|s| s.title.clone())
            .or_else(|| self.active.as_ref().map(|_| self.opened_title.clone()))
            .unwrap_or_default();
        self.window.set_active_title(title.into());
    }

    fn chat_item(&mut self, id: ItemId, item: &Item) -> ChatItem {
        let mut row = ChatItem {
            id: item_id(id),
            ..ChatItem::default()
        };
        match item {
            Item::User {
                text,
                background: true,
                ..
            } => {
                // Autonomous turn (§9.17): one muted line with a refresh icon, not a user bubble.
                row.kind = 6;
                row.text = text.as_str().into();
            }
            Item::User {
                text,
                background: false,
                images,
            } => {
                row.kind = 0;
                row.text = text.as_str().into();
                (row.images, row.images_more) = self.row_images(id, images);
            }
            Item::Assistant {
                agent,
                text,
                status,
            } => {
                row.kind = 1;
                row.agent = self.agent_name(agent).into();
                row.text = text.as_str().into();
                row.status = turn_status(*status);
                let model = Rc::new(VecModel::from(markdown::render(text)));
                row.blocks = ModelRc::from(model.clone());
                self.blocks.insert(id, model);
                self.texts.insert(id, text.clone());
            }
            Item::Tool { title, status, .. } => {
                row.kind = 2;
                row.title = one_line(title).into();
                row.status = tool_status(*status);
                let image: Vec<String> = thumbs::tool_image(title, &self.workspace)
                    .into_iter()
                    .collect();
                (row.images, row.images_more) = self.row_images(id, &image);
            }
            Item::Notice {
                notice: Notice::Failover { from, to, reset },
            } => {
                row.kind = 3;
                row.from = self.agent_name(from).into();
                row.to = self.agent_name(to).into();
                row.reset = reset.clone().unwrap_or_default().into();
            }
            Item::Notice {
                notice: Notice::Paused { reset },
            } => {
                row.kind = 4;
                row.reset = reset.clone().unwrap_or_default().into();
            }
        }
        row
    }

    fn push_item(&mut self, id: ItemId, item: &Item) {
        if matches!(
            item,
            Item::User {
                background: false,
                ..
            }
        ) {
            crate::live::new_turn(&self.window);
        }
        self.note_item(id, item);
        let row = self.chat_item(id, item);
        self.rows.insert(id, self.chat.row_count());
        self.chat.push(row);
    }

    fn update_row(&self, id: ItemId, f: impl FnOnce(&mut ChatItem)) {
        let Some(&index) = self.rows.get(&id) else {
            return;
        };
        if let Some(mut row) = self.chat.row_data(index) {
            f(&mut row);
            self.chat.set_row_data(index, row);
        }
    }

    fn append_text(&mut self, id: ItemId, delta: &str) {
        let Some(text) = self.texts.get_mut(&id) else {
            return;
        };
        text.push_str(delta);
        let full = text.clone();
        if let Some(model) = self.blocks.get(&id) {
            // 只更新变化的块：流式时通常只有最后一块变化，Slint 只重绘该块。
            let next = markdown::render(&full);
            for (index, block) in next.iter().enumerate() {
                match model.row_data(index) {
                    Some(existing) if existing == *block => {}
                    Some(_) => model.set_row_data(index, block.clone()),
                    None => model.push(block.clone()),
                }
            }
            while model.row_count() > next.len() {
                model.remove(model.row_count() - 1);
            }
        }
        self.update_row(id, |row| row.text = full.as_str().into());
    }

    fn open_session(
        &mut self,
        summary: &SessionSummary,
        items: &[(ItemId, Item)],
        sticky: AgentId,
        queued: &[QueuedMessage],
        paused: bool,
    ) {
        if self.turn.keep_unread.as_deref() != Some(summary.id.as_str()) {
            self.turn.keep_unread = None;
        }
        self.active = Some(summary.id.clone());
        self.opened_title.clone_from(&summary.title);
        self.set_workspace(&summary.workspace_path);
        if let Some(existing) = self.summaries.iter_mut().find(|s| s.id == summary.id) {
            *existing = summary.clone();
        }
        self.rows.clear();
        self.thumbs.clear_rows();
        self.texts.clear();
        self.blocks.clear();
        self.chat.set_vec(Vec::new());
        self.reset_turn_view();
        for (id, item) in items {
            self.push_item(*id, item);
        }
        self.sticky = sticky;
        self.set_queue(queued);
        self.window.set_paused(paused);
        self.window.set_perm_visible(false);
        self.refresh_agents();
        self.refresh_sessions();
        self.session_opened(items, summary.unread);
        self.reset_dock();
        if let Some((text, context)) = self.pending_send.take() {
            self.send_message(summary.id.clone(), text, context);
        }
    }

    /// 修复：工作区只在启动时设置一次，打开其它文件夹的会话后右侧路径、左下角项目名、文件树和 Git/终端面板
    /// 仍停在旧文件夹；现在跟随打开的会话（空路径 = 保持当前工作区）。
    fn set_workspace(&mut self, path: &str) {
        let path = PathBuf::from(path);
        // 与界面上显示的工作区比较：启动时界面为空，第一次打开会话时必须显示。
        if path.as_os_str().is_empty()
            || self.window.get_workspace().as_str() == path.to_string_lossy()
        {
            return;
        }
        self.workspace.clone_from(&path);
        show_workspace(&self.window, &path);
        let _ = self.files_tx.send(FileReq::List(ListDir {
            dir: path.clone(),
            depth: 0,
        }));
        crate::chat::set_workspace(&path);
        crate::live::set_workspace(&path);
        crate::panels::set_workspace(path);
    }

    /// New session on the composer's current agent: passing it in `CreateSession` keeps a choice made before any
    /// session existed (`SetStickyAgent` needs a session) from being replaced by the hub's default order.
    /// `workspace` 为该会话指定工作区（“Open folder…”，None = 启动时的工作区）。
    fn new_session(&mut self, worktree: bool, workspace: Option<PathBuf>) {
        let agent = self.sticky.clone();
        if worktree {
            self.send(Command::CreateWorktreeSession { agent, workspace });
        } else {
            self.send(Command::CreateSession { agent, workspace });
        }
    }

    /// Plain text goes as `Send`; with files, images or sessions attached as `SendContext` (spec §9.2.1).
    fn send_message(&self, session: SessionId, text: String, context: Vec<ContextItem>) {
        let agent = self.sticky.clone();
        if context.is_empty() {
            self.send(Command::Send {
                session,
                agent,
                text,
            });
        } else {
            self.send(Command::SendContext {
                session,
                agent,
                text,
                context,
            });
        }
    }

    /// Leaves the open session (it was archived or deleted): empty chat, nothing queued.
    fn clear_session(&mut self) {
        self.active = None;
        self.turn.keep_unread = None;
        self.reset_turn_view();
        self.rows.clear();
        self.thumbs.clear_rows();
        self.texts.clear();
        self.blocks.clear();
        self.chat.set_vec(Vec::new());
        self.set_queue(&[]);
        self.window.set_paused(false);
        self.window.set_running(false);
        self.window.set_perm_visible(false);
        self.reset_dock();
    }

    fn set_perm_choices(&mut self, options: Vec<(String, String, PermissionOptionKind)>) {
        let rows: Vec<PermOption> = options
            .iter()
            .map(|(id, label, kind)| PermOption {
                id: id.as_str().into(),
                label: label.as_str().into(),
                kind: match kind {
                    PermissionOptionKind::AllowOnce => 0,
                    PermissionOptionKind::AllowAlways => 1,
                    PermissionOptionKind::RejectOnce => 2,
                    PermissionOptionKind::RejectAlways => 3,
                },
            })
            .collect();
        self.window
            .set_perm_options(ModelRc::new(VecModel::from(rows)));
        self.perm_choices = options
            .into_iter()
            .map(|(id, _, kind)| (id, kind))
            .collect();
    }

    /// Approve / deny (buttons, keys 1/2, Esc): the agent's own "once" option when it offered one, else its
    /// "always" one. Before the options arrive nothing is sent.
    fn answer_permission(&self, request: u64, allow: bool) {
        let Some(session) = self.active.clone() else {
            return;
        };
        let (once, always) = if allow {
            (
                PermissionOptionKind::AllowOnce,
                PermissionOptionKind::AllowAlways,
            )
        } else {
            (
                PermissionOptionKind::RejectOnce,
                PermissionOptionKind::RejectAlways,
            )
        };
        let pick = self
            .perm_choices
            .iter()
            .find(|(_, kind)| *kind == once)
            .or_else(|| self.perm_choices.iter().find(|(_, kind)| *kind == always));
        // 修复（R9）：旧的 RespondPermission 兜底按“第一个 allow/reject 选项”猜测，可能把 always 当成 once；
        // A 的核心已对每个请求先发 PermissionChoices，选项到达前卡片显示“正在加载选项”，这里不再猜测。
        if let Some((option_id, _)) = pick {
            self.send(Command::ChoosePermission {
                session,
                request,
                option_id: option_id.clone(),
            });
        }
    }

    fn set_queue(&self, items: &[QueuedMessage]) {
        self.queue.set_vec(
            items
                .iter()
                .map(|q| QueueRow {
                    id: q.id.as_str().into(),
                    text: q.text.as_str().into(),
                })
                .collect::<Vec<_>>(),
        );
        self.window
            .set_queued(i32::try_from(items.len()).unwrap_or(i32::MAX));
    }

    fn is_active(&self, session: &str) -> bool {
        self.active.as_deref() == Some(session)
    }

    /// Theme, language and zoom are applied by `crate::settings` from the store's own snapshots (Phase 9.1);
    /// the core's settings event only drives the fallback order here.
    fn apply_settings(&mut self, settings: Settings) {
        self.settings = settings;
        self.refresh_agents();
    }

    fn apply_agents(&mut self, list: Vec<AgentStatus>) {
        crate::settings::agents_changed(
            list.iter()
                .map(|a| {
                    (
                        a.id.clone(),
                        a.name.clone(),
                        !matches!(
                            a.state,
                            AgentState::Unavailable | AgentState::AdapterMissing
                        ),
                    )
                })
                .collect(),
        );
        self.agent_list = list;
        if self.sticky.is_empty() {
            self.sticky = self
                .settings
                .fallback_order
                .iter()
                .find(|id| {
                    self.agent_list
                        .iter()
                        .any(|a| &&a.id == id && a.state == AgentState::Available)
                })
                .cloned()
                .unwrap_or_default();
        }
        self.refresh_agents();
        self.refresh_sessions();
    }

    // One flat match over the event kinds; each arm is short.
    #[allow(clippy::too_many_lines)]
    fn apply(&mut self, event: Event) {
        match event {
            Event::Settings(settings) => self.apply_settings(settings),
            Event::Agents(list) => self.apply_agents(list),
            Event::AgentOptions {
                agent,
                modes,
                models,
                efforts,
                known,
            } => {
                // 未知（agent 尚未启动过）时列表为空，不覆盖已有的选项。
                if known {
                    crate::settings::agent_options(&agent, modes, models, efforts);
                }
            }
            Event::AgentsOverview(rows) => crate::settings::agents_overview(rows),
            Event::AgentTested(result) => crate::settings::agent_tested(result),
            Event::Accounts(result) => {
                crate::plan_usage::accounts(&result);
                crate::settings::accounts(result);
            }
            Event::Providers(result) => crate::settings::providers(result),
            Event::AgyAccounts { sign_in, result } => {
                crate::plan_usage::agy_accounts(&result);
                crate::settings::agy::accounts(sign_in.as_deref(), result);
            }
            Event::SetupStatus(result) => crate::setup::status(result),
            Event::AdapterInstall {
                agent,
                status,
                line,
                message,
            } => {
                if agent == "gh" {
                    crate::inbox::gh_install_event(status, message.clone());
                }
                crate::setup::install_event(&agent, status, line, message);
            }
            Event::Sessions(list) => {
                crate::settings::sessions_changed();
                // 当前会话被归档或删除后不再出现在列表里；hub 不会替界面关闭它，这里清空视图。
                // 子代理会话本来就不在列表中（§9.8，list_sessions 隐藏子会话）：打开它时不能当作已删除而清空。
                let subagent_open = !self.ui().get_parent_id().is_empty();
                if !subagent_open
                    && self.active.as_ref().is_some_and(|id| {
                        self.summaries.iter().any(|s| &s.id == id)
                            && !list.iter().any(|s| &s.id == id)
                    })
                {
                    self.clear_session();
                }
                self.summaries = list;
                self.refresh_sessions();
                // 打开着的会话不应显示未读：hub 在回合结束时标记未读，这里立即清除。
                // 修复：用户在打开的会话上“标为未读”后，下一次 Sessions 事件会立刻把它清掉；
                // 用户主动标记的会话在切换到别的会话之前保持未读。
                if let Some(active) = self.active.clone() {
                    let kept = self.turn.keep_unread.as_deref() == Some(active.as_str());
                    if !kept && self.summaries.iter().any(|s| s.id == active && s.unread) {
                        self.set_unread(&active, false);
                    }
                }
            }
            Event::SessionOpened {
                session,
                items,
                sticky,
                queued,
                paused,
            } => self.open_session(&session, &items, sticky, &queued, paused),
            Event::ItemAdded { session, id, item } => {
                if self.is_active(&session) {
                    self.push_item(id, &item);
                }
            }
            Event::TextDelta { session, id, text } => {
                if self.is_active(&session) {
                    self.append_text(id, &text);
                }
            }
            Event::TurnEnded {
                session,
                id,
                status,
            } => {
                if self.is_active(&session) {
                    self.update_row(id, |row| row.status = turn_status(status));
                    self.turn_ended(id);
                }
            }
            Event::ToolUpdated {
                session,
                id,
                status,
            } => {
                if self.is_active(&session) {
                    self.update_row(id, |row| row.status = tool_status(status));
                }
            }
            Event::ToolTitle { session, id, title } => {
                let changed = self.is_active(&session)
                    && self
                        .rows
                        .get(&id)
                        .and_then(|&index| self.chat.row_data(index))
                        .is_some_and(|row| row.title.as_str() != one_line(&title));
                if changed {
                    let image: Vec<String> = thumbs::tool_image(&title, &self.workspace)
                        .into_iter()
                        .collect();
                    let (images, more) = self.row_images(id, &image);
                    self.update_row(id, |row| {
                        row.title = one_line(&title).into();
                        row.images = images;
                        row.images_more = more;
                    });
                }
            }
            Event::PermissionRequested {
                session,
                request,
                title,
                detail,
            } => {
                if self.is_active(&session) {
                    self.window
                        .set_perm_request(i32::try_from(request).unwrap_or(i32::MAX));
                    self.window.set_perm_title(title.into());
                    self.window.set_perm_detail(detail.into());
                    // 选项随后由 PermissionChoices 送达；先清掉上一个请求的选项，避免把旧 id 回答给新请求。
                    self.set_perm_choices(Vec::new());
                    self.window.set_perm_visible(true);
                }
            }
            Event::PermissionChoices {
                session,
                request,
                options,
            } => {
                if self.is_active(&session)
                    && i64::from(self.window.get_perm_request())
                        == i64::try_from(request).unwrap_or(-1)
                {
                    self.set_perm_choices(
                        options
                            .into_iter()
                            .map(|o| (o.id, o.name, o.kind))
                            .collect(),
                    );
                }
            }
            Event::PermissionCleared { session, request } => {
                if self.is_active(&session)
                    && i64::from(self.window.get_perm_request())
                        == i64::try_from(request).unwrap_or(-1)
                {
                    self.window.set_perm_visible(false);
                }
            }
            Event::Sticky { session, agent } => {
                if self.is_active(&session) {
                    self.sticky = agent;
                    self.refresh_agents();
                    self.load_commands();
                    self.load_skills();
                }
            }
            Event::Running { session, running } => {
                if self.is_active(&session) {
                    self.window.set_running(running);
                    // 新回合开始：上一次失败的横幅过时了。
                    if running {
                        self.dismiss_failure();
                    }
                }
            }
            Event::Queue { session, items } => {
                if self.is_active(&session) {
                    self.set_queue(&items);
                    self.queue_changed(&items);
                }
            }
            Event::Paused { session, paused } => {
                if self.is_active(&session) {
                    self.window.set_paused(paused);
                }
            }
            Event::Error(message) => {
                if !self.failure_repeats(&message) {
                    self.window.set_error_text(message.into());
                }
            }
            Event::ToolDetail {
                session,
                id,
                kind,
                content,
                locations,
                raw_input,
                raw_output,
            } => {
                if self.is_active(&session) {
                    self.tool_detail(
                        id,
                        kind.as_deref(),
                        &content,
                        &locations,
                        raw_input.as_deref(),
                        raw_output.as_deref(),
                    );
                    // Only a running turn moves the Live view (opening a session replays every tool call).
                    if self.window.get_running() {
                        crate::live::touch(&self.window, kind.as_deref(), &content, &locations);
                    }
                }
            }
            Event::Thought { session, text } => {
                if self.is_active(&session) {
                    self.thought(&text);
                }
            }
            Event::Plan { session, entries } => {
                if self.is_active(&session) {
                    self.plan(&entries);
                }
            }
            Event::Subagents { session, items } => {
                if self.is_active(&session) {
                    self.subagents(&items);
                }
            }
            Event::Fleet(list) => self.fleet(&list),
            Event::BackgroundTasks { session, tasks } => {
                if self.is_active(&session) {
                    self.background_tasks(&tasks);
                }
            }
            Event::ParentLink { session, parent } => {
                if self.is_active(&session) {
                    self.parent_link(parent);
                }
            }
            Event::SessionMeta {
                session,
                worktree,
                cost,
            } => {
                if self.is_active(&session) {
                    self.session_meta(worktree.as_ref(), cost.as_ref());
                }
            }
            Event::Usage { session, usage } => {
                if self.is_active(&session) {
                    self.usage(&usage);
                }
            }
            Event::Commands { agent } => {
                if agent == self.sticky {
                    self.load_commands();
                }
            }
            Event::Groups => self.load_groups(),
            Event::Board { workspace_key } => self.board_event(&workspace_key),
            Event::Inbox => crate::inbox::changed(),
            Event::Schedules => {
                if self
                    .window
                    .global::<crate::ui::ChatUi>()
                    .get_schedules_open()
                {
                    self.load_schedules();
                }
            }
            Event::FilesRestored { session, item, .. } => self.files_restored(&session, item),
            Event::SessionOptions {
                session,
                mode,
                model,
                effort,
                effort_id,
                model_id,
                modes,
                models,
                efforts,
                pending,
                locked,
                machine,
                ..
            } => {
                if self.is_active(&session) {
                    self.apply_options(Some(dock::Options {
                        mode,
                        model,
                        effort,
                        effort_id,
                        model_id,
                        modes,
                        models,
                        efforts,
                        pending,
                        locked,
                        machine,
                    }));
                }
            }
            Event::Failure {
                session,
                kind,
                message,
                details,
                trace,
                retry,
                ..
            } => self.failure(dock::FailureEvent {
                session,
                kind,
                message,
                details,
                trace,
                retry,
            }),
        }
    }

    fn toggle_file(&mut self, index: usize) {
        let Some(row) = self.files.row_data(index) else {
            return;
        };
        if !row.is_dir {
            return;
        }
        if row.expanded {
            let mut end = index + 1;
            while end < self.files.row_count()
                && self
                    .files
                    .row_data(end)
                    .is_some_and(|r| r.depth > row.depth)
            {
                end += 1;
            }
            for remove in (index + 1..end).rev() {
                self.files.remove(remove);
                self.file_paths.remove(remove);
            }
            self.files.set_row_data(
                index,
                FileRow {
                    expanded: false,
                    ..row
                },
            );
        } else if let Some(path) = self.file_paths.get(index) {
            let _ = self.files_tx.send(FileReq::List(ListDir {
                dir: path.clone(),
                depth: row.depth + 1,
            }));
        }
    }
}

/// Applies a batch of core events on the UI thread.
pub fn apply(events: Vec<Event>) {
    with_state(|state| {
        for event in events {
            state.apply(event);
        }
    });
}

/// File listing reply (UI thread).
pub fn files_listed(dir: &Path, depth: i32, entries: Entries) {
    with_state(|state| {
        let (insert_at, parent) = if depth == 0 {
            state.files.set_vec(Vec::new());
            state.file_paths.clear();
            (0, None)
        } else {
            // 父目录可能在请求期间被折叠或移动，按路径重新定位，找不到就丢弃结果。
            let Some(parent) = state.file_paths.iter().position(|p| p == dir) else {
                return;
            };
            if state.files.row_data(parent).is_some_and(|r| r.expanded) {
                return;
            }
            (parent + 1, Some(parent))
        };
        for (offset, (name, is_dir)) in entries.into_iter().enumerate() {
            state.files.insert(
                insert_at + offset,
                FileRow {
                    name: name.as_str().into(),
                    depth,
                    is_dir,
                    expanded: false,
                    icon: icon_index(&name, is_dir),
                },
            );
            state.file_paths.insert(insert_at + offset, dir.join(&name));
        }
        if let Some(parent) = parent {
            if let Some(row) = state.files.row_data(parent) {
                state.files.set_row_data(
                    parent,
                    FileRow {
                        expanded: true,
                        ..row
                    },
                );
            }
        }
    });
}

/// Installs the UI state and wires every window callback to a core command.
/// System UI font with Simplified Chinese glyphs. Windows' default fallback picks a Japanese font for Han
/// characters, which lacks simplified-only forms; an unknown family falls back to Slint's default.
fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

pub(crate) fn ui_font_family(zh: bool, choice: &str) -> &'static str {
    if !zh && choice == "inter" {
        "Inter"
    } else if !zh && choice != "system" {
        "Plus Jakarta Sans"
    } else if cfg!(windows) {
        if zh {
            "Microsoft YaHei UI"
        } else {
            "Segoe UI"
        }
    } else if cfg!(target_os = "macos") {
        "PingFang SC"
    } else {
        "Noto Sans CJK SC"
    }
}

/// Workspace path, name and initial shown in the side pane, the project chip and the title bar.
fn show_workspace(window: &AppWindow, workspace: &Path) {
    window.set_workspace(SharedString::from(workspace.to_string_lossy().as_ref()));
    let name = workspace
        .file_name()
        .map_or_else(|| workspace.to_string_lossy(), |n| n.to_string_lossy());
    window.set_workspace_name(SharedString::from(name.as_ref()));
    window.set_workspace_initial(SharedString::from(
        name.chars()
            .next()
            .map(|c| c.to_uppercase().collect::<String>())
            .unwrap_or_else(|| "H".to_owned()),
    ));
}

pub fn install(
    window: &AppWindow,
    cmd: mpsc::UnboundedSender<Command>,
    files_tx: mpsc::UnboundedSender<FileReq>,
    workspace: &Path,
) {
    let sessions = Rc::new(VecModel::default());
    let agents = Rc::new(VecModel::default());
    let fallback = Rc::new(VecModel::default());
    let chat = Rc::new(VecModel::default());
    let files = Rc::new(VecModel::default());
    let queue = Rc::new(VecModel::default());
    window.set_sessions(ModelRc::from(sessions.clone()));
    window.set_agents(ModelRc::from(agents.clone()));
    window.set_fallback(ModelRc::from(fallback.clone()));
    window.set_chat(ModelRc::from(chat.clone()));
    window.set_files(ModelRc::from(files.clone()));
    window.set_queue(ModelRc::from(queue.clone()));
    // 修复：启动时不显示上次的工作区（名称/路径），直到打开会话或文件夹为止。
    show_workspace(window, Path::new(""));
    let mono = if cfg!(target_os = "windows") {
        "Consolas"
    } else if cfg!(target_os = "macos") {
        "Menlo"
    } else {
        "DejaVu Sans Mono"
    };
    window.global::<Theme>().set_mono_font(mono.into());
    // Agent A's panels ask the side pane to switch tabs (a git change row opens the Diff tab).
    let weak = window.as_weak();
    crate::live::install(window);
    crate::wallpaper::install(window);
    crate::live::set_workspace(workspace);
    window.global::<PanelNav>().on_open_tab(move |id| {
        if let Some(window) = weak.upgrade() {
            window.set_side_tab(id);
        }
    });
    let state = UiState {
        window: window.clone_strong(),
        cmd,
        files_tx,
        sessions,
        agents,
        fallback,
        chat,
        files,
        queue,
        file_paths: Vec::new(),
        active: None,
        opened_title: String::new(),
        rows: HashMap::new(),
        thumbs: thumbs::Thumbs::default(),
        texts: HashMap::new(),
        blocks: HashMap::new(),
        agent_list: Vec::new(),
        disabled: Vec::new(),
        settings: Settings::default(),
        summaries: Vec::new(),
        sticky: String::new(),
        pending_send: None,
        perm_choices: Vec::new(),
        dock_state: dock::DockState::default(),
        workspace: workspace.to_path_buf(),
        turn: turns::TurnState::default(),
        board: board::BoardState::default(),
    };
    STATE.with(|cell| {
        if let Ok(mut guard) = cell.try_borrow_mut() {
            *guard = Some(state);
        }
    });

    wire_callbacks(window);
    install_open_folder(window);
    turns::wire(window);
    dock::wire(window);
    board::wire(window);
    with_state(|s| s.load_groups());
}

fn wire_callbacks(window: &AppWindow) {
    window.on_new_session(|| with_state(|s| s.new_session(false, None)));
    window.on_new_worktree_session(|| with_state(|s| s.new_session(true, None)));
    window.on_archive_session(|id| archive_session(id.to_string()));
    window.on_open_session(|id| {
        with_state(|s| {
            s.send(Command::OpenSession {
                session: id.to_string(),
            });
        });
    });
    window.on_send_draft(|text| {
        with_state(|s| {
            // 长粘贴并入正文；文件、图片、会话 chip 作为 ACP 上下文（src/chat），并记录提示词历史。
            let (text, context) = crate::chat::prepare_send(&text);
            if text.trim().is_empty() && context.is_empty() {
                return;
            }
            // 正在编辑队列消息：Send 改为 EditQueued（队列由 hub 持有，结果以下一次 Queue 事件为准）。
            if s.send_edit(text.clone()) {
                return;
            }
            if let Some(session) = s.active.clone() {
                s.send_message(session, text, context);
            } else {
                s.pending_send = Some((text, context));
                s.new_session(false, None);
            }
        });
    });
    window.on_stop(|| {
        with_state(|s| {
            if let Some(session) = s.active.clone() {
                s.send(Command::Cancel { session });
            }
        });
    });
    window.on_cancel_queued(|id| {
        with_state(|s| {
            if let Some(session) = s.active.clone() {
                s.send(Command::CancelQueued {
                    session,
                    id: id.to_string(),
                });
            }
        });
    });
    window.on_choose_agent(|agent| {
        with_state(|s| {
            let agent = agent.to_string();
            if let Some(session) = s.active.clone() {
                s.send(Command::SetStickyAgent { session, agent });
            } else {
                s.sticky = agent;
                s.refresh_agents();
            }
        });
    });
    window.on_permission(|request, allow| {
        with_state(|s| {
            if let Ok(request) = u64::try_from(request) {
                s.answer_permission(request, allow);
            }
        });
    });
    window.on_choose_permission(|request, option_id| {
        with_state(|s| {
            if let (Some(session), Ok(request)) = (s.active.clone(), u64::try_from(request)) {
                s.send(Command::ChoosePermission {
                    session,
                    request,
                    option_id: option_id.to_string(),
                });
            }
        });
    });

    window.on_move_fallback(|index, delta| {
        with_state(|s| {
            let visible: Vec<AgentId> = (0..s.fallback.row_count())
                .filter_map(|i| s.fallback.row_data(i))
                .map(|r| r.id.to_string())
                .collect();
            let (Ok(from), Ok(to)) = (usize::try_from(index), usize::try_from(index + delta))
            else {
                return;
            };
            let (Some(a), Some(b)) = (visible.get(from), visible.get(to)) else {
                return;
            };
            let mut order = s.full_order();
            if let (Some(x), Some(y)) = (
                order.iter().position(|id| id == a),
                order.iter().position(|id| id == b),
            ) {
                order.swap(x, y);
                s.send(Command::SetFallbackOrder(order));
            }
        });
    });
    window.on_toggle_file(|index| {
        with_state(|s| {
            if let Ok(index) = usize::try_from(index) {
                s.toggle_file(index);
            }
        });
    });
    // Round 1: links are not opened yet (opener wiring is round 2); keep the click side-effect free.
    window.on_open_link(|_url| {});
}

/// One transcript row as plain text (find, export).
pub struct TranscriptRow {
    pub id: i32,
    /// `ChatItem.kind`: 0 user, 1 assistant, 2 tool, 3 failover, 4 paused, 5 thinking.
    pub kind: i32,
    pub who: String,
    pub text: String,
}

/// The loaded transcript of the open session, in display order.
pub fn transcript() -> Vec<TranscriptRow> {
    let mut rows = Vec::new();
    with_state(|s| {
        for i in 0..s.chat.row_count() {
            let Some(item) = s.chat.row_data(i) else {
                continue;
            };
            let text = match item.kind {
                0 | 1 => item.text.to_string(),
                2 => item.title.to_string(),
                3 => format!("Switched from {} to {}", item.from, item.to),
                5 => format!("Thought: {}", item.text),
                _ => "All agents reached their limits. Paused".to_string(),
            };
            rows.push(TranscriptRow {
                id: item.id,
                kind: item.kind,
                who: item.agent.to_string(),
                text,
            });
        }
    });
    rows
}

/// (id, display name) of every known agent.
pub fn agent_names() -> Vec<(String, String)> {
    let mut list = Vec::new();
    with_state(|s| {
        list = s
            .agent_list
            .iter()
            .filter(|a| {
                !matches!(
                    a.state,
                    AgentState::Unavailable | AgentState::AdapterMissing
                )
            })
            .map(|a| (a.id.clone(), a.name.clone()))
            .collect();
    });
    list
}

pub fn open_session(id: &str) {
    with_state(|s| {
        s.window.set_settings_open(false);
        s.send(Command::OpenSession {
            session: id.to_string(),
        });
    });
}

pub fn choose_agent(id: &str) {
    if let Some(window) = STATE.with(|cell| {
        cell.try_borrow()
            .ok()
            .and_then(|g| g.as_ref().map(|s| s.window.clone_strong()))
    }) {
        window.invoke_choose_agent(id.into());
    }
}

/// Session action from the command center ("rename", "fork", "toggle-pin"; empty id = the open session).
pub fn session_action(action: &str, id: &str) {
    with_state(|s| s.session_action(action, id));
}

/// Scheduled prompts action from the command center ("open").
pub fn schedule_action(action: &str, id: &str) {
    with_state(|s| s.schedule_action(action, id));
}

pub fn refresh_usage_parts() {
    with_state(|s| s.render_parts());
}

pub fn archive(id: String) {
    archive_session(id);
}

/// Archives one session from the sidebar (hub `archive_sessions`).
fn archive_session(id: String) {
    let (reply, answer) = tokio::sync::oneshot::channel();
    with_state(|s| {
        s.send(Command::Settings(SettingsCall::ArchiveSessions(
            vec![id],
            reply,
        )));
    });
    // 归档结果经 Event::Sessions 刷新列表；这里只把失败（运行中 Busy 等）显示出来。
    let spawned = slint::spawn_local(async move {
        let error = match answer.await {
            Ok(Ok(())) => None,
            Ok(Err(error)) => Some(error.to_string()),
            Err(_) => Some(harness_core_api::ApiError::ShutDown.to_string()),
        };
        if let Some(error) = error {
            with_state(|s| s.window.set_error_text(error.into()));
        }
    });
    if spawned.is_err() {
        with_state(|s| s.window.set_error_text("event loop unavailable".into()));
    }
}

pub fn workspace_path() -> String {
    let mut out = String::new();
    with_state(|s| out = s.workspace.to_string_lossy().into_owned());
    out
}

/// “Open folder…”：在后台线程打开系统文件夹选择框（阻塞调用不能占住 UI 线程），选好后回到事件循环，
/// 用当前粘滞 agent 新建一个工作区为该文件夹的会话。修复：此前该回调把目录写进 RELAUNCH 并 `quit_event_loop`，
/// 由 `relaunch()` 重开进程，用户看到的是 Harness 直接退出。
pub fn install_open_folder(window: &AppWindow) {
    window.on_open_folder(|| {
        let mut start = PathBuf::from(workspace_path());
        std::thread::spawn(move || {
            if !start.is_dir() {
                start = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
            }
            let Some(dir) = rfd::FileDialog::new().set_directory(&start).pick_folder() else {
                return;
            };
            let _ = slint::invoke_from_event_loop(move || {
                with_state(|s| s.new_session(false, Some(dir)));
            });
        });
    });
}

pub fn orchestra_agents() -> Vec<(String, String, bool)> {
    let mut out = Vec::new();
    with_state(|s| {
        out = s
            .agent_list
            .iter()
            .map(|a| {
                (
                    a.id.as_str().to_owned(),
                    a.name.clone(),
                    !s.disabled.contains(&a.id),
                )
            })
            .collect();
    });
    out
}

/// Sends one command to the core (settings pages: overview, connection test).
pub fn send(command: Command) {
    let pending = STATE.with(|cell| match cell.try_borrow_mut() {
        Ok(mut guard) => {
            if let Some(state) = guard.as_mut() {
                state.send(command);
            }
            None
        }
        Err(_) => Some(command),
    });
    if let Some(command) = pending {
        slint::Timer::single_shot(std::time::Duration::ZERO, move || send(command));
    }
}

pub fn request_agent_options() {
    with_state(|s| {
        for agent in &s.agent_list {
            s.send(Command::AgentOptions(agent.id.clone()));
        }
    });
}

/// Drops the UI state (and with it the command/file senders) so the core thread can finish.
pub fn teardown() {
    STATE.with(|cell| {
        if let Ok(mut guard) = cell.try_borrow_mut() {
            if let Some(state) = guard.take() {
                let _ = state.cmd.send(Command::Shutdown);
            }
        }
    });
}

/// Coalesces core events and hands them to the UI thread in batches (spec §3 "Streaming batching").
pub async fn pump(mut events: mpsc::UnboundedReceiver<Event>) {
    const FRAME: Duration = Duration::from_millis(33);
    while let Some(first) = events.recv().await {
        let streaming = matches!(first, Event::TextDelta { .. });
        let mut batch = vec![first];
        if streaming {
            let deadline = tokio::time::Instant::now() + FRAME;
            while let Ok(Some(event)) = tokio::time::timeout_at(deadline, events.recv()).await {
                merge(&mut batch, event);
            }
        } else {
            while let Ok(event) = events.try_recv() {
                merge(&mut batch, event);
            }
        }
        if slint::invoke_from_event_loop(move || apply(batch)).is_err() {
            return;
        }
    }
}

fn merge(batch: &mut Vec<Event>, event: Event) {
    if let Event::TextDelta { session, id, text } = &event {
        if let Some(Event::TextDelta {
            session: last_session,
            id: last_id,
            text: last_text,
        }) = batch.last_mut()
        {
            if last_session == session && last_id == id {
                last_text.push_str(text);
                return;
            }
        }
    }
    batch.push(event);
}
