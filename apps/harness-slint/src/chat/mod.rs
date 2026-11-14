//! Chat and workspace view logic of Phase 9.2 (spec `docs/specs/harness/phase-9-parity.md` §7): command center,
//! find in session, composer popups / chips / long paste / prompt history, navigation history, export, clipboard.
//!
//! Lives in a `thread_local` on the UI thread like `crate::bridge`. Core facts (sessions, transcript, agents) are
//! read from the bridge on demand; nothing here is persisted or sent to the core except through bridge commands.
//! Rule against re-entrancy: callbacks compute an [`Effect`] while borrowing the state and run it after the borrow
//! ends, because effects reach other UI-thread owners (bridge, settings) that may call back into this module.

mod export;
mod fuzzy;
mod whiteboard;

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel};
use tokio::sync::mpsc;

use harness_core_api::{ContextItem, MentionCandidate, MentionKind};

use crate::files::{icon_index, FileReq};
use crate::ui::{AppWindow, ChatUi, ChipRow, Labels, PaletteRow};

/// `ZCode` `LONG_PASTE_TEXT_ATTACHMENT_CHAR_THRESHOLD` (`packages/ui/src/lib/chatAttachments.ts`).
const LONG_PASTE_CHARS: usize = 15 * 1024;
const MAX_ROWS: usize = 50;
const MAX_HISTORY: usize = 100;

/// Command-center sections (`ZCode` `CommandCenter` groups) and their actions, in display order. With an empty query
/// the palette shows "Recent" (actions run from the palette) and then these sections; a query ranks them flat.
const SECTIONS: [(&str, &[&str]); 3] = [
    (
        "suggested",
        &[
            "newTask",
            "findInTask",
            "attachFiles",
            "whiteboard",
            "renameSession",
            "forkSession",
            "pinSession",
            "scheduledPrompts",
            "exportMarkdown",
            "exportHtml",
            "copySessionId",
            "archiveSession",
        ],
    ),
    (
        "panels",
        &[
            "toggleSidePane",
            "switchTheme",
            "previousConversation",
            "nextConversation",
            "navigateBack",
            "navigateForward",
            "zoomIn",
            "zoomOut",
            "resetZoom",
        ],
    ),
    ("configure", &["openSettings", "revealWorkspace"]),
];
/// Most recent palette actions kept for the "Recent" section.
const MAX_RECENT: usize = 5;

/// Local slash commands (agent-advertised commands arrive with the hub's `ChatCall::Commands`, R13).
const SLASH: [(&str, &str); 7] = [
    ("new", "newTask"),
    ("compact", "compactSession"),
    ("autocompact", "toggleAutoCompact"),
    ("find", "findInTask"),
    ("export-md", "exportMarkdown"),
    ("export-html", "exportHtml"),
    ("settings", "openSettings"),
];

#[derive(Clone)]
enum Target {
    Action(String),
    Session(String),
    File(String),
    Agent(String),
    /// Section label row (not runnable).
    Header,
    /// An agent / CLI-config slash command: written into the draft as `/name ` and sent to the agent.
    Slash(String),
    /// Skill or subagent of the active agent (index into `ChatState::skills`, R13).
    Skill(usize),
}

/// Payload of one composer chip (same order as `ChatState::chips`).
#[derive(Clone)]
enum Chip {
    /// Workspace-relative or absolute path; sent as an ACP resource (`ContextItem::File`).
    File(String),
    /// Long paste kept out of the input; folded into the message text (the agents read it as the prompt).
    Paste(String),
    /// Image file (picked, dropped or pasted and saved as PNG).
    Image { path: String, mime: String },
    /// `@session` mention.
    Session(String),
    /// Skill of the active agent: its `SKILL.md` as an ACP resource (R13).
    Skill(ContextItem),
}

/// File extension → image MIME type of the formats the agents accept.
fn image_mime(path: &str) -> Option<&'static str> {
    let ext = Path::new(path)
        .extension()?
        .to_string_lossy()
        .to_ascii_lowercase();
    match ext.as_str() {
        "png" => Some("image/png"),
        "jpg" | "jpeg" => Some("image/jpeg"),
        "gif" => Some("image/gif"),
        "webp" => Some("image/webp"),
        _ => None,
    }
}

/// Work that must run after the state borrow ends.
enum Effect {
    None,
    Command(String),
    OpenSession(String),
    ChooseAgent(String),
    /// Session / schedule actions of the bridge and the file picker (run after the chat borrow ends).
    Later(String),
}

struct ChatState {
    window: AppWindow,
    files_tx: mpsc::UnboundedSender<FileReq>,
    workspace: PathBuf,
    exports: PathBuf,
    /// Workspace-relative files (walk result); `None` until the first walk answered.
    files: Option<Vec<String>>,
    walk_sent: bool,
    palette_rows: Rc<VecModel<PaletteRow>>,
    palette_targets: Vec<Target>,
    palette_query: (String, String),
    /// Action ids run from the palette, newest first (command-center history).
    recent: Vec<String>,
    mention_rows: Rc<VecModel<PaletteRow>>,
    mention_targets: Vec<Target>,
    /// Byte offset where the current `@` / `/` token starts in the draft.
    mention_start: usize,
    chips: Rc<VecModel<ChipRow>>,
    /// Chip payloads, same order as `chips`.
    chip_data: Vec<Chip>,
    /// Slash commands of the open session's agent (`ChatCall::Commands`): (name, description, hint).
    agent_commands: Vec<(String, String)>,
    /// Skills and subagents of the open session's agent (`ChatCall::Mentions`, R13).
    skills: Vec<MentionCandidate>,
    /// Pasted images and whiteboard drawings are saved here (`<data>/attachments`).
    attachments: PathBuf,
    last_draft: String,
    history: Vec<String>,
    /// Index into `history` while browsing with ↑/↓, `None` when editing a fresh draft.
    history_pos: Option<usize>,
    history_stash: String,
    /// Sidebar order and the open session (pushed by the bridge).
    sessions: Vec<(String, String)>,
    active: Option<String>,
    nav: Vec<String>,
    nav_pos: usize,
    nav_jump: Option<String>,
    find_query: String,
    find_hits: Vec<(i32, usize)>,
    find_pos: usize,
    toast_timer: slint::Timer,
}

thread_local! {
    static STATE: RefCell<Option<ChatState>> = const { RefCell::new(None) };
}

