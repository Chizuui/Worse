//! Settings pages, UI-thread side (spec phase-9-parity.md §6.3). State owners:
//! - `settings.json`: the shared `SettingsStore` (one per process, opened in `main`); this module only sends
//!   patches to the worker and mirrors the store's watch snapshots (`apply_snapshot`).
//! - Agent CLI files: the worker plans, the user confirms the diff, the worker applies.
//! - MEMORY.md, projects, archive: the hub (through `Command::Settings`); the worker only forwards the calls.
//! - Agents overview / connection test: the hub (`Command::AgentsOverview` / `TestAgent`, answered as events).
//! - Page-local state (form fields, query, recorder) lives here and is not persisted.

pub mod agy;
mod docview;
pub mod keymap;
mod values;
mod worker;

use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;

use harness_agent_config::{
    AgentKind, Command as CmdEntry, Concept, ConceptSupport, ConfigTarget, Entry, EntryBody, Hook,
    McpServer, McpTransport, Scope, Skill, Subagent,
};
use harness_core_api as api;
use harness_settings::{Language, Settings};
use serde_json::Value;
use slint::{ComponentHandle, ModelRc, SharedString, VecModel};
use tokio::sync::mpsc;

use crate::ui::{
    AccountRow, AgentCard, AgentsUi, AppWindow, ArchiveRow, ArchiveUi, ConfigUi, DiffLine, DiffUi,
    DocUi, FormField, Labels, NavEntry, ProjectRow, ProjectsUi, ProviderRow, ResGroup, ResRow,
    SelectOption, SettingsUi, ShortcutRow, ShortcutsUi, Theme,
};
use keymap::{Keymap, Press, Verdict};
use worker::{DocKind, PlanOp};
pub use worker::{Req, Worker};

const NAV: &[(&str, i32)] = &[
    ("general", 0),
    ("appearance", 0),
    ("agents", 0),
    ("orchestra", 0),
    ("inbox", 0),
    ("remote", 0),
    ("shortcuts", 0),
    ("instructions", 1),
    ("memory", 1),
    ("subagent", 1),
    ("mcp", 1),
    ("skill", 1),
    ("command", 1),
    ("hook", 1),
    ("projects", 2),
    ("archive", 2),
    ("import", 2),
];

const HOOK_EVENTS: &[&str] = &[
    "PreToolUse",
    "PostToolUse",
    "UserPromptSubmit",
    "Notification",
    "Stop",
    "SubagentStop",
    "SessionStart",
    "SessionEnd",
    "PreCompact",
];

/// A loaded instructions / memory document.
pub struct Doc {
    pub path: PathBuf,
    pub exists: bool,
    pub text: String,
    pub supported: bool,
    pub reason: String,
}

struct Edit {
    previous: Option<String>,
    base: Option<EntryBody>,
    fields: Vec<(String, String)>,
}

#[derive(Default)]
struct AgentOptions {
    modes: Vec<(String, String)>,
    models: Vec<(String, String)>,
    efforts: Vec<(String, String)>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum After {
    Config,
    Doc,
}

pub struct State {
    window: AppWindow,
    req: mpsc::UnboundedSender<Req>,
    settings: Arc<Settings>,
    rev: i32,
    seq: u64,
    agents: Vec<(String, String, bool)>,
    auth: HashMap<String, i32>,
    launch: HashMap<String, (String, String, String)>,
    options: HashMap<String, AgentOptions>,
    support: Option<ConceptSupport>,
    entries: Vec<Entry>,
    edit: Option<Edit>,
    doc_original: String,
    doc_path: PathBuf,
    after: After,
    /// Hub overview per agent: (version, auth 0 present / 1 absent / 2 unknown).
    overview: HashMap<String, (String, i32)>,
    /// Agents whose CLI is installed but whose ACP adapter is missing (hub overview, §9.10).
    adapter_missing: HashSet<String>,
    /// Extra Claude accounts (hub, phase-9-parity.md §9.6).
    accounts: Vec<api::AccountInfo>,
    /// Custom API providers (hub, phase-9-parity.md §9.7; never with the key).
    providers: Vec<api::ProviderInfo>,
    /// Connection test per agent: (state 1 running / 2 ok / 3 failed, latency ms, error).
    tests: HashMap<String, (i32, i32, String)>,
    /// Sequence of the projects / archive lists (separate from the config / document `seq`).
    list_seq: u64,
    projects: Vec<api::ProjectInfo>,
    selected: HashSet<String>,
    /// Keys removed by the last remove (undo restores them).
    removed: Vec<String>,
    archive: Vec<ArchiveRow>,
}

/// `YYYY-MM-DD` (UTC) of a unix-ms timestamp; language-neutral, so no translation is needed.
pub(crate) fn date_of(ms: i64) -> String {
    let days = ms.div_euclid(86_400_000);
    // Civil-from-days (H. Hinnant), valid for the whole i64 day range used here.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}")
}

fn folder_name(path: &str) -> String {
    Path::new(path)
        .file_name()
        .map_or_else(|| path.to_string(), |n| n.to_string_lossy().into_owned())
}

thread_local! {
    static STATE: RefCell<Option<State>> = const { RefCell::new(None) };
    // Pure-callback reads (bindings) never borrow STATE: they use this copy of the snapshot and the keymap.
    static SNAP: RefCell<Value> = const { RefCell::new(Value::Null) };
    static KEYMAP: RefCell<Option<Keymap>> = const { RefCell::new(None) };
}

pub(super) fn with_state(f: impl FnOnce(&mut State)) {
    STATE.with(|cell| {
        if let Ok(mut guard) = cell.try_borrow_mut() {
            if let Some(state) = guard.as_mut() {
                f(state);
            }
        }
    });
}

fn snap<T>(f: impl FnOnce(&Value) -> T) -> T {
    SNAP.with(|cell| f(&cell.borrow()))
}

fn keymap<T>(f: impl FnOnce(&Keymap) -> T) -> Option<T> {
    KEYMAP.with(|cell| cell.borrow().as_ref().map(f))
}

fn sv(s: impl AsRef<str>) -> SharedString {
    SharedString::from(s.as_ref())
}

fn model<T: Clone + 'static>(items: Vec<T>) -> ModelRc<T> {
    ModelRc::from(Rc::new(VecModel::from(items)))
}

fn agent_kind(id: &str) -> AgentKind {
    AgentKind::from_agent_id(id).unwrap_or(AgentKind::ClaudeCode)
}

fn scope_of(id: &str) -> Scope {
    if id == "workspace" {
        Scope::Workspace
    } else {
        Scope::User
    }
}

fn concept_of(id: &str) -> Option<Concept> {
    Some(match id {
        "mcp" => Concept::Mcp,
        "skill" => Concept::Skill,
        "command" => Concept::Command,
        "hook" => Concept::Hook,
        "subagent" => Concept::Subagent,
        _ => return None,
    })
}

/// Bundle for the stored language; `system` follows the OS locale.
fn bundle_for(language: Language) -> &'static str {
    let zh = match language {
        Language::ZhCn => true,
        Language::EnUs => false,
        Language::System => {
            sys_locale::get_locale().is_some_and(|l| l.to_ascii_lowercase().starts_with("zh"))
        }
    };
    if zh {
        "zh_CN"
    } else {
        ""
    }
}

fn diff_lines(diff: &str) -> Vec<DiffLine> {
    diff.lines()
        .map(|line| {
            let kind =
                if line.starts_with("+++") || line.starts_with("---") || line.starts_with("@@") {
                    3
                } else if line.starts_with('+') {
                    1
                } else if line.starts_with('-') {
                    2
                } else {
                    0
                };
            DiffLine {
                kind,
                text: sv(line),
            }
        })
        .collect()
}

fn opt(value: Option<&String>) -> String {
    value.cloned().unwrap_or_default()
}

fn non_empty(value: &str) -> Option<String> {
    let trimmed = value.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

fn lines_of(value: &str) -> Vec<String> {
    value
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect()
}

fn pairs(value: &str, sep: char) -> BTreeMap<String, String> {
    lines_of(value)
        .into_iter()
        .filter_map(|l| {
            l.split_once(sep)
                .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
        })
        .collect()
}

fn transport_id(t: McpTransport) -> &'static str {
    match t {
        McpTransport::Stdio => "stdio",
        McpTransport::Http => "http",
        McpTransport::Sse => "sse",
    }
}