fn with_state<T>(f: impl FnOnce(&mut ChatState) -> T) -> Option<T> {
    STATE.with(|cell| {
        cell.try_borrow_mut()
            .ok()
            .and_then(|mut guard| guard.as_mut().map(f))
    })
}

fn sv(s: &str) -> SharedString {
    SharedString::from(s)
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

/// Timeline group and relative time of a session row (`ZCode` task list). Groups use elapsed time, not local
/// calendar days: std has no time-zone database (documented gap in `uiB-9.2.md`).
pub fn timeline(updated_ms: u64) -> (&'static str, &'static str, i32) {
    let elapsed = now_ms().saturating_sub(updated_ms) / 1000;
    let minutes = elapsed / 60;
    let hours = minutes / 60;
    let days = hours / 24;
    let group = match days {
        0 => "today",
        1 => "yesterday",
        2..=6 => "week",
        7..=29 => "month",
        _ => "older",
    };
    let n = |v: u64| i32::try_from(v).unwrap_or(i32::MAX);
    let (unit, amount) = if minutes == 0 {
        ("now", 0)
    } else if hours == 0 {
        ("m", n(minutes))
    } else if days == 0 {
        ("h", n(hours))
    } else if days < 7 {
        ("d", n(days))
    } else {
        ("w", n(days / 7))
    };
    (group, unit, amount)
}

fn file_row(path: &str) -> PaletteRow {
    let (dir, name) = path.rsplit_once('/').unwrap_or(("", path));
    PaletteRow {
        kind: sv("file"),
        id: sv(path),
        title: sv(name),
        detail: sv(dir),
        shortcut: SharedString::new(),
        icon: icon_index(name, false),
    }
}

impl ChatState {
    fn ui(&self) -> ChatUi<'_> {
        self.window.global::<ChatUi>()
    }

    fn want_files(&mut self) {
        if !self.walk_sent {
            self.walk_sent = true;
            let _ = self.files_tx.send(FileReq::Walk(self.workspace.clone()));
        }
    }

    fn action_title(&self, id: &str) -> String {
        let label = self.window.global::<Labels>().invoke_command_label(sv(id));
        if label.as_str() == id {
            self.ui().invoke_action_label(sv(id)).to_string()
        } else {
            label.to_string()
        }
    }

    fn action_enabled(&self, id: &str) -> bool {
        let has = self.active.is_some();
        match id {
            "exportMarkdown" | "exportHtml" | "findInTask" => {
                self.window.get_chat().row_count() > 0
            }
            "copySessionId" | "archiveSession" | "renameSession" | "forkSession" | "pinSession" => {
                has
            }
            "navigateBack" => self.nav_pos > 0,
            "navigateForward" => self.nav_pos + 1 < self.nav.len(),
            "previousConversation" | "nextConversation" => !self.sessions.is_empty(),
            _ => true,
        }
    }

    // ---- command center --------------------------------------------------------------------------------

    fn palette_query(&mut self, query: &str, scope: &str) {
        self.palette_query = (query.to_string(), scope.to_string());
        if scope == "all" || scope == "files" {
            self.want_files();
        }
        let mut scored: Vec<(i32, usize, PaletteRow, Target)> = Vec::new();
        let mut push = |score: Option<i32>, row: PaletteRow, target: Target| {
            if let Some(score) = score {
                let order = scored.len();
                scored.push((score, order, row, target));
            }
        };
        let sections = query.is_empty();
        let header = |id: &str| PaletteRow {
            kind: sv("header"),
            id: sv(id),
            title: sv(id),
            detail: SharedString::new(),
            shortcut: SharedString::new(),
            icon: 0,
        };
        if scope == "all" || scope == "actions" {
            let action_row = |id: &str| PaletteRow {
                kind: sv("action"),
                id: sv(id),
                title: sv(&self.action_title(id)),
                detail: SharedString::new(),
                shortcut: sv(&crate::settings::shortcut_label(id)),
                icon: 0,
            };
            let recent: Vec<String> = self
                .recent
                .iter()
                .filter(|id| self.action_enabled(id))
                .cloned()
                .collect();
            if sections && !recent.is_empty() {
                push(Some(0), header("recent"), Target::Header);
                for id in &recent {
                    push(Some(0), action_row(id), Target::Action(id.clone()));
                }
            }
            for (section, ids) in SECTIONS {
                let ids: Vec<&str> = ids
                    .iter()
                    .copied()
                    .filter(|id| {
                        self.action_enabled(id) && !(sections && recent.iter().any(|r| r == id))
                    })
                    .collect();
                if sections && !ids.is_empty() {
                    push(Some(0), header(section), Target::Header);
                }
                for id in ids {
                    let row = action_row(id);
                    // 动作略微优先：同分时排在会话和文件之前。
                    let ranked = fuzzy::score(query, &row.title).map(|s| s + 2);
                    push(ranked, row, Target::Action(id.into()));
                }
            }
        }
        if sections && scope == "all" && !self.sessions.is_empty() {
            push(Some(0), header("sessions"), Target::Header);
        }
        if scope == "all" || scope == "sessions" {
            for (id, title) in &self.sessions {
                let row = PaletteRow {
                    kind: sv("session"),
                    id: sv(id),
                    title: sv(title),
                    detail: SharedString::new(),
                    shortcut: SharedString::new(),
                    icon: 0,
                };
                push(fuzzy::score(query, title), row, Target::Session(id.clone()));
            }
        }
        if scope == "all" || scope == "files" {
            // 空查询时不列出整个工作区（ZCode 也只在输入后搜索文件）；文件范围除外。
            if !query.is_empty() || scope == "files" {
                for path in self.files.iter().flatten() {
                    push(
                        fuzzy::score(query, path),
                        file_row(path),
                        Target::File(path.clone()),
                    );
                }
            }
        }
        if !query.is_empty() {
            scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
        }
        scored.truncate(MAX_ROWS);
        self.palette_targets = scored.iter().map(|s| s.3.clone()).collect();
        self.palette_rows
            .set_vec(scored.into_iter().map(|s| s.2).collect::<Vec<_>>());
        let first = self
            .palette_targets
            .iter()
            .position(|t| !matches!(t, Target::Header))
            .unwrap_or_default();
        self.ui()
            .set_palette_index(i32::try_from(first).unwrap_or_default());
    }

    /// ↑ / ↓ in the command center: moves to the next runnable row, skipping section labels.
    fn palette_step(&self, delta: i32) {
        let count = self.palette_targets.len();
        if count == 0 {
            return;
        }
        let mut index = usize::try_from(self.ui().get_palette_index()).unwrap_or_default();
        for _ in 0..count {
            index = if delta < 0 {
                (index + count - 1) % count
            } else {
                (index + 1) % count
            };
            if !matches!(self.palette_targets.get(index), Some(Target::Header)) {
                break;
            }
        }
        self.ui()
            .set_palette_index(i32::try_from(index).unwrap_or_default());
    }

    fn palette_run(&mut self, index: usize) -> Effect {
        let Some(target) = self.palette_targets.get(index).cloned() else {
            return Effect::None;
        };
        if matches!(target, Target::Header) {
            return Effect::None;
        }
        self.ui().set_palette_open(false);
        match target {
            Target::Action(id) => {
                self.recent.retain(|r| r != &id);
                self.recent.insert(0, id.clone());
                self.recent.truncate(MAX_RECENT);
                Effect::Command(id)
            }
            Target::Session(id) => Effect::OpenSession(id),
            Target::File(path) => {
                self.add_file_chip(&path);
                Effect::None
            }
            Target::Agent(id) => Effect::ChooseAgent(id),
            Target::Slash(_) | Target::Skill(_) | Target::Header => Effect::None,
        }
    }

    // ---- composer --------------------------------------------------------------------------------------

    fn add_file_chip(&mut self, path: &str) {
        let exists = self.chip_data.iter().any(|c| match c {
            Chip::File(p) | Chip::Image { path: p, .. } => p == path,
            _ => false,
        });
        if exists {
            return;
        }
        let name = Path::new(path)
            .file_name()
            .map_or_else(|| path.to_string(), |n| n.to_string_lossy().into_owned());
        let (kind, chip) = match image_mime(path) {
            Some(mime) => (
                "image",
                Chip::Image {
                    path: path.to_string(),
                    mime: mime.to_string(),
                },
            ),
            None => ("file", Chip::File(path.to_string())),
        };
        self.chips.push(ChipRow {
            kind: sv(kind),
            label: sv(&name),
            detail: SharedString::new(),
        });
        self.chip_data.push(chip);
    }

    fn add_session_chip(&mut self, id: &str) {
        if self
            .chip_data
            .iter()
            .any(|c| matches!(c, Chip::Session(s) if s == id))
        {
            return;
        }
        let title = self
            .sessions
            .iter()
            .find(|(sid, _)| sid == id)
            .map_or_else(|| id.to_string(), |(_, t)| t.clone());
        self.chips.push(ChipRow {
            kind: sv("session"),
            label: sv(&title),
            detail: SharedString::new(),
        });
        self.chip_data.push(Chip::Session(id.to_string()));
    }

    fn add_skill_chip(&mut self, skill: &MentionCandidate) {
        if self
            .chip_data
            .iter()
            .any(|c| matches!(c, Chip::Skill(item) if *item == skill.item))
        {
            return;
        }
        self.chips.push(ChipRow {
            kind: sv("skill"),
            label: sv(&skill.label),
            detail: SharedString::new(),
        });
        self.chip_data.push(Chip::Skill(skill.item.clone()));
    }

    fn draft_edited(&mut self, draft: &str) {
        if draft == self.last_draft {
            return;
        }
        let previous = std::mem::replace(&mut self.last_draft, draft.to_string());
        // 浏览历史时由 history() 写入的草稿不算编辑；用户真正改动后退出历史浏览。
        if let Some(pos) = self.history_pos {
            if self.history.get(pos).map_or(true, |h| h != draft) {
                self.history_pos = None;
            }
        }
        if self.long_paste(&previous, draft) {
            return;
        }
        self.update_mention(draft);
    }

    /// Moves a long pasted block out of the input into a "Pasted text" chip (`ZCode` long-paste attachment).
    fn long_paste(&mut self, previous: &str, draft: &str) -> bool {
        let prefix = previous
            .char_indices()
            .zip(draft.chars())
            .take_while(|((_, a), b)| a == b)
            .last()
            .map_or(0, |((i, a), _)| i + a.len_utf8());
        let tail = previous[prefix..]
            .chars()
            .rev()
            .zip(draft[prefix..].chars().rev())
            .take_while(|(a, b)| a == b)
            .map(|(a, _)| a.len_utf8())
            .sum::<usize>();
        let inserted = &draft[prefix..draft.len() - tail];
        if inserted.chars().count() < LONG_PASTE_CHARS {
            return false;
        }
        let lines = inserted.lines().count();
        self.chips.push(ChipRow {
            kind: sv("paste"),
            label: self
                .window
                .global::<ChatUi>()
                .invoke_action_label(sv("pastedText")),
            detail: sv(&format!("{lines} lines")),
        });
        self.chip_data.push(Chip::Paste(inserted.to_string()));
        let rest = format!("{}{}", &draft[..prefix], &draft[draft.len() - tail..]);
        self.last_draft.clone_from(&rest);
        self.window.set_draft(sv(&rest));
        true
    }

    // Builds the "/" (commands) or "@" (files, sessions, agents) popup; one flat pass per row source.
    #[allow(clippy::too_many_lines)]
    fn update_mention(&mut self, draft: &str) {
        let window = self.window.clone_strong();
        let ui = window.global::<ChatUi>();
        let slash = draft.starts_with('/') && !draft.contains(char::is_whitespace);
        let token_start = draft.rfind(char::is_whitespace).map_or(0, |i| {
            i + draft[i..].chars().next().map_or(1, char::len_utf8)
        });
        let token = &draft[token_start..];
        let trigger = if slash {
            "/"
        } else if token.starts_with('@') {
            "@"
        } else {
            ui.set_mention_open(false);
            return;
        };
        let query = &token[1..];
        self.mention_start = token_start;
        let mut rows: Vec<(i32, usize, PaletteRow, Target)> = Vec::new();
        if trigger == "/" {
            for (name, action) in SLASH {
                if let Some(score) = fuzzy::score(query, name) {
                    let order = rows.len();
                    rows.push((
                        score,
                        order,
                        PaletteRow {
                            kind: sv("command"),
                            id: sv(name),
                            title: sv(&format!("/{name}")),
                            detail: sv(&self.action_title(action)),
                            shortcut: SharedString::new(),
                            icon: 0,
                        },
                        Target::Action(action.into()),
                    ));
                }
            }
            // R13：agent 广播的命令与 CLI 配置里的命令（hub `list_commands`），写入草稿后随消息发给 agent。
            for (name, description) in &self.agent_commands {
                if let Some(score) = fuzzy::score(query, name) {
                    let order = rows.len();
                    rows.push((
                        score,
                        order,
                        PaletteRow {
                            kind: sv("command"),
                            id: sv(name),
                            title: sv(&format!("/{name}")),
                            detail: sv(description),
                            shortcut: SharedString::new(),
                            icon: 0,
                        },
                        Target::Slash(name.clone()),
                    ));
                }
            }
        } else {
            for (id, title) in &self.sessions {
                if self.active.as_deref() == Some(id.as_str()) {
                    continue;
                }
                let shown = if title.is_empty() { id } else { title };
                if let Some(score) = fuzzy::score(query, shown) {
                    let order = rows.len();
                    rows.push((
                        score + 2,
                        order,
                        PaletteRow {
                            kind: sv("session"),
                            id: sv(id),
                            title: sv(shown),
                            detail: sv(id),
                            shortcut: SharedString::new(),
                            icon: 0,
                        },
                        Target::Session(id.clone()),
                    ));
                }
            }
            self.want_files();
            for (id, name) in crate::bridge::agent_names() {
                if let Some(score) = fuzzy::score(query, &name).or_else(|| fuzzy::score(query, &id))
                {
                    let order = rows.len();
                    rows.push((
                        score + 4,
                        order,
                        PaletteRow {
                            kind: sv("agent"),
                            id: sv(&id),
                            title: sv(&name),
                            detail: sv(&id),
                            shortcut: SharedString::new(),
                            icon: 0,
                        },
                        Target::Agent(id),
                    ));
                }
            }
            for path in self.files.iter().flatten() {
                if let Some(score) = fuzzy::score(query, path) {
                    let order = rows.len();
                    rows.push((score, order, file_row(path), Target::File(path.clone())));
                }
            }
            // R13：技能与子 agent 排在会话、agent 与文件之后（同分时更靠后）。
            for (index, skill) in self.skills.iter().enumerate() {
                if let Some(score) = fuzzy::score(query, &skill.label) {
                    let order = rows.len();
                    let kind = if skill.kind == MentionKind::Subagent {
                        "subagent"
                    } else {
                        "skill"
                    };
                    rows.push((
                        score - 1,
                        order,
                        PaletteRow {
                            kind: sv(kind),
                            id: sv(&skill.label),
                            title: sv(&skill.label),
                            detail: sv(&skill.detail),
                            shortcut: SharedString::new(),
                            icon: 0,
                        },
                        Target::Skill(index),
                    ));
                }
            }
        }
        if !query.is_empty() {
            rows.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
        }
        rows.truncate(MAX_ROWS);
        self.mention_targets = rows.iter().map(|r| r.3.clone()).collect();
        self.mention_rows
            .set_vec(rows.into_iter().map(|r| r.2).collect::<Vec<_>>());
        ui.set_mention_trigger(sv(trigger));
        ui.set_mention_index(0);
        ui.set_mention_open(true);
    }

    fn mention_pick(&mut self, index: usize) -> Effect {
        let Some(target) = self.mention_targets.get(index).cloned() else {
            return Effect::None;
        };
        self.ui().set_mention_open(false);
        let draft = self.last_draft.clone();
        let head = draft
            .get(..self.mention_start)
            .unwrap_or_default()
            .to_string();
        let (next, effect) = match target {
            // 文件：以 @路径 写入正文（agent CLI 都识别 @path），同时不额外加附件。
            Target::File(path) => (format!("{head}@{path} "), Effect::None),
            // agent：本条消息改由该 agent 处理（切换会话的 sticky agent），移除 @ 记号。
            Target::Agent(id) => (head, Effect::ChooseAgent(id)),
            // 斜杠命令：立即执行并清空输入。
            Target::Action(id) => (String::new(), Effect::Command(id)),
            // 会话：作为 #session 上下文附件（ContextItem::Session），移除 @ 记号。
            Target::Session(id) => {
                self.add_session_chip(&id);
                (head, Effect::None)
            }
            Target::Slash(name) => (format!("/{name} "), Effect::None),
            // 子 agent 插入 `@agent-<name>`（Claude Code 语法），不加附件；技能作为 SKILL.md 资源附件。
            Target::Skill(index) => match self.skills.get(index).cloned() {
                Some(MentionCandidate {
                    insert_text: Some(insert),
                    ..
                }) => (format!("{head}{insert} "), Effect::None),
                Some(skill) => {
                    self.add_skill_chip(&skill);
                    (head, Effect::None)
                }
                None => (draft, Effect::None),
            },
            Target::Header => (draft, Effect::None),
        };
        self.last_draft.clone_from(&next);
        self.window.set_draft(sv(&next));
        effect
    }

    fn history(&mut self, delta: i32) -> bool {
        if self.history.is_empty() {
            return false;
        }
        let draft = self.window.get_draft().to_string();
        let next = match (self.history_pos, delta < 0) {
            (None, true) if draft.is_empty() => {
                self.history_stash = draft;
                Some(self.history.len() - 1)
            }
            (Some(pos), true) => Some(pos.saturating_sub(1)),
            (Some(pos), false) if pos + 1 < self.history.len() => Some(pos + 1),
            (Some(_), false) => {
                // 越过最新一条：回到浏览前的草稿。
                self.history_pos = None;
                let stash = std::mem::take(&mut self.history_stash);
                self.last_draft.clone_from(&stash);
                self.window.set_draft(sv(&stash));
                return true;
            }
            _ => None,
        };
        let Some(pos) = next else {
            return false;
        };
        self.history_pos = Some(pos);
        let text = self.history[pos].clone();
        self.last_draft.clone_from(&text);
        self.window.set_draft(sv(&text));
        true
    }

    /// Final message: pasted text is folded into the text; files, images and sessions become ACP context items
    /// (`Command::SendContext`, spec §9.2.1). The prompt is recorded in the history.
    fn prepare_send(&mut self, text: &str) -> (String, Vec<ContextItem>) {
        let mut out = text.trim_end().to_string();
        let mut context = Vec::new();
        for chip in &self.chip_data {
            match chip {
                Chip::Paste(data) => {
                    if !out.is_empty() {
                        out.push_str("\n\n");
                    }
                    out.push_str(data);
                }
                Chip::File(path) => context.push(ContextItem::File { path: path.clone() }),
                Chip::Image { path, mime } => context.push(ContextItem::Image {
                    path: Some(path.clone()),
                    data_base64: None,
                    mime: mime.clone(),
                }),
                Chip::Session(id) => context.push(ContextItem::Session {
                    session_id: harness_core_api::SessionId::new(id.as_str()),
                }),
                Chip::Skill(item) => context.push(item.clone()),
            }
        }
        self.chips.set_vec(Vec::new());
        self.chip_data.clear();
        self.ui().set_mention_open(false);
        if !text.trim().is_empty() && self.history.last().map_or(true, |h| h != text) {
            self.history.push(text.to_string());
            if self.history.len() > MAX_HISTORY {
                self.history.remove(0);
            }
        }
        self.history_pos = None;
        self.last_draft.clear();
        (out, context)
    }

    /// Puts a rewound turn back into the composer: its text and its context items as chips.
    fn restore(&mut self, text: &str, items: Vec<ContextItem>) {
        self.chips.set_vec(Vec::new());
        self.chip_data.clear();
        for item in items {
            match item {
                ContextItem::File { path } | ContextItem::Folder { path } => {
                    self.add_file_chip(&path);
                }
                ContextItem::Image {
                    path: Some(path), ..
                } => self.add_file_chip(&path),
                ContextItem::Image { path: None, .. } => {}
                ContextItem::Text { text, .. } => {
                    let lines = text.lines().count();
                    self.chips.push(ChipRow {
                        kind: sv("paste"),
                        label: self
                            .window
                            .global::<ChatUi>()
                            .invoke_action_label(sv("pastedText")),
                        detail: sv(&format!("{lines} lines")),
                    });
                    self.chip_data.push(Chip::Paste(text));
                }
                ContextItem::Session { session_id } => self.add_session_chip(session_id.as_str()),
            }
        }
        self.last_draft = text.to_string();
        self.window.set_draft(sv(text));
    }

    // ---- navigation ------------------------------------------------------------------------------------

    fn refresh_nav(&self) {
        let ui = self.ui();
        let index = self
            .active
            .as_ref()
            .and_then(|a| self.sessions.iter().position(|(id, _)| id == a));
        ui.set_can_back(self.nav_pos > 0);
        ui.set_can_forward(self.nav_pos + 1 < self.nav.len());
        ui.set_can_prev(match index {
            Some(i) => i > 0,
            None => !self.sessions.is_empty(),
        });
        ui.set_can_next(match index {
            Some(i) => i + 1 < self.sessions.len(),
            None => !self.sessions.is_empty(),
        });
        ui.set_has_session(self.active.is_some());
    }

    fn session_opened(&mut self, id: &str) {
        if self.nav_jump.as_deref() == Some(id) {
            self.nav_jump = None;
        } else if self.nav.get(self.nav_pos).map_or(true, |cur| cur != id) {
            if !self.nav.is_empty() {
                self.nav.truncate(self.nav_pos + 1);
            }
            self.nav.push(id.to_string());
            self.nav_pos = self.nav.len() - 1;
        }
        self.active = Some(id.to_string());
        self.close_find();
        self.refresh_nav();
    }

    fn nav(&mut self, how: &str) -> Effect {
        let target = match how {
            "back" if self.nav_pos > 0 => {
                self.nav_pos -= 1;
                self.nav.get(self.nav_pos).cloned()
            }
            "forward" if self.nav_pos + 1 < self.nav.len() => {
                self.nav_pos += 1;
                self.nav.get(self.nav_pos).cloned()
            }
            "prev" | "next" => {
                let index = self
                    .active
                    .as_ref()
                    .and_then(|a| self.sessions.iter().position(|(id, _)| id == a));
                let next = match (index, how) {
                    (Some(i), "prev") => i.checked_sub(1),
                    (Some(i), _) => Some(i + 1),
                    (None, _) => Some(0),
                };
                return next
                    .and_then(|i| self.sessions.get(i))
                    .map_or(Effect::None, |(id, _)| Effect::OpenSession(id.clone()));
            }
            _ => None,
        };
        target.map_or(Effect::None, |id| {
            self.nav_jump = Some(id.clone());
            Effect::OpenSession(id)
        })
    }

    // ---- find ------------------------------------------------------------------------------------------

    fn find(&mut self, query: &str) {
        self.find_query = query.to_lowercase();
        self.find_hits.clear();
        self.find_pos = 0;
        if !self.find_query.is_empty() {
            for (row, item) in crate::bridge::transcript().into_iter().enumerate() {
                let haystack = item.text.to_lowercase();
                let count = haystack.matches(self.find_query.as_str()).count();
                self.find_hits
                    .extend(std::iter::repeat((item.id, row)).take(count));
            }
        }
        self.show_find();
    }

    fn find_step(&mut self, delta: i32) {
        let len = self.find_hits.len();
        if len == 0 {
            return;
        }
        self.find_pos = if delta < 0 {
            (self.find_pos + len - 1) % len
        } else {
            (self.find_pos + 1) % len
        };
        self.show_find();
    }

    fn show_find(&self) {
        let ui = self.ui();
        let count = i32::try_from(self.find_hits.len()).unwrap_or(i32::MAX);
        ui.set_find_count(count);
        if let Some(&(id, row)) = self.find_hits.get(self.find_pos) {
            ui.set_find_current(i32::try_from(self.find_pos + 1).unwrap_or(i32::MAX));
            ui.set_find_id(id);
            ui.set_find_row(i32::try_from(row).unwrap_or(-1));
        } else {
            ui.set_find_current(0);
            ui.set_find_id(-1);
            ui.set_find_row(-1);
        }
        ui.set_find_seq(ui.get_find_seq().wrapping_add(1));
    }

    fn close_find(&mut self) {
        self.find_query.clear();
        self.find_hits.clear();
        self.find_pos = 0;
        let ui = self.ui();
        ui.set_find_open(false);
        ui.set_find_id(-1);
        ui.set_find_row(-1);
        ui.set_find_count(0);
        ui.set_find_current(0);
    }

    // ---- feedback --------------------------------------------------------------------------------------

    fn toast(&self, text: &str) {
        self.ui().set_toast(sv(text));
        let weak = self.window.as_weak();
        self.toast_timer.start(
            slint::TimerMode::SingleShot,
            Duration::from_millis(1800),
            move || {
                if let Some(window) = weak.upgrade() {
                    window.global::<ChatUi>().set_toast(SharedString::new());
                }
            },
        );
    }

    fn copy(&self, text: &str) {
        self.window.invoke_copy_text(sv(text));
        let label = self.ui().invoke_action_label(sv("copied"));
        self.toast(&label);
    }

    fn export(&self, format: &str) {
        let rows = crate::bridge::transcript();
        if rows.is_empty() {
            return;
        }
        let title = self
            .active
            .as_ref()
            .and_then(|a| self.sessions.iter().find(|(id, _)| id == a))
            .map(|(_, t)| t.clone())
            .filter(|t| !t.is_empty())
            .unwrap_or_else(|| "Harness session".into());
        let (contents, ext) = if format == "html" {
            (export::html(&title, &rows), "html")
        } else {
            (export::markdown(&title, &rows), "md")
        };
        let path = self.exports.join(format!(
            "{}-{}.{ext}",
            export::slug(&title),
            now_ms() / 1000
        ));
        let _ = self.files_tx.send(FileReq::Write { path, contents });
    }
}

/// Runs an effect outside every state borrow.
fn run(effect: Effect) {
    match effect {
        Effect::None => {}
        Effect::Command(id) => crate::settings::run_command(&id),
        Effect::OpenSession(id) => crate::bridge::open_session(&id),
        Effect::ChooseAgent(id) => crate::bridge::choose_agent(&id),
        Effect::Later(id) => match id.as_str() {
            "attachFiles" => pick_files(),
            "whiteboard" => whiteboard::open(-1),
            "scheduledPrompts" => crate::bridge::schedule_action("open", ""),
            "renameSession" => crate::bridge::session_action("rename", ""),
            "forkSession" => crate::bridge::session_action("fork", ""),
            "pinSession" => crate::bridge::session_action("toggle-pin", ""),
            _ => {}
        },
    }
}

/// Commands the settings dispatcher does not own (keymap ids and the chat-only actions).
pub fn skill_count() -> usize {
    with_state(|s| s.skills.len()).unwrap_or(0)
}

pub fn command(id: &str) {
    if id == "compactSession" {
        let weak = with_state(|s| s.window.as_weak());
        slint::Timer::single_shot(std::time::Duration::ZERO, move || {
            if let Some(window) = weak.and_then(|w| w.upgrade()) {
                if !window.get_running() {
                    window.invoke_send_draft("/compact".into());
                }
            }
        });
        return;
    }
    let effect = with_state(|s| match id {
        "openCommandCenter" => {
            let ui = s.ui();
            let open = !ui.get_palette_open();
            if open {
                // 修复：上次关闭时停在 Files 等范围，重新打开仍沿用，导致动作搜不到；与 ZCode 一致，每次打开回到 All。
                ui.set_palette_scope("all".into());
                ui.set_palette_index(0);
            }
            ui.set_palette_open(open);
            Effect::None
        }
        "findInTask" => {
            if s.window.get_chat().row_count() > 0 {
                s.ui().set_find_open(true);
            }
            Effect::None
        }
        "toggleSidebar" => {
            s.window.set_drawer_open(!s.window.get_drawer_open());
            Effect::None
        }
        "previousConversation" => s.nav("prev"),
        "nextConversation" => s.nav("next"),
        "navigateBack" => s.nav("back"),
        "navigateForward" => s.nav("forward"),
        "exportMarkdown" => {
            s.export("markdown");
            Effect::None
        }
        "exportHtml" => {
            s.export("html");
            Effect::None
        }
        "copySessionId" => {
            if let Some(id) = s.active.clone() {
                s.copy(&id);
            }
            Effect::None
        }
        "revealWorkspace" => {
            reveal(&s.workspace);
            Effect::None
        }
        "archiveSession" => {
            if let Some(id) = s.active.clone() {
                crate::bridge::archive(id);
            }
            Effect::None
        }
        "renameSession" | "forkSession" | "pinSession" | "scheduledPrompts" | "attachFiles"
        | "whiteboard" => Effect::Later(id.to_string()),
        _ => Effect::None,
    });
    if let Some(effect) = effect {
        run(effect);
    }
}