/// Agent ids of the `agents` form field (comma or newline separated); empty = all agents.
fn agent_ids(value: &str) -> Vec<String> {
    value
        .split(['\n', ','])
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

/// Form fields of `body`. `overlay` = the Harness MCP overlay target, the only config whose entries know which
/// agent ids receive a server (`McpServer::agents`).
fn fields_of(body: &EntryBody, overlay: bool) -> Vec<(String, String)> {
    let f = |k: &str, v: String| (k.to_string(), v);
    match body {
        // Harness overlay only: the CLI configs have no per-agent filter.
        EntryBody::Mcp(s) if overlay => {
            let mut fields = fields_of(body, false);
            fields.push(f("agents", s.agents.join(", ")));
            fields
        }
        EntryBody::Mcp(s) => vec![
            f("name", s.name.clone()),
            f("transport", transport_id(s.transport).into()),
            f("command", opt(s.command.as_ref())),
            f("args", s.args.join("\n")),
            f(
                "env",
                s.env
                    .iter()
                    .map(|(k, v)| format!("{k}={v}"))
                    .collect::<Vec<_>>()
                    .join("\n"),
            ),
            f("url", opt(s.url.as_ref())),
            f(
                "headers",
                s.headers
                    .iter()
                    .map(|(k, v)| format!("{k}: {v}"))
                    .collect::<Vec<_>>()
                    .join("\n"),
            ),
        ],
        EntryBody::Skill(s) => vec![
            f("name", s.name.clone()),
            f("description", opt(s.description.as_ref())),
            f("body", s.body.clone()),
        ],
        EntryBody::Command(c) => vec![
            f("name", c.name.clone()),
            f("description", opt(c.description.as_ref())),
            f("argumentHint", opt(c.argument_hint.as_ref())),
            f("model", opt(c.model.as_ref())),
            f("body", c.body.clone()),
        ],
        EntryBody::Hook(h) => vec![
            f("event", h.event.clone()),
            f("matcher", opt(h.matcher.as_ref())),
            f("command", h.command.clone()),
            f(
                "timeout",
                h.timeout.map(|t| t.to_string()).unwrap_or_default(),
            ),
            f("statusMessage", opt(h.status_message.as_ref())),
            f("async", h.run_async.unwrap_or(false).to_string()),
        ],
        EntryBody::Subagent(s) => vec![
            f("name", s.name.clone()),
            f("description", opt(s.description.as_ref())),
            f("tools", opt(s.tools.as_ref())),
            f("model", opt(s.model.as_ref())),
            f("color", opt(s.color.as_ref())),
            f("systemPrompt", s.body.clone()),
        ],
        EntryBody::Instructions { .. } => Vec::new(),
    }
}

fn default_body(concept: Concept) -> EntryBody {
    match concept {
        Concept::Mcp => EntryBody::Mcp(McpServer::default()),
        Concept::Skill => EntryBody::Skill(Skill::default()),
        Concept::Command => EntryBody::Command(CmdEntry::default()),
        Concept::Hook => EntryBody::Hook(Hook {
            event: "PreToolUse".into(),
            hook_type: "command".into(),
            ..Hook::default()
        }),
        Concept::Subagent | Concept::Instructions => EntryBody::Subagent(Subagent::default()),
    }
}

fn body_from(base: EntryBody, fields: &[(String, String)], overlay: bool) -> EntryBody {
    let get = |k: &str| {
        fields
            .iter()
            .find(|(key, _)| key == k)
            .map(|(_, v)| v.clone())
            .unwrap_or_default()
    };
    match base {
        EntryBody::Mcp(mut s) => {
            s.name = get("name").trim().to_string();
            s.transport = match get("transport").as_str() {
                "http" => McpTransport::Http,
                "sse" => McpTransport::Sse,
                _ => McpTransport::Stdio,
            };
            s.command = non_empty(&get("command"));
            s.args = lines_of(&get("args"));
            s.env = pairs(&get("env"), '=');
            s.url = non_empty(&get("url"));
            s.headers = pairs(&get("headers"), ':');
            // Harness overlay only: keep `agents` untouched for the CLI configs, which never write it.
            if overlay {
                s.agents = agent_ids(&get("agents"));
            }
            EntryBody::Mcp(s)
        }
        EntryBody::Skill(mut s) => {
            s.name = get("name").trim().to_string();
            s.description = non_empty(&get("description"));
            s.body = get("body");
            EntryBody::Skill(s)
        }
        EntryBody::Command(mut c) => {
            c.name = get("name").trim().to_string();
            c.description = non_empty(&get("description"));
            c.argument_hint = non_empty(&get("argumentHint"));
            c.model = non_empty(&get("model"));
            c.body = get("body");
            EntryBody::Command(c)
        }
        EntryBody::Hook(mut h) => {
            h.event = get("event");
            h.matcher = non_empty(&get("matcher"));
            h.command = get("command");
            h.timeout = get("timeout").trim().parse::<f64>().ok();
            h.status_message = non_empty(&get("statusMessage"));
            h.run_async = (get("async") == "true").then_some(true);
            if h.hook_type.is_empty() {
                h.hook_type = "command".into();
            }
            EntryBody::Hook(h)
        }
        EntryBody::Subagent(mut s) => {
            s.name = get("name").trim().to_string();
            s.description = non_empty(&get("description"));
            s.tools = non_empty(&get("tools"));
            s.model = non_empty(&get("model"));
            s.color = non_empty(&get("color"));
            s.body = get("systemPrompt");
            EntryBody::Subagent(s)
        }
        other @ EntryBody::Instructions { .. } => other,
    }
}

impl State {
    fn send(&self, req: Req) {
        // worker 已退出只会发生在关停阶段。
        let _ = self.req.send(req);
    }

    pub(super) fn status(&self, text: &str, error: bool) {
        let ui = self.window.global::<SettingsUi>();
        ui.set_status(sv(text));
        ui.set_status_error(error);
    }

    pub(super) fn set_pending_dir(&self, dir: &str) {
        self.window
            .global::<SettingsUi>()
            .set_pending_data_dir(sv(dir));
    }

    pub(super) fn apply_snapshot(&mut self, settings: Arc<Settings>) {
        let json = serde_json::to_value(&*settings).unwrap_or(Value::Null);
        SNAP.with(|cell| *cell.borrow_mut() = json);
        KEYMAP.with(|cell| *cell.borrow_mut() = Some(Keymap::new(&settings.shortcuts)));
        let theme = self.window.global::<Theme>();
        theme.set_pref(match settings.appearance.theme {
            harness_settings::ThemePref::System => 0,
            harness_settings::ThemePref::Light => 1,
            harness_settings::ThemePref::Dark => 2,
        });
        let zoom = f32::from(settings.appearance.zoom_percent) / 100.0;
        theme.set_ui_font_size(f32::from(settings.appearance.ui_font_size) * zoom);
        theme.set_code_size(f32::from(settings.appearance.code_font_size) * zoom);
        theme.set_reduce_motion(settings.appearance.reduce_motion);
        // "#7C8CFF" was the old default accent and is no longer offered: treat it as "use the default".
        let accent = if settings.appearance.accent_color.eq_ignore_ascii_case("#7C8CFF") {
            "#C8F04A"
        } else {
            settings.appearance.accent_color.as_str()
        };
        if let Some(c) = parse_hex(accent) {
            theme.set_accent_pick(c);
        }
        let look = self.window.global::<crate::ui::Look>();
        look.set_thinking(settings.appearance.thinking_style.as_str().into());
        look.set_words(settings.appearance.status_words.as_str().into());
        sync_launch_at_startup(settings.general.launch_at_startup);
        let bundle = bundle_for(settings.general.language);
        if slint::select_bundled_translation(bundle).is_err() {
            self.status("language unavailable", true);
        }
        self.window.set_ui_font_family(
            crate::bridge::ui_font_family(bundle == "zh_CN", &settings.appearance.ui_font).into(),
        );
        self.window
            .global::<crate::ui::ChatUi>()
            .set_auto_compact(settings.general.auto_compact);
        slint::Timer::single_shot(
            std::time::Duration::ZERO,
            crate::bridge::refresh_usage_parts,
        );
        self.settings = settings;
        self.rev = self.rev.wrapping_add(1);
        self.window.global::<SettingsUi>().set_rev(self.rev);
        self.refresh_shortcuts();
        self.refresh_cards();
    }

    fn patch(&self, path: &str, value: Value) {
        self.send(Req::Patch {
            path: path.to_string(),
            value,
        });
    }

    fn open_page(&mut self, id: &str) {
        let ui = self.window.global::<SettingsUi>();
        ui.set_page(sv(id));
        ui.set_narrow_nav(false);
        ui.set_status(SharedString::new());
        match id {
            "agents" => {
                self.send(Req::Agents(
                    self.agents.iter().map(|a| a.0.clone()).collect(),
                ));
                crate::bridge::request_agent_options();
                crate::bridge::send(crate::core::Command::AgentsOverview);
                crate::bridge::send(crate::core::Command::Accounts(
                    crate::core::AccountsCall::List,
                ));
                crate::bridge::send(crate::core::Command::Providers(
                    crate::core::ProvidersCall::List,
                ));
                agy::refresh();
                self.refresh_cards();
            }
            "orchestra" => crate::orchestra::open(),
            "inbox" => crate::inbox::open(),
            "import" => crate::import::open(),
            "remote" => crate::remote::open(),
            "shortcuts" => self.refresh_shortcuts(),
            "instructions" | "memory" => {
                let doc = self.window.global::<DocUi>();
                doc.set_kind(sv(id));
                doc.set_query(SharedString::new());
                doc.set_matches(model(Vec::new()));
                self.load_doc();
            }
            "projects" => {
                self.selected.clear();
                self.removed.clear();
                self.window.global::<ProjectsUi>().set_undo_count(0);
                self.refresh_projects();
            }
            "archive" => self.refresh_archive(0),
            concept if concept_of(concept).is_some() => {
                let cfg = self.window.global::<ConfigUi>();
                cfg.set_concept(sv(concept));
                if cfg.get_agent() == "harness" && concept != "mcp" && concept != "skill" {
                    cfg.set_agent(sv("claude-code"));
                }
                cfg.set_query(SharedString::new());
                self.close_editor();
                self.reload_config();
            }
            _ => {}
        }
    }

    // ---- shortcuts ---------------------------------------------------------------------------------------

    fn refresh_shortcuts(&self) {
        let ui = self.window.global::<ShortcutsUi>();
        let labels = self.window.global::<Labels>();
        let query = ui.get_query().to_lowercase();
        let key = ui.get_key_query().to_string();
        let key_search = ui.get_key_search();
        let rows = keymap(|k| {
            k.rows
                .iter()
                .filter(|(id, _, bindings, _)| {
                    if key_search && !key.is_empty() {
                        return bindings.iter().any(|b| keymap::same(b, &key));
                    }
                    query.is_empty()
                        || labels
                            .invoke_command_label(sv(id))
                            .to_lowercase()
                            .contains(&query)
                        || id.to_lowercase().contains(&query)
                        || bindings
                            .iter()
                            .any(|b| keymap::display(b).to_lowercase().contains(&query))
                })
                .map(|(id, scope, bindings, custom)| ShortcutRow {
                    id: sv(id),
                    scope: sv(if *scope == keymap::Scope::Composer {
                        "composer"
                    } else {
                        "global"
                    }),
                    bindings: model(bindings.iter().map(sv).collect()),
                    custom: *custom,
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
        // Settings S6 groups rows under caps headings: global commands first, composer keys last (stable).
        let mut rows = rows;
        rows.sort_by_key(|r| r.scope == "composer");
        ui.set_rows(model(rows));
    }

    fn save_shortcuts(&mut self, edit: impl FnOnce(&mut BTreeMap<String, Vec<String>>)) {
        let mut map = self.settings.shortcuts.clone();
        edit(&mut map);
        // 乐观更新本地键位表，store 的 watch 稍后给出权威值。
        KEYMAP.with(|cell| *cell.borrow_mut() = Some(Keymap::new(&map)));
        let mut next = (*self.settings).clone();
        next.shortcuts.clone_from(&map);
        self.settings = Arc::new(next);
        self.send(Req::Shortcuts(map));
        self.refresh_shortcuts();
    }

    fn stop_recording(&self) {
        let ui = self.window.global::<ShortcutsUi>();
        ui.set_recording(SharedString::new());
        ui.set_recorded(SharedString::new());
        ui.set_error(SharedString::new());
        ui.set_conflict(SharedString::new());
    }

    #[allow(clippy::fn_params_excessive_bools)] // Slint KeyEvent modifiers, one flag each.
    fn key(&mut self, text: &str, ctrl: bool, alt: bool, shift: bool, meta: bool) -> bool {
        let ui = self.window.global::<ShortcutsUi>();
        let id = ui.get_recording().to_string();
        let press = keymap::press(text, ctrl, alt, shift, meta);
        if id.is_empty() {
            if !ui.get_key_search() {
                return false;
            }
            match press {
                Press::Escape => {
                    ui.set_key_search(false);
                    ui.set_key_query(SharedString::new());
                }
                Press::Binding(binding) => ui.set_key_query(sv(binding)),
                _ => return true,
            }
            self.refresh_shortcuts();
            return true;
        }
        match press {
            Press::Escape => self.stop_recording(),
            Press::Backspace => {
                self.save_shortcuts(|m| {
                    m.remove(&id);
                });
                self.stop_recording();
            }
            Press::ModifierOnly => {}
            Press::Invalid => {
                ui.set_error(sv("invalidKey"));
                ui.set_conflict(SharedString::new());
            }
            Press::Binding(binding) => {
                let verdict = keymap(|k| keymap::check(k, &id, &binding)).unwrap_or(Verdict::Ok);
                ui.set_recorded(sv(&binding));
                match verdict {
                    Verdict::Ok => {
                        self.save_shortcuts(|m| {
                            m.insert(id.clone(), vec![binding]);
                        });
                        self.stop_recording();
                    }
                    Verdict::Error(code) => {
                        ui.set_error(sv(code));
                        ui.set_conflict(SharedString::new());
                    }
                    Verdict::Occupied(owner) => {
                        ui.set_error(sv("occupied"));
                        ui.set_conflict(sv(owner));
                    }
                }
            }
        }
        true
    }

    fn steal(&mut self) {
        let ui = self.window.global::<ShortcutsUi>();
        let id = ui.get_recording().to_string();
        let owner = ui.get_conflict().to_string();
        let binding = ui.get_recorded().to_string();
        if id.is_empty() || owner.is_empty() || binding.is_empty() {
            return;
        }
        let owner_bindings: Vec<String> = keymap(|k| k.bindings(&owner))
            .unwrap_or_default()
            .into_iter()
            .filter(|b| !keymap::same(b, &binding))
            .collect();
        self.save_shortcuts(|m| {
            m.insert(owner, owner_bindings);
            m.insert(id, vec![binding]);
        });
        self.stop_recording();
    }

    fn run_command(&self, id: &str) {
        let zoom = i64::from(self.settings.appearance.zoom_percent);
        match id {
            "openSettings" => self.window.set_settings_open(true),
            "switchTheme" => {
                let dark = self.window.global::<Theme>().get_dark();
                self.patch("appearance.theme", Value::from(if dark { 1 } else { 2 }));
            }
            "zoomIn" => self.patch("appearance.zoomPercent", Value::from((zoom + 10).min(150))),
            "zoomOut" => self.patch("appearance.zoomPercent", Value::from((zoom - 10).max(80))),
            "resetZoom" => self.patch("appearance.zoomPercent", Value::from(100)),
            "toggleSidePane" => self.window.set_files_open(!self.window.get_files_open()),
            "toggleAutoCompact" => self.patch(
                "general.autoCompact",
                Value::from(!self.settings.general.auto_compact),
            ),
            "newTask" => {
                self.window.set_settings_open(false);
                self.window.invoke_new_session();
            }
            // 其余命令（命令中心、查找、导航、导出…）由 src/chat 处理；没有对应功能的命令什么也不做。
            other => {
                let other = other.to_string();
                // 在当前借用结束后再交给 chat，避免 chat 回调设置页时重入。
                slint::Timer::single_shot(std::time::Duration::ZERO, move || {
                    crate::chat::command(&other);
                });
            }
        }
    }

    // ---- agents ------------------------------------------------------------------------------------------

    fn refresh_cards(&self) {
        let cards: Vec<AgentCard> = self
            .agents
            .iter()
            .map(|(id, name, installed)| {
                let prefs = self.settings.agent(id);
                let empty = AgentOptions::default();
                let opts = self.options.get(id).unwrap_or(&empty);
                let options = |list: &[(String, String)]| {
                    model(
                        list.iter()
                            .map(|(id, label)| SelectOption {
                                id: sv(id),
                                label: sv(label),
                            })
                            .collect(),
                    )
                };
                let label = |list: &[(String, String)], current: &Option<String>| {
                    current.as_ref().map_or_else(String::new, |c| {
                        list.iter()
                            .find(|(id, _)| id == c)
                            .map_or_else(|| c.clone(), |(_, l)| l.clone())
                    })
                };
                let (program, args, env) = self.launch.get(id).cloned().unwrap_or_default();
                AgentCard {
                    id: sv(id),
                    name: sv(name),
                    installed: *installed,
                    adapter_missing: !*installed && self.adapter_missing.contains(id),
                    version: sv(self.overview.get(id).map_or("", |o| o.0.as_str())),
                    auth: self
                        .overview
                        .get(id)
                        .map(|o| o.1)
                        .or_else(|| self.auth.get(id).copied())
                        .unwrap_or(2),
                    enabled: prefs.enabled,
                    mode: sv(opt(prefs.default_mode.as_ref())),
                    model: sv(opt(prefs.default_model.as_ref())),
                    effort: sv(opt(prefs.default_effort.as_ref())),
                    modes: options(&opts.modes),
                    models: options(&opts.models),
                    efforts: options(&opts.efforts),
                    mode_label: sv(label(&opts.modes, &prefs.default_mode)),
                    model_label: sv(label(&opts.models, &prefs.default_model)),
                    effort_label: sv(label(&opts.efforts, &prefs.default_effort)),
                    program: sv(program),
                    args: sv(args),
                    env: sv(env),
                    test_state: self.tests.get(id).map_or(0, |t| t.0),
                    test_latency: self.tests.get(id).map_or(0, |t| t.1),
                    test_error: sv(self.tests.get(id).map_or("", |t| t.2.as_str())),
                }
            })
            .collect();
        self.window.global::<AgentsUi>().set_cards(model(cards));
        let disabled: Vec<String> = self
            .agents
            .iter()
            .filter(|(id, _, _)| !self.settings.agent(id).enabled)
            .map(|(id, _, _)| id.clone())
            .collect();
        slint::Timer::single_shot(std::time::Duration::ZERO, move || {
            crate::bridge::set_disabled_agents(disabled);
        });
    }

    pub(super) fn agents_loaded(
        &mut self,
        auth: Vec<(String, i32)>,
        launch: Vec<(String, String, String, String)>,
    ) {
        self.auth = auth.into_iter().collect();
        self.launch = launch
            .into_iter()
            .map(|(id, program, args, env)| (id, (program, args, env)))
            .collect();
        self.refresh_cards();
    }

    // ---- agent config (MCP, skills, commands, hooks, subagents) ------------------------------------------

    fn target(&self) -> (Option<Concept>, ConfigTarget) {
        let cfg = self.window.global::<ConfigUi>();
        (
            concept_of(&cfg.get_concept()),
            ConfigTarget::new(agent_kind(&cfg.get_agent()), scope_of(&cfg.get_scope())),
        )
    }

    fn reload_config(&mut self) {
        let (Some(concept), target) = self.target() else {
            return;
        };
        self.seq += 1;
        let cfg = self.window.global::<ConfigUi>();
        cfg.set_loading(true);
        cfg.set_error(SharedString::new());
        self.send(Req::List {
            seq: self.seq,
            concept,
            target,
        });
    }

    pub(super) fn config_listed(
        &mut self,
        seq: u64,
        support: Option<ConceptSupport>,
        result: Result<Vec<Entry>, String>,
    ) {
        if seq != self.seq {
            return;
        }
        let cfg = self.window.global::<ConfigUi>();
        cfg.set_loading(false);
        let scope = scope_of(&cfg.get_scope());
        let supported = support.as_ref().is_some_and(|c| c.user || c.workspace);
        let scope_supported = support.as_ref().is_some_and(|c| match scope {
            Scope::User => c.user,
            Scope::Workspace => c.workspace,
        });
        cfg.set_supported(supported);
        cfg.set_scope_supported(scope_supported);
        cfg.set_can_disable(support.as_ref().is_some_and(|c| c.can_disable));
        cfg.set_reason(sv(support
            .as_ref()
            .and_then(|c| c.reason.clone())
            .unwrap_or_default()));
        self.support = support;
        match result {
            Ok(entries) => self.entries = entries,
            Err(error) => {
                self.entries.clear();
                if supported && scope_supported {
                    cfg.set_error(sv(&error));
                    self.status(&error, true);
                }
            }
        }
        self.refresh_rows();
    }

    fn refresh_rows(&self) {
        let cfg = self.window.global::<ConfigUi>();
        let query = cfg.get_query().to_lowercase();
        let builtins: &[(&str, &str)] =
            if cfg.get_concept() == "mcp" && cfg.get_agent() == "harness" {
                &BUILTIN_MCP
            } else {
                &[]
            };
        let builtin_rows = builtins
            .iter()
            .filter(|(name, summary)| {
                query.is_empty() || name.contains(&query) || summary.to_lowercase().contains(&query)
            })
            .map(|(name, summary)| ResRow {
                name: sv(*name),
                scope: sv("user"),
                enabled: true,
                toggleable: false,
                summary: sv(*summary),
                path: SharedString::new(),
                group: sv("Added by Harness to every session"),
                builtin: true,
                detail: sv("stdio"),
                extra: SharedString::new(),
            });
        let rows: Vec<ResRow> = builtin_rows
            .chain(
                self.entries
                    .iter()
                    .filter(|e| {
                        query.is_empty()
                            || e.name.to_lowercase().contains(&query)
                            || e.summary.to_lowercase().contains(&query)
                    })
                    .map(|e| ResRow {
                        name: sv(&e.name),
                        scope: sv(match e.scope {
                            Scope::User => "user",
                            Scope::Workspace => "workspace",
                        }),
                        enabled: e.enabled.unwrap_or(true),
                        toggleable: e.enabled.is_some(),
                        summary: sv(&e.summary),
                        path: sv(e.source_path.display().to_string()),
                        group: sv(match &e.body {
                            EntryBody::Hook(h) => h.event.clone(),
                            _ if builtins.is_empty() => String::new(),
                            _ => "Configured".to_owned(),
                        }),
                        builtin: false,
                        detail: sv(match &e.body {
                            EntryBody::Mcp(m) => transport_id(m.transport).to_owned(),
                            EntryBody::Subagent(a) => opt(a.model.as_ref()),
                            EntryBody::Command(c) => opt(c.argument_hint.as_ref()),
                            EntryBody::Hook(h) => opt(h.matcher.as_ref()),
                            _ => String::new(),
                        }),
                        extra: sv(match &e.body {
                            EntryBody::Subagent(a) => opt(a.tools.as_ref()),
                            EntryBody::Hook(h) => h.command.clone(),
                            _ => String::new(),
                        }),
                    }),
            )
            .collect();
        // Hooks page: one card per event, in first-seen order.
        let mut groups: Vec<ResGroup> = Vec::new();
        for r in &rows {
            match groups.iter_mut().find(|g| g.name == r.group) {
                Some(g) => g.count += 1,
                None => groups.push(ResGroup {
                    name: r.group.clone(),
                    count: 1,
                }),
            }
        }
        cfg.set_groups(model(groups));
        cfg.set_rows(model(rows));
    }

    fn show_form(&self) {
        let cfg = self.window.global::<ConfigUi>();
        let Some(edit) = &self.edit else {
            cfg.set_form(model(Vec::new()));
            return;
        };
        let transports: Vec<SelectOption> = self
            .support
            .as_ref()
            .map(|s| s.transports.clone())
            .unwrap_or_default()
            .into_iter()
            .map(|t| SelectOption {
                id: sv(transport_id(t)),
                label: sv(transport_id(t)),
            })
            .collect();
        let form: Vec<FormField> = edit
            .fields
            .iter()
            .map(|(key, value)| {
                let (kind, options) = match key.as_str() {
                    "transport" => ("select", transports.clone()),
                    "event" => (
                        "select",
                        HOOK_EVENTS
                            .iter()
                            .map(|e| SelectOption {
                                id: sv(e),
                                label: sv(e),
                            })
                            .collect(),
                    ),
                    "args" | "env" | "headers" | "body" | "systemPrompt" => {
                        ("multiline", Vec::new())
                    }
                    "command" | "url" | "matcher" => ("mono", Vec::new()),
                    "async" => ("switch", Vec::new()),
                    _ => ("text", Vec::new()),
                };
                FormField {
                    key: sv(key),
                    value: sv(value),
                    kind: sv(kind),
                    options: model(options),
                    enabled: true,
                }
            })
            .collect();
        cfg.set_form(model(form));
    }

    fn open_editor(&mut self, name: Option<&str>) {
        let (Some(concept), target) = self.target() else {
            return;
        };
        // Only the Harness overlay carries the per-agent filter (`agents`).
        let is_overlay = target.agent == AgentKind::Harness;
        let cfg = self.window.global::<ConfigUi>();
        let entry = name.and_then(|n| self.entries.iter().find(|e| e.name == n).cloned());
        let base = entry
            .as_ref()
            .map_or_else(|| default_body(concept), |e| e.body.clone());
        self.edit = Some(Edit {
            previous: entry.as_ref().map(|e| e.name.clone()),
            fields: fields_of(&base, is_overlay),
            base: Some(base),
        });
        cfg.set_editing(true);
        cfg.set_is_new(entry.is_none());
        cfg.set_raw_mode(false);
        cfg.set_edit_name(sv(name.unwrap_or_default()));
        cfg.set_error(SharedString::new());
        cfg.set_raw(SharedString::new());
        let path = entry.map(|e| e.source_path);
        cfg.set_raw_path(sv(path
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_default()));
        if let Some(path) = path {
            self.seq += 1;
            self.send(Req::ReadRaw {
                seq: self.seq,
                path,
            });
        }
        self.show_form();
    }

    pub(super) fn raw_loaded(&self, seq: u64, text: Result<String, String>) {
        if seq != self.seq {
            return;
        }
        let cfg = self.window.global::<ConfigUi>();
        match text {
            Ok(text) => cfg.set_raw(sv(text)),
            Err(error) => cfg.set_error(sv(error)),
        }
    }

    fn close_editor(&mut self) {
        self.edit = None;
        let cfg = self.window.global::<ConfigUi>();
        cfg.set_editing(false);
        cfg.set_raw_mode(false);
        cfg.set_error(SharedString::new());
        cfg.set_form(model(Vec::new()));
    }

    fn save_editor(&mut self) {
        let (_, target) = self.target();
        // Only the Harness overlay carries the per-agent filter (`agents`).
        let is_overlay = target.agent == AgentKind::Harness;
        let cfg = self.window.global::<ConfigUi>();
        let Some(edit) = &self.edit else {
            return;
        };
        let op = if cfg.get_raw_mode() && !cfg.get_raw_path().is_empty() {
            PlanOp::Raw {
                path: PathBuf::from(cfg.get_raw_path().as_str()),
                text: cfg.get_raw().to_string(),
            }
        } else {
            let Some(base) = edit.base.clone() else {
                return;
            };
            PlanOp::Upsert {
                target,
                body: Box::new(body_from(base, &edit.fields, is_overlay)),
                previous: edit.previous.clone(),
            }
        };
        self.after = After::Config;
        self.send(Req::Plan { title: "save", op });
    }

    // ---- diff confirmation -------------------------------------------------------------------------------

    pub(super) fn show_diff(&self, title: &str, path: &str, diff: &str, no_op: bool) {
        let ui = self.window.global::<DiffUi>();
        ui.set_title(sv(title));
        ui.set_path(sv(path));
        ui.set_lines(model(diff_lines(diff)));
        ui.set_no_op(no_op);
        ui.set_error(SharedString::new());
        ui.set_open(true);
    }

    pub(super) fn plan_failed(&self, error: &str) {
        if self.window.global::<ConfigUi>().get_editing() {
            self.window.global::<ConfigUi>().set_error(sv(error));
        }
        if self.after == After::Doc {
            self.window.global::<DocUi>().set_error(sv(error));
        }
        self.status(error, true);
    }

    pub(super) fn applied(&mut self, result: Result<(), String>) {
        let diff = self.window.global::<DiffUi>();
        match result {
            Ok(()) => {
                diff.set_open(false);
                diff.set_lines(model(Vec::new()));
                match self.after {
                    After::Config => {
                        self.close_editor();
                        self.reload_config();
                    }
                    After::Doc => self.load_doc(),
                }
            }
            Err(error) => diff.set_error(sv(error)),
        }
    }

    // ---- instructions / memory ---------------------------------------------------------------------------

    fn load_doc(&mut self) {
        let doc = self.window.global::<DocUi>();
        let scope = scope_of(&doc.get_scope());
        let kind = if doc.get_kind() == "memory" {
            DocKind::Memory
        } else {
            DocKind::Instructions(agent_kind(&doc.get_agent()))
        };
        self.seq += 1;
        doc.set_error(SharedString::new());
        doc.set_saved(SharedString::new());
        self.send(Req::Doc {
            seq: self.seq,
            kind,
            scope,
        });
    }

    pub(super) fn doc_loaded(&mut self, seq: u64, loaded: Result<Doc, String>) {
        if seq != self.seq {
            return;
        }
        let ui = self.window.global::<DocUi>();
        match loaded {
            Ok(doc) => {
                ui.set_supported(doc.supported);
                ui.set_path(sv(doc.path.display().to_string()));
                ui.set_exists(doc.exists);
                ui.set_text(sv(&doc.text));
                ui.set_bytes(i32::try_from(doc.text.len()).unwrap_or(i32::MAX));
                ui.set_dirty(false);
                ui.set_error(sv(&doc.reason));
                self.doc_original = doc.text;
                self.doc_path = doc.path;
                ui.set_selected(-1);
                self.search_doc();
            }
            Err(error) => {
                ui.set_supported(true);
                ui.set_error(sv(error));
            }
        }
    }

    fn search_doc(&self) {
        let ui = self.window.global::<DocUi>();
        let query = ui.get_query().to_lowercase();
        let matches: Vec<SharedString> = if query.is_empty() {
            Vec::new()
        } else {
            ui.get_text()
                .lines()
                .filter(|l| l.to_lowercase().contains(&query))
                .map(sv)
                .collect()
        };
        ui.set_matches(model(matches));
        docview::refresh(&ui, &self.doc_original, &self.doc_path);
    }

    fn save_doc(&mut self) {
        let ui = self.window.global::<DocUi>();
        let text = ui.get_text().to_string();
        let scope = scope_of(&ui.get_scope());
        let (title, op) = if ui.get_kind() == "memory" {
            ("memory", PlanOp::Memory { scope, text })
        } else {
            (
                "instructions",
                PlanOp::Instructions {
                    target: ConfigTarget::new(agent_kind(&ui.get_agent()), scope),
                    path: self.doc_path.clone(),
                    text,
                },
            )
        };
        self.after = After::Doc;
        self.send(Req::Plan { title, op });
    }

    // ---- projects ----------------------------------------------------------------------------------------

    fn refresh_projects(&mut self) {
        self.list_seq += 1;
        let query = self.window.global::<ProjectsUi>().get_query().to_string();
        self.send(Req::Projects {
            seq: self.list_seq,
            query,
        });
    }

    pub(super) fn projects_listed(
        &mut self,
        seq: u64,
        result: Result<Vec<api::ProjectInfo>, String>,
    ) {
        if seq != self.list_seq {
            return;
        }
        let ui = self.window.global::<ProjectsUi>();
        match result {
            Ok(list) => {
                ui.set_available(true);
                self.selected
                    .retain(|k| list.iter().any(|p| &p.workspace_key == k));
                self.projects = list;
                self.show_projects();
            }
            Err(error) => {
                // hub 不可用时退回只读：不显示任何可操作的行，错误写到状态栏。
                ui.set_available(false);
                self.projects.clear();
                self.selected.clear();
                self.show_projects();
                self.status(&error, true);
            }
        }
    }

    fn show_projects(&self) {
        let ui = self.window.global::<ProjectsUi>();
        let unused_only = ui.get_unused_only();
        let open = crate::bridge::workspace_path();
        let rows: Vec<ProjectRow> = self
            .projects
            .iter()
            .filter(|p| !unused_only || p.unused)
            .map(|p| ProjectRow {
                id: sv(&p.workspace_key),
                name: sv(folder_name(&p.workspace_path)),
                path: sv(&p.workspace_path),
                short: sv(short_path(&p.workspace_path)),
                sessions: i32::try_from(p.sessions).unwrap_or(i32::MAX),
                last_used: sv(p.last_used.map(date_of).unwrap_or_default()),
                unused: p.unused,
                selected: self.selected.contains(&p.workspace_key),
                current: same_path(&p.workspace_path, &open),
            })
            .collect();
        let unused = self.projects.iter().filter(|p| p.unused).count();
        ui.set_unused_count(i32::try_from(unused).unwrap_or(i32::MAX));
        let total: u64 = self.projects.iter().map(|p| p.sessions as u64).sum();
        ui.set_total_sessions(i32::try_from(total).unwrap_or(i32::MAX));
        ui.set_selected(i32::try_from(self.selected.len()).unwrap_or(i32::MAX));
        ui.set_rows(model(rows));
    }

    fn select_only_project(&mut self, key: &str) {
        self.selected.clear();
        self.selected.insert(key.to_string());
        self.show_projects();
    }

    fn select_project(&mut self, key: &str, on: bool) {
        if on {
            self.selected.insert(key.to_string());
        } else {
            self.selected.remove(key);
        }
        self.show_projects();
    }

    fn remove_projects(&mut self, delete_data: bool) {
        let keys: Vec<String> = self.selected.iter().cloned().collect();
        if keys.is_empty() {
            return;
        }
        self.send(Req::RemoveProjects { keys, delete_data });
    }

    pub(super) fn projects_removed(
        &mut self,
        keys: Vec<String>,
        delete_data: bool,
        result: Result<(), String>,
    ) {
        match result {
            Ok(()) => {
                self.selected.clear();
                // 删除数据不可撤销，只有“移除”提供撤销。
                self.removed = if delete_data { Vec::new() } else { keys };
                let undo = i32::try_from(self.removed.len()).unwrap_or(i32::MAX);
                self.window.global::<ProjectsUi>().set_undo_count(undo);
                self.refresh_projects();
            }
            Err(error) => self.status(&error, true),
        }
    }

    fn undo_remove(&mut self) {
        let keys = std::mem::take(&mut self.removed);
        self.window.global::<ProjectsUi>().set_undo_count(0);
        if !keys.is_empty() {
            self.send(Req::RestoreProjects(keys));
        }
    }

    pub(super) fn projects_restored(&mut self, result: Result<(), String>) {
        if let Err(error) = result {
            self.status(&error, true);
        }
        self.refresh_projects();
    }

    // ---- archive -----------------------------------------------------------------------------------------

    fn refresh_archive(&mut self, offset: u32) {
        self.list_seq += 1;
        let query = self.window.global::<ArchiveUi>().get_query().to_string();
        self.send(Req::Archived {
            seq: self.list_seq,
            query,
            offset,
        });
    }

    pub(super) fn archive_listed(
        &mut self,
        seq: u64,
        offset: u32,
        result: Result<api::ArchivePage, String>,
    ) {
        if seq != self.list_seq {
            return;
        }
        let ui = self.window.global::<ArchiveUi>();
        match result {
            Ok(page) => {
                if offset == 0 {
                    self.archive.clear();
                }
                let names = crate::bridge::agent_names();
                self.archive.extend(page.rows.into_iter().map(|a| {
                    ArchiveRow {
                        id: sv(&a.summary.id.0),
                        title: sv(&a.summary.title),
                        project: sv(folder_name(&a.summary.workspace_path)),
                        updated: sv(date_of(a.archived_at)),
                        agent: sv(names
                            .iter()
                            .find(|(id, _)| *id == a.summary.active_agent.to_string())
                            .map_or_else(
                                || a.summary.active_agent.to_string(),
                                |(_, n)| n.clone(),
                            )),
                    }
                }));
                ui.set_available(true);
                ui.set_has_more(page.has_more);
                ui.set_rows(model(self.archive.clone()));
            }
            Err(error) => {
                ui.set_available(false);
                ui.set_has_more(false);
                self.archive.clear();
                ui.set_rows(model(Vec::new()));
                self.status(&error, true);
            }
        }
    }

    pub(super) fn archive_changed(&mut self, result: Result<(), String>) {
        if let Err(error) = result {
            self.status(&error, true);
        }
        self.refresh_archive(0);
    }

    fn page(&self) -> SharedString {
        self.window.global::<SettingsUi>().get_page()
    }
}
/// Writes one setting through the settings worker (the store's single write path), e.g. `setup.firstRunDone`.
/// "#RRGGBB" → colour (settings S2 accent swatches).
/// Last two segments of a path ("D:\\work\\ZCode\\Worse" -> "ZCode/Worse").
fn same_path(a: &str, b: &str) -> bool {
    let norm = |s: &str| {
        s.trim_end_matches(['\\', '/'])
            .replace('/', "\\")
            .to_lowercase()
    };
    !b.is_empty() && norm(a) == norm(b)
}

fn short_path(path: &str) -> String {
    let parts: Vec<&str> = path.split(['\\', '/']).filter(|s| !s.is_empty()).collect();
    let n = parts.len();
    parts[n.saturating_sub(2)..].join("/")
}

fn parse_hex(s: &str) -> Option<slint::Color> {
    let h = s.trim().strip_prefix('#')?;
    if h.len() != 6 {
        return None;
    }
    let n = u32::from_str_radix(h, 16).ok()?;
    let [_, r, g, b] = n.to_be_bytes();
    Some(slint::Color::from_rgb_u8(r, g, b))
}

/// Settings S1 "Launch at startup": mirrors the flag into HKCU\…\Run (Windows), only when it changes.
fn sync_launch_at_startup(on: bool) {
    thread_local!(static LAST: std::cell::Cell<Option<bool>> = const { std::cell::Cell::new(None) });
    let prev = LAST.with(|c| c.replace(Some(on)));
    // First snapshot after start: only write when enabling (keeps the entry pointing at the current build).
    if prev == Some(on) || (prev.is_none() && !on) {
        return;
    }
    #[cfg(windows)]
    {
        let key = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";
        let mut cmd = std::process::Command::new("reg");
        if on {
            let exe = crate::live_update::launch_exe();
            cmd.args(["add", key, "/v", "Harness", "/t", "REG_SZ", "/f", "/d"])
                .arg(format!("\"{}\"", exe.display()));
        } else {
            cmd.args(["delete", key, "/v", "Harness", "/f"]);
        }
        {
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
        }
        std::thread::spawn(move || {
            let _ = cmd.output();
        });
    }
}

/// Settings S1 "Reset all settings to defaults": app preferences only — projects, sessions, agent prefs,
/// remote access and API keys are kept.
fn reset_settings(s: &State) {
    let defaults = serde_json::to_value(Settings::default()).unwrap_or(Value::Null);
    for section in [
        "general",
        "appearance",
        "notifications",
        "updates",
        "orchestra",
    ] {
        if let Some(v) = defaults.get(section) {
            let mut v = v.clone();
            // Keep the last workspace so the next start reopens the same project.
            if section == "general" {
                if let (Some(obj), Some(last)) =
                    (v.as_object_mut(), s.settings.general.last_workspace.clone())
                {
                    obj.insert("lastWorkspace".into(), Value::from(last));
                }
            }
            s.patch(section, v);
        }
    }
    s.status("settings reset to defaults", false);
}

pub fn patch_setting(path: &str, value: Value) {
    with_state(|s| s.patch(path, value));
}

/// The last settings snapshot mirrored from the store (UI thread only).
pub fn current() -> Option<Arc<Settings>> {
    let mut out = None;
    with_state(|s| out = Some(Arc::clone(&s.settings)));
    out
}

/// Agent list from the core (bridge): (id, display name, installed).
pub fn agents_changed(list: Vec<(String, String, bool)>) {
    with_state(|s| {
        s.agents = list;
        s.refresh_cards();
    });
}

/// The session list changed (archive, unarchive, remove, new session): refresh the open data page.
/// Display text of the first binding of a keymap command ("" when unbound or unknown).
pub fn shortcut_label(id: &str) -> String {
    keymap(|k| k.bindings(id).first().map(|b| keymap::display(b)))
        .flatten()
        .unwrap_or_default()
}

/// Runs a global command (command center, slash commands) through the same dispatcher as the shortcuts.
pub fn run_command(id: &str) {
    with_state(|s| s.run_command(id));
}

pub fn sessions_changed() {
    with_state(|s| {
        // 设置页未打开时也会收到会话事件，此时不发请求。
        if !s.window.get_settings_open() {
            return;
        }
        match s.page().as_str() {
            "projects" => s.refresh_projects(),
            "archive" => s.refresh_archive(0),
            _ => {}
        }
    });
}

/// Hub Agents overview rows (version, login presence).
pub fn agents_overview(rows: Vec<api::AgentOverview>) {
    with_state(|s| {
        s.adapter_missing = rows
            .iter()
            .filter(|r| r.adapter_missing)
            .map(|r| r.id.0.clone())
            .collect();
        s.overview = rows
            .into_iter()
            .map(|r| {
                let auth = match r.auth {
                    api::AuthPresence::Present => 0,
                    api::AuthPresence::Absent => 1,
                    api::AuthPresence::Unknown => 2,
                };
                (r.id.0, (r.version.unwrap_or_default(), auth))
            })
            .collect();
        s.refresh_cards();
    });
}

/// Account list (or the error of the last account call) from the hub (phase-9-parity.md §9.6).
pub fn accounts(result: Result<Vec<api::AccountInfo>, String>) {
    with_state(|s| {
        let ui = s.window.global::<AgentsUi>();
        match result {
            Ok(rows) => {
                ui.set_account_error(SharedString::new());
                s.accounts = rows;
            }
            Err(error) => ui.set_account_error(error.into()),
        }
        let rows: Vec<AccountRow> = s
            .accounts
            .iter()
            .map(|a| AccountRow {
                id: a.id.0.clone().into(),
                label: a.label.clone().into(),
                auth: match a.auth {
                    api::AuthPresence::Present => 0,
                    api::AuthPresence::Absent => 1,
                    api::AuthPresence::Unknown => 2,
                },
                loaded: a.loaded,
            })
            .collect();
        ui.set_accounts(model(rows));
    });
}

/// Provider list (or the error of the last provider call) from the hub (phase-9-parity.md §9.7).
pub fn providers(result: Result<Vec<api::ProviderInfo>, String>) {
    with_state(|s| {
        let ui = s.window.global::<AgentsUi>();
        match result {
            Ok(rows) => {
                ui.set_provider_error(SharedString::new());
                s.providers = rows;
            }
            Err(error) => ui.set_provider_error(error.into()),
        }
        let rows: Vec<ProviderRow> = s
            .providers
            .iter()
            .map(|p| ProviderRow {
                id: p.id.0.clone().into(),
                label: p.label.clone().into(),
                model: p.model.clone().into(),
                endpoint: p.base_url.clone().into(),
                has_key: p.has_key,
                loaded: p.loaded,
            })
            .collect();
        ui.set_providers(model(rows));
    });
}

/// Opens a console that runs `claude` with the account's `CLAUDE_CONFIG_DIR`, where the user signs in
/// (`/login`). Harness never sees the credentials (§9.6).
fn open_account_login(dir: &str) -> std::io::Result<()> {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // The outer cmd has no window; `start` opens a new console for the login.
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        let child = std::process::Command::new("cmd")
            .args(["/C", "start", "Claude login", "cmd", "/K", "claude"])
            .env("CLAUDE_CONFIG_DIR", dir)
            .creation_flags(CREATE_NO_WINDOW)
            .spawn()?;
        reap(child);
        Ok(())
    }
    #[cfg(not(windows))]
    {
        let quoted = format!("'{}'", dir.replace('\'', r"'\''"));
        let script = format!("CLAUDE_CONFIG_DIR={quoted} claude");
        let child = if cfg!(target_os = "macos") {
            let apple = script.replace('\\', "\\\\").replace('"', "\\\"");
            std::process::Command::new("osascript")
                .args([
                    "-e",
                    &format!("tell application \"Terminal\" to do script \"{apple}\""),
                    "-e",
                    "tell application \"Terminal\" to activate",
                ])
                .spawn()?
        } else {
            std::process::Command::new("x-terminal-emulator")
                .args(["-e", "sh", "-c", &format!("{script}; exec sh")])
                .spawn()?
        };
        reap(child);
        Ok(())
    }
}

/// Waits for the launcher on a thread so it is reaped (it exits as soon as the console is open).
fn reap(mut child: std::process::Child) {
    std::thread::spawn(move || {
        let _ = child.wait();
    });
}

/// Result of a connection test started from the Agents page.
pub fn agent_tested(result: api::AgentTestResult) {
    with_state(|s| {
        let state = if result.ok { 2 } else { 3 };
        let latency = i32::try_from(result.latency_ms).unwrap_or(i32::MAX);
        s.tests.insert(
            result.agent.0,
            (state, latency, result.error.unwrap_or_default()),
        );
        s.refresh_cards();
    });
}

/// Mode / model / effort lists of one agent (hub `agent_options`).
#[allow(clippy::similar_names)] // modes / models are the protocol's names.
pub fn agent_options(
    agent: &str,
    modes: Vec<(String, String)>,
    models: Vec<(String, String)>,
    efforts: Vec<(String, String)>,
) {
    with_state(|s| {
        s.options.insert(
            agent.to_string(),
            AgentOptions {
                modes,
                models,
                efforts,
            },
        );
        s.refresh_cards();
    });
}

/// Installs the settings state and callbacks. `settings` is the store's snapshot at startup, so the first frame
/// already has the stored theme, language and zoom.
pub fn install(
    window: &AppWindow,
    req: mpsc::UnboundedSender<Req>,
    settings: Arc<Settings>,
    data_dir: &Path,
) {
    let ui = window.global::<SettingsUi>();
    ui.set_nav(model(
        NAV.iter()
            .map(|(id, group)| NavEntry {
                id: sv(id),
                group: *group,
            })
            .collect(),
    ));
    ui.set_data_dir(sv(data_dir.display().to_string()));
    let state = State {
        window: window.clone_strong(),
        req,
        settings: Arc::new(Settings::default()),
        rev: 0,
        seq: 0,
        agents: Vec::new(),
        auth: HashMap::new(),
        launch: HashMap::new(),
        options: HashMap::new(),
        support: None,
        entries: Vec::new(),
        edit: None,
        doc_original: String::new(),
        doc_path: PathBuf::new(),
        after: After::Config,
        overview: HashMap::new(),
        adapter_missing: HashSet::new(),
        accounts: Vec::new(),
        providers: Vec::new(),
        tests: HashMap::new(),
        list_seq: 0,
        projects: Vec::new(),
        selected: HashSet::new(),
        removed: Vec::new(),
        archive: Vec::new(),
    };
    STATE.with(|cell| {
        if let Ok(mut guard) = cell.try_borrow_mut() {
            *guard = Some(state);
        }
    });
    with_state(|s| s.apply_snapshot(settings));
    wire(window);
}

const BUILTIN_MCP: [(&str, &str); 3] = [
    (
        "harness-memory",
        "Global and project memory (MEMORY.md) and the working notes of each session",
    ),
    (
        "harness-delegate",
        "Subagent delegation and coordination between sessions",
    ),
    ("harness-computer", "Screen, mouse and keyboard control"),
];

#[allow(clippy::too_many_lines)]
fn wire(window: &AppWindow) {
    let ui = window.global::<SettingsUi>();
    ui.on_get_bool(|key, _| {
        snap(|v| {
            values::get(v, &key)
                .and_then(Value::as_bool)
                .unwrap_or(false)
        })
    });
    ui.on_get_int(|key, _| {
        snap(|v| match values::get(v, &key) {
            Some(Value::String(s)) if key == "appearance.theme" => match s.as_str() {
                "light" => 1,
                "dark" => 2,
                _ => 0,
            },
            Some(value) => value
                .as_i64()
                .and_then(|n| i32::try_from(n).ok())
                .unwrap_or(0),
            None => 0,
        })
    });
    ui.on_get_string(|key, _| {
        // Read-only app facts for the settings nav footer (not stored in settings.json).
        if key == "app.version" {
            return SharedString::from(env!("CARGO_PKG_VERSION"));
        }
        snap(|v| match values::get(v, &key) {
            Some(Value::String(s)) => sv(s),
            Some(Value::Number(n)) => sv(n.to_string()),
            _ => SharedString::new(),
        })
    });
    ui.on_set_bool(|key, on| with_state(|s| s.patch(&key, Value::from(on))));
    ui.on_set_int(|key, n| with_state(|s| s.patch(&key, Value::from(n))));
    ui.on_set_string(|key, text| with_state(|s| s.patch(&key, Value::from(text.as_str()))));
    ui.on_open_page(|id| with_state(|s| s.open_page(&id)));
    ui.on_reset_settings(|| with_state(|s| reset_settings(s)));
    ui.on_relocate_data_dir(|target| {
        with_state(|s| {
            let path = PathBuf::from(target.trim());
            if path.is_absolute() {
                s.send(Req::Relocate(path));
            } else {
                s.status("data directory must be an absolute path", true);
            }
        });
    });
    ui.on_cancel_relocation(|| with_state(|s| s.send(Req::CancelRelocation)));
    window.on_run_command(|id| with_state(|s| s.run_command(&id)));

    let keys = window.global::<ShortcutsUi>();
    keys.on_display(|b| sv(keymap::display(&b)));
    keys.on_display_keys(|b| {
        // "Ctrl+Shift+O" -> ["Ctrl", "Shift", "O"]; a literal "+" key ("Ctrl++") stays one cap.
        let text = keymap::display(&b);
        let mut caps: Vec<SharedString> = Vec::new();
        let mut rest = text.as_str();
        while !rest.is_empty() {
            match rest.find('+') {
                Some(0) => {
                    caps.push(sv("+"));
                    rest = rest[1..].strip_prefix('+').unwrap_or(&rest[1..]);
                }
                Some(i) => {
                    caps.push(sv(&rest[..i]));
                    rest = &rest[i + 1..];
                }
                None => {
                    caps.push(sv(rest));
                    rest = "";
                }
            }
        }
        model(caps)
    });
    keys.on_match(|text, ctrl, alt, shift, meta| {
        match keymap::press(&text, ctrl, alt, shift, meta) {
            Press::Binding(b) => keymap(|k| k.command_for(&b).map(sv))
                .flatten()
                .unwrap_or_default(),
            _ => SharedString::new(),
        }
    });
    keys.on_key(|text, ctrl, alt, shift, meta| {
        let mut consumed = false;
        with_state(|s| consumed = s.key(&text, ctrl, alt, shift, meta));
        consumed
    });
    keys.on_start_record(|id| {
        with_state(|s| {
            s.stop_recording();
            let ui = s.window.global::<ShortcutsUi>();
            ui.set_key_search(false);
            ui.set_recording(id);
        });
    });
    keys.on_cancel_record(|| with_state(|s| s.stop_recording()));
    keys.on_steal(|| with_state(State::steal));
    keys.on_clear(|id| {
        with_state(|s| {
            s.save_shortcuts(|m| {
                m.insert(id.to_string(), Vec::new());
            });
        });
    });
    keys.on_reset_all(|| with_state(|s| s.save_shortcuts(BTreeMap::clear)));
    keys.on_search(|_| with_state(|s| s.refresh_shortcuts()));
    keys.on_start_key_search(|| {
        with_state(|s| {
            s.stop_recording();
            let ui = s.window.global::<ShortcutsUi>();
            ui.set_key_search(true);
            ui.set_key_query(SharedString::new());
            s.refresh_shortcuts();
        });
    });
    keys.on_clear_key_search(|| {
        with_state(|s| {
            let ui = s.window.global::<ShortcutsUi>();
            ui.set_key_search(false);
            ui.set_key_query(SharedString::new());
            s.refresh_shortcuts();
        });
    });

    let agents = window.global::<AgentsUi>();
    agents.on_set_enabled(|id, on| {
        with_state(|s| s.patch(&format!("agents.{id}.enabled"), Value::from(on)));
    });
    agents.on_set_default(|id, field, value| {
        // "" = the menu's "Agent default" row: clear the stored default (null), never store an empty id.
        let value = if value.is_empty() {
            Value::Null
        } else {
            Value::from(value.as_str())
        };
        with_state(|s| s.patch(&format!("agents.{id}.{field}"), value));
    });
    agents.on_refresh(|| with_state(|s| s.open_page("agents")));
    agents.on_test(|id| {
        with_state(|s| {
            s.tests.insert(id.to_string(), (1, 0, String::new()));
            s.refresh_cards();
        });
        crate::bridge::send(crate::core::Command::TestAgent(id.to_string()));
    });
    agents.on_add_account(|label| {
        let label = label.trim().to_owned();
        if label.is_empty() {
            return;
        }
        with_state(|s| {
            s.window
                .global::<AgentsUi>()
                .set_account_label(SharedString::new());
        });
        crate::bridge::send(crate::core::Command::Accounts(
            crate::core::AccountsCall::Add(label),
        ));
    });
    agents.on_remove_account(|id| {
        crate::bridge::send(crate::core::Command::Accounts(
            crate::core::AccountsCall::Remove(id.to_string()),
        ));
    });
    agents.on_login_account(|id| {
        with_state(|s| {
            let ui = s.window.global::<AgentsUi>();
            let Some(dir) = s
                .accounts
                .iter()
                .find(|a| a.id.as_str() == id.as_str())
                .map(|a| a.profile_dir.clone())
            else {
                return;
            };
            match open_account_login(&dir) {
                Ok(()) => ui.set_account_error(SharedString::new()),
                Err(error) => ui.set_account_error(format!("open console: {error}").into()),
            }
        });
    });
    agents.on_add_provider(|| {
        let mut request = None;
        with_state(|s| {
            let ui = s.window.global::<AgentsUi>();
            let candidate = api::AddProvider {
                label: ui.get_provider_label().trim().to_owned(),
                base_url: ui.get_provider_url().trim().to_owned(),
                model: ui.get_provider_model().trim().to_owned(),
                api_key: ui.get_provider_key().trim().to_owned(),
            };
            if candidate.label.is_empty()
                || candidate.base_url.is_empty()
                || candidate.model.is_empty()
            {
                return;
            }
            // 提交后立即清空输入框，密钥不在界面里停留。
            for clear in [
                AgentsUi::set_provider_label,
                AgentsUi::set_provider_url,
                AgentsUi::set_provider_model,
                AgentsUi::set_provider_key,
            ] {
                clear(&ui, SharedString::new());
            }
            ui.set_provider_open(false);
            request = Some(candidate);
        });
        if let Some(request) = request {
            crate::bridge::send(crate::core::Command::Providers(
                crate::core::ProvidersCall::Add(request),
            ));
        }
    });
    agents.on_remove_provider(|id| {
        crate::bridge::send(crate::core::Command::Providers(
            crate::core::ProvidersCall::Remove(id.to_string()),
        ));
    });
    agents.on_refresh_accounts(|| {
        crate::bridge::send(crate::core::Command::Providers(
            crate::core::ProvidersCall::List,
        ));
        crate::bridge::send(crate::core::Command::Accounts(
            crate::core::AccountsCall::List,
        ));
        crate::bridge::send(crate::core::Command::AgentsOverview);
        agy::refresh();
    });
    agy::wire(window);

    let cfg = window.global::<ConfigUi>();
    cfg.on_select_agent(|agent| {
        with_state(|s| {
            let cfg = s.window.global::<ConfigUi>();
            cfg.set_agent(agent.clone());
            if agent == "harness" {
                cfg.set_scope(sv("user"));
            }
            s.close_editor();
            s.reload_config();
        });
    });
    cfg.on_select_scope(|scope| {
        with_state(|s| {
            s.window.global::<ConfigUi>().set_scope(scope);
            s.close_editor();
            s.reload_config();
        });
    });
    cfg.on_search(|_| with_state(|s| s.refresh_rows()));
    cfg.on_refresh(|| with_state(State::reload_config));
    cfg.on_edit(|name| with_state(|s| s.open_editor(Some(&name))));
    cfg.on_create(|| with_state(|s| s.open_editor(None)));
    cfg.on_toggle(|name, on| {
        with_state(|s| {
            let (Some(concept), target) = s.target() else {
                return;
            };
            s.after = After::Config;
            s.send(Req::Plan {
                title: "toggle",
                op: PlanOp::Toggle {
                    concept,
                    target,
                    name: name.to_string(),
                    on,
                },
            });
        });
    });
    cfg.on_remove(|name| {
        with_state(|s| {
            let (Some(concept), target) = s.target() else {
                return;
            };
            s.after = After::Config;
            s.send(Req::Plan {
                title: "delete",
                op: PlanOp::Delete {
                    concept,
                    target,
                    name: name.to_string(),
                },
            });
        });
    });
    cfg.on_copy_to_harness(|name| {
        with_state(|s| {
            let (Some(concept), from) = s.target() else {
                return;
            };
            s.after = After::Config;
            s.send(Req::Plan {
                title: "copy",
                op: PlanOp::Copy {
                    concept,
                    from,
                    to: ConfigTarget {
                        agent: AgentKind::Harness,
                        scope: Scope::User,
                    },
                    name: name.to_string(),
                },
            });
        });
    });
    cfg.on_import(|source| {
        with_state(|s| {
            let sources = match source.as_str() {
                "claude-code" => vec![AgentKind::ClaudeCode],
                "codex" => vec![AgentKind::Codex],
                "agy" => vec![AgentKind::Agy],
                _ => vec![AgentKind::ClaudeCode, AgentKind::Codex, AgentKind::Agy],
            };
            s.after = After::Config;
            s.send(Req::Plan {
                title: "import",
                op: PlanOp::ImportMcp(sources),
            });
        });
    });
    cfg.on_set_field(|key, value| {
        with_state(|s| {
            if let Some(edit) = s.edit.as_mut() {
                if let Some(field) = edit.fields.iter_mut().find(|(k, _)| *k == key.as_str()) {
                    field.1 = value.to_string();
                }
            }
        });
    });
    cfg.on_save(|| with_state(State::save_editor));
    cfg.on_close_editor(|| with_state(State::close_editor));
    cfg.on_open_folder(|_| {});

    let diff = window.global::<DiffUi>();
    diff.on_confirm(|| with_state(|s| s.send(Req::Confirm)));
    diff.on_cancel(|| {
        with_state(|s| {
            s.send(Req::Cancel);
            let diff = s.window.global::<DiffUi>();
            diff.set_open(false);
            diff.set_lines(model(Vec::new()));
        });
    });

    let doc = window.global::<DocUi>();
    doc.on_select_agent(|agent| {
        with_state(|s| {
            s.window.global::<DocUi>().set_agent(agent);
            s.load_doc();
        });
    });
    doc.on_select_scope(|scope| {
        with_state(|s| {
            s.window.global::<DocUi>().set_scope(scope);
            s.load_doc();
        });
    });
    doc.on_edited(|text| {
        with_state(|s| {
            let ui = s.window.global::<DocUi>();
            ui.set_dirty(text.as_str() != s.doc_original);
            ui.set_bytes(i32::try_from(text.len()).unwrap_or(i32::MAX));
            ui.set_saved(SharedString::new());
            s.search_doc();
        });
    });
    doc.on_save(|| with_state(State::save_doc));
    doc.on_revert(|| {
        with_state(|s| {
            let ui = s.window.global::<DocUi>();
            ui.set_text(sv(&s.doc_original));
            ui.set_dirty(false);
            ui.set_bytes(i32::try_from(s.doc_original.len()).unwrap_or(i32::MAX));
            s.search_doc();
        });
    });
    doc.on_refresh(|| with_state(State::load_doc));
    doc.on_search(|_| with_state(|s| s.search_doc()));
    // Memory entries (design S8): edit one section of MEMORY.md at a time.
    doc.on_select_entry(|index| {
        with_state(|s| {
            let ui = s.window.global::<DocUi>();
            ui.set_selected(index);
            ui.set_entry_text(sv(docview::entry_text(&ui.get_text(), index)));
        });
    });
    fn set_doc_text(s: &mut State, text: String) {
        {
            let ui = s.window.global::<DocUi>();
            ui.set_dirty(text != s.doc_original);
            ui.set_bytes(i32::try_from(text.len()).unwrap_or(i32::MAX));
            ui.set_saved(SharedString::new());
            ui.set_text(sv(text));
        }
        s.search_doc();
    }
    doc.on_entry_edited(|section| {
        with_state(|s| {
            let text = {
                let ui = s.window.global::<DocUi>();
                docview::replace_entry(&ui.get_text(), ui.get_selected(), Some(&section))
            };
            set_doc_text(s, text);
        });
    });
    doc.on_new_entry(|| {
        with_state(|s| {
            let text = {
                let ui = s.window.global::<DocUi>();
                let (text, index) = docview::append_entry(&ui.get_text());
                ui.set_selected(index);
                ui.set_entry_text(sv(docview::entry_text(&text, index)));
                text
            };
            set_doc_text(s, text);
        });
    });
    doc.on_delete_entry(|| {
        with_state(|s| {
            let text = {
                let ui = s.window.global::<DocUi>();
                let text = docview::replace_entry(&ui.get_text(), ui.get_selected(), None);
                ui.set_selected(-1);
                text
            };
            set_doc_text(s, text);
        });
    });

    let projects = window.global::<ProjectsUi>();
    projects.on_search(|_| with_state(State::refresh_projects));
    projects.on_set_unused_only(|on| {
        with_state(|s| {
            s.window.global::<ProjectsUi>().set_unused_only(on);
            s.show_projects();
        });
    });
    projects.on_select(|key, on| with_state(|s| s.select_project(&key, on)));
    projects.on_select_only(|key| with_state(|s| s.select_only_project(&key)));
    projects.on_remove_selected(|delete_data| with_state(|s| s.remove_projects(delete_data)));
    projects.on_undo_remove(|| with_state(State::undo_remove));

    let archive = window.global::<ArchiveUi>();
    archive.on_search(|_| with_state(|s| s.refresh_archive(0)));
    archive.on_unarchive(|id| with_state(|s| s.send(Req::Unarchive(id.to_string()))));
    archive.on_delete(|id| with_state(|s| s.send(Req::DeleteArchived(id.to_string()))));
    archive.on_more(|| {
        with_state(|s| {
            let offset = u32::try_from(s.archive.len()).unwrap_or(u32::MAX);
            s.refresh_archive(offset);
        });
    });
}

/// Drops the settings state (window handle and worker sender) so the worker can end.
pub fn teardown() {
    STATE.with(|cell| {
        if let Ok(mut guard) = cell.try_borrow_mut() {
            guard.take();
        }
    });
}

#[cfg(test)]
mod mcp_agents_tests {
    use super::*;

    fn server() -> EntryBody {
        EntryBody::Mcp(McpServer {
            name: "ssh".into(),
            command: Some("node".into()),
            agents: vec!["claude-code".into(), "codex".into()],
            ..McpServer::default()
        })
    }

    fn agents_of(body: &EntryBody) -> Vec<String> {
        match body {
            EntryBody::Mcp(s) => s.agents.clone(),
            _ => Vec::new(),
        }
    }

    #[test]
    fn overlay_form_round_trips_agents() {
        let mut fields = fields_of(&server(), true);
        let agents = fields.iter_mut().find(|(k, _)| k == "agents");
        assert_eq!(
            agents.as_ref().map(|(_, v)| v.as_str()),
            Some("claude-code, codex")
        );
        if let Some((_, v)) = agents {
            *v = " codex ,\napi-deepseek, ".into();
        }
        let body = body_from(server(), &fields, true);
        assert_eq!(
            agents_of(&body),
            vec!["codex".to_string(), "api-deepseek".to_string()]
        );
    }

    #[test]
    fn cli_targets_have_no_agents_field_and_keep_the_value() {
        let fields = fields_of(&server(), false);
        assert!(fields.iter().all(|(k, _)| k != "agents"));
        let body = body_from(server(), &fields, false);
        assert_eq!(
            agents_of(&body),
            vec!["claude-code".to_string(), "codex".to_string()]
        );
    }
}