fn reveal(path: &Path) {
    use harness_platform::PathRevealer;
    // 资源管理器可能阻塞，放到独立线程；失败只显示在错误横幅里。
    let path = path.to_path_buf();
    let weak = with_state(|s| s.window.as_weak());
    std::thread::spawn(move || {
        if let Err(error) = harness_platform::desktop::SystemPathRevealer::new().reveal(&path) {
            let message = error.to_string();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(window) = weak.and_then(|w| w.upgrade()) {
                    window.set_error_text(sv(&message));
                }
            });
        }
    });
}

// ---- bridge / worker hooks (UI thread) ------------------------------------------------------------------

/// Sidebar order and open session changed (bridge `refresh_sessions`).
pub fn sessions_changed(list: Vec<(String, String)>, active: Option<&str>) {
    with_state(|s| {
        s.sessions = list;
        if s.active.as_deref() != active {
            if let Some(id) = active {
                s.session_opened(id);
            } else {
                s.active = None;
                s.close_find();
            }
        }
        s.refresh_nav();
    });
}

/// Message about to be sent: pasted text folded in, the other chips as context items, prompt history recorded.
pub fn prepare_send(text: &str) -> (String, Vec<ContextItem>) {
    with_state(|s| s.prepare_send(text)).unwrap_or_else(|| (text.to_string(), Vec::new()))
}

/// Path and label of the image chip at `index` (whiteboard annotation).
fn chip_image(index: usize) -> Option<(String, String)> {
    with_state(
        |s| match (s.chip_data.get(index), s.chips.row_data(index)) {
            (Some(Chip::Image { path, .. }), Some(row)) => {
                let full = if Path::new(path).is_absolute() {
                    PathBuf::from(path)
                } else {
                    s.workspace.join(path)
                };
                Some((full.to_string_lossy().into_owned(), row.label.to_string()))
            }
            _ => None,
        },
    )
    .flatten()
}

/// Removes the image chip whose file is `full` (the annotated original). Looked up by path, not index: chips can be
/// removed while the whiteboard is open.
fn remove_image_chip(full: &str) {
    with_state(|s| {
        let found = s.chip_data.iter().position(|c| match c {
            Chip::Image { path, .. } => {
                let resolved = if Path::new(path).is_absolute() {
                    PathBuf::from(path)
                } else {
                    s.workspace.join(path)
                };
                resolved.to_string_lossy() == full
            }
            _ => false,
        });
        if let Some(index) = found {
            s.chip_data.remove(index);
            s.chips.remove(index);
        }
    });
}

fn attachments_dir() -> Option<PathBuf> {
    with_state(|s| s.attachments.clone())
}

/// Short confirmation toast; `id` is a `ChatUi.action-label` id (translated in Slint).
pub fn toast_label(id: &str) {
    with_state(|s| {
        let label = s.ui().invoke_action_label(sv(id));
        s.toast(&label);
    });
}

/// "Open in editor" of a tool-card path (`path` or `path:line`): the OS default app on its own thread.
/// `HARNESS_OPENER_LOG` (gates only) appends the resolved path to that file instead of launching an app.
pub fn open_path(raw: &str) {
    let Some(workspace) = with_state(|s| s.workspace.clone()) else {
        return;
    };
    let trimmed = raw.trim();
    // "src/a.rs:12" → "src/a.rs"; a Windows drive "C:\x" keeps its colon (only a numeric suffix is cut).
    let path = match trimmed.rsplit_once(':') {
        Some((head, line)) if !line.is_empty() && line.bytes().all(|b| b.is_ascii_digit()) => head,
        _ => trimmed,
    };
    let path = if Path::new(path).is_absolute() {
        PathBuf::from(path)
    } else {
        // 修复：agent 报告的相对路径用 "/" 分隔，直接 join 会在 Windows 上得到 "ws\src/a.rs" 这种混合分隔符，
        // 交给 explorer 时可能打不开；按组件逐段拼接，得到本平台的分隔符。
        path.split(['/', '\\'])
            .filter(|c| !c.is_empty() && *c != ".")
            .fold(workspace, |acc, c| acc.join(c))
    };
    std::thread::spawn(move || {
        let result = match std::env::var_os("HARNESS_OPENER_LOG") {
            Some(log) => {
                use std::io::Write;
                std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(log)
                    .and_then(|mut f| writeln!(f, "{}", path.display()))
            }
            None => open_with_os(&path),
        };
        if let Err(error) = result {
            let message = error.to_string();
            let _ = slint::invoke_from_event_loop(move || {
                with_state(|s| s.window.set_error_text(sv(&message)));
            });
        }
    });
}

fn open_with_os(path: &Path) -> std::io::Result<()> {
    let mut command = if cfg!(windows) {
        let mut c = std::process::Command::new("explorer.exe");
        c.arg(path);
        c
    } else if cfg!(target_os = "macos") {
        let mut c = std::process::Command::new("open");
        c.arg(path);
        c
    } else {
        let mut c = std::process::Command::new("xdg-open");
        c.arg(path);
        c
    };
    // explorer.exe 成功打开时也可能返回非零退出码，这里只关心能否启动。
    command.spawn().map(|_| ())
}

/// A rewound turn's text and context back into the composer (`ChatCall::Rewind` result).
pub fn restore_composer(text: &str, items: Vec<ContextItem>) {
    with_state(|s| s.restore(text, items));
}

/// Skills and subagents of the open session's agent (`ChatCall::Mentions` with the skill kinds).
pub fn agent_mentions(list: Vec<MentionCandidate>) {
    with_state(|s| s.skills = list);
}

/// Slash commands of the open session's agent (`ChatCall::Commands`).
pub fn agent_commands(commands: Vec<(String, String)>) {
    with_state(|s| {
        s.agent_commands = commands;
        if s.ui().get_mention_open() {
            let draft = s.last_draft.clone();
            s.update_mention(&draft);
        }
    });
}

/// Adds picked / saved files as chips (images become image chips).
pub fn attach_paths(paths: Vec<PathBuf>) {
    for path in paths {
        dropped(&path);
    }
}

/// "Attach files": the OS file picker runs on its own thread so the UI keeps painting.
pub fn pick_files() {
    let start = with_state(|s| s.workspace.clone());
    std::thread::spawn(move || {
        let mut dialog = rfd::FileDialog::new();
        if let Some(dir) = start {
            dialog = dialog.set_directory(dir);
        }
        if let Some(paths) = dialog.pick_files() {
            let _ = slint::invoke_from_event_loop(move || attach_paths(paths));
        }
    });
}

/// Ctrl/Cmd+V with an image on the clipboard: saved as PNG under `<data>/attachments`, attached as an image chip.
/// Returns false when the clipboard holds no image (the text paste then proceeds).
pub fn paste_image() -> bool {
    let Some(png) = clipboard_png() else {
        return false;
    };
    let Some(dir) = with_state(|s| s.attachments.clone()) else {
        return false;
    };
    save_attachment(dir, png, "pasted");
    true
}

/// Writes PNG bytes to `<dir>/<prefix>-<uuid>.png` off the UI thread, then attaches the file.
pub fn save_attachment(dir: PathBuf, png: Vec<u8>, prefix: &str) {
    let path = dir.join(format!("{prefix}-{}.png", uuid::Uuid::new_v4().simple()));
    std::thread::spawn(move || {
        let result = std::fs::create_dir_all(&dir).and_then(|()| std::fs::write(&path, &png));
        let _ = slint::invoke_from_event_loop(move || match result {
            Ok(()) => attach_paths(vec![path]),
            Err(error) => {
                with_state(|s| s.window.set_error_text(sv(&error.to_string())));
            }
        });
    });
}

#[cfg(windows)]
fn clipboard_png() -> Option<Vec<u8>> {
    use clipboard_win::{formats, get_clipboard};
    let bmp: Vec<u8> = get_clipboard(formats::Bitmap).ok()?;
    let image = image::load_from_memory_with_format(&bmp, image::ImageFormat::Bmp).ok()?;
    let mut out = std::io::Cursor::new(Vec::new());
    image.write_to(&mut out, image::ImageFormat::Png).ok()?;
    Some(out.into_inner())
}

/// Image paste reads the Windows clipboard bitmap; other platforms attach images by drop or the file picker.
#[cfg(not(windows))]
fn clipboard_png() -> Option<Vec<u8>> {
    None
}

/// Files the OS dropped on the window (winit `DroppedFile`); paths inside the workspace become relative.
pub fn dropped(path: &Path) {
    with_state(|s| {
        s.ui().set_drop_hover(false);
        let shown = path.strip_prefix(&s.workspace).map_or_else(
            |_| path.to_string_lossy().into_owned(),
            |rel| rel.to_string_lossy().replace('\\', "/"),
        );
        s.add_file_chip(&shown);
    });
}

pub fn drop_hover(on: bool) {
    with_state(|s| s.ui().set_drop_hover(on));
}

/// Workspace walk reply (files worker).
pub fn files_walked(files: Vec<String>) {
    with_state(|s| {
        s.files = Some(files);
        if s.ui().get_palette_open() {
            let (query, scope) = s.palette_query.clone();
            s.palette_query(&query, &scope);
        }
        if s.ui().get_mention_open() {
            let draft = s.last_draft.clone();
            s.update_mention(&draft);
        }
    });
}

/// Export write reply (files worker).
pub fn exported(result: Result<PathBuf, String>) {
    with_state(|s| match result {
        Ok(path) => {
            let label = s.ui().invoke_action_label(sv("exported"));
            s.toast(&format!("{label} {}", path.display()));
        }
        Err(error) => s.window.set_error_text(sv(&error)),
    });
}

pub fn install(
    window: &AppWindow,
    files_tx: mpsc::UnboundedSender<FileReq>,
    workspace: &Path,
    data_dir: &Path,
) {
    let palette_rows = Rc::new(VecModel::default());
    let mention_rows = Rc::new(VecModel::default());
    let chips = Rc::new(VecModel::default());
    let ui = window.global::<ChatUi>();
    ui.set_palette_rows(ModelRc::from(palette_rows.clone()));
    ui.set_mention_rows(ModelRc::from(mention_rows.clone()));
    ui.set_chips(ModelRc::from(chips.clone()));
    let state = ChatState {
        window: window.clone_strong(),
        files_tx,
        workspace: workspace.to_path_buf(),
        exports: data_dir.join("exports"),
        files: None,
        walk_sent: false,
        palette_rows,
        palette_targets: Vec::new(),
        palette_query: (String::new(), "all".into()),
        recent: Vec::new(),
        mention_rows,
        mention_targets: Vec::new(),
        mention_start: 0,
        chips,
        chip_data: Vec::new(),
        agent_commands: Vec::new(),
        skills: Vec::new(),
        attachments: data_dir.join("attachments"),
        last_draft: String::new(),
        history: Vec::new(),
        history_pos: None,
        history_stash: String::new(),
        sessions: Vec::new(),
        active: None,
        nav: Vec::new(),
        nav_pos: 0,
        nav_jump: None,
        find_query: String::new(),
        find_hits: Vec::new(),
        find_pos: 0,
        toast_timer: slint::Timer::default(),
    };
    STATE.with(|cell| {
        if let Ok(mut guard) = cell.try_borrow_mut() {
            *guard = Some(state);
        }
    });
    wire(window);
}

fn index(i: i32) -> usize {
    usize::try_from(i).unwrap_or(usize::MAX)
}

fn wire(window: &AppWindow) {
    let ui = window.global::<ChatUi>();
    ui.on_palette_step(|delta| {
        with_state(|s| s.palette_step(delta));
    });
    ui.on_palette_query(|query, scope| {
        with_state(|s| s.palette_query(&query, &scope));
    });
    ui.on_palette_run(|i| {
        if let Some(effect) = with_state(|s| s.palette_run(index(i))) {
            run(effect);
        }
    });
    ui.on_find(|query| {
        with_state(|s| s.find(&query));
    });
    ui.on_find_step(|delta| {
        with_state(|s| s.find_step(delta));
    });
    ui.on_find_close(|| {
        with_state(ChatState::close_find);
    });
    ui.on_draft_edited(|draft| {
        with_state(|s| s.draft_edited(&draft));
    });
    ui.on_mention_pick(|i| {
        if let Some(effect) = with_state(|s| s.mention_pick(index(i))) {
            run(effect);
        }
    });
    ui.on_remove_chip(|i| {
        with_state(|s| {
            let i = index(i);
            if i < s.chip_data.len() {
                s.chip_data.remove(i);
                s.chips.remove(i);
            }
        });
    });
    ui.on_history(|delta| with_state(|s| s.history(delta)).unwrap_or(false));
    ui.on_nav(|how| {
        if let Some(effect) = with_state(|s| s.nav(&how)) {
            run(effect);
        }
    });
    ui.on_export(|format| {
        with_state(|s| s.export(&format));
    });
    ui.on_reveal(|| command("revealWorkspace"));
    ui.on_copy_session_id(|id| {
        with_state(|s| {
            let id = if id.is_empty() {
                s.active.clone()
            } else {
                Some(id.to_string())
            };
            if let Some(id) = id {
                s.copy(&id);
            }
        });
    });
    ui.on_copy(|text| {
        with_state(|s| s.copy(&text));
    });
    ui.on_link_at(|links, offset| crate::markdown::link_at(&links, offset));
    ui.on_dismiss_error(|| {
        with_state(|s| s.window.set_error_text(SharedString::new()));
    });
    ui.on_attach_files(pick_files);
    whiteboard::install(window);
    ui.on_open_path(|path| open_path(&path));
    ui.on_paste_image(paste_image);
    install_drop(window);
}

/// OS file drops (Slint's winit backend does not forward them as Slint events).
fn install_drop(window: &AppWindow) {
    use slint::winit_030::{winit::event::WindowEvent, EventResult, WinitWindowAccessor};
    window.window().on_winit_window_event(|_, event| {
        match event {
            WindowEvent::HoveredFile(_) => drop_hover(true),
            WindowEvent::HoveredFileCancelled => drop_hover(false),
            WindowEvent::DroppedFile(path) => dropped(path),
            _ => {}
        }
        EventResult::Propagate
    });
}

/// Drops the files sender so the core thread can finish.
pub fn teardown() {
    STATE.with(|cell| {
        if let Ok(mut guard) = cell.try_borrow_mut() {
            guard.take();
        }
    });
}
