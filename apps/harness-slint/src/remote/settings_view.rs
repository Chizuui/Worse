use std::sync::{Arc, RwLock};

use harness_remote::SettingsView;
use harness_settings::Settings;
use serde::Serialize;
use serde_json::{json, Value};

static MIRROR: RwLock<Option<Arc<Settings>>> = RwLock::new(None);

pub fn mirror(settings: Option<Arc<Settings>>) {
    if let Ok(mut slot) = MIRROR.write() {
        *slot = settings;
    }
}

fn current() -> Option<Arc<Settings>> {
    MIRROR.read().ok().and_then(|slot| slot.clone())
}

pub struct DesktopSettings;

fn label<T: Serialize>(value: &T) -> String {
    let raw = serde_json::to_value(value)
        .ok()
        .and_then(|v| v.as_str().map(str::to_owned))
        .unwrap_or_default();
    let mut out = String::new();
    for (i, word) in raw.split(['_', '-']).filter(|w| !w.is_empty()).enumerate() {
        if i > 0 {
            out.push(' ');
        }
        let mut chars = word.chars();
        if let Some(first) = chars.next() {
            if i == 0 {
                out.extend(first.to_uppercase());
            } else {
                out.push(first);
            }
            out.push_str(chars.as_str());
        }
    }
    out
}

fn agent_name(id: &str) -> String {
    match id {
        "claude-code" => "Claude Code".into(),
        "codex" => "Codex".into(),
        "agy" => "Antigravity".into(),
        "api-deepseek" => "DeepSeek".into(),
        other => other.to_owned(),
    }
}

fn on_off(on: bool) -> &'static str {
    if on {
        "On"
    } else {
        "Off"
    }
}

fn row(title: &str, value: impl Into<String>) -> Value {
    json!({ "title": title, "value": value.into() })
}

fn toggle(title: &str, on: bool) -> Value {
    json!({ "title": title, "kind": "toggle", "on": on })
}

fn section(title: &str, rows: Vec<Value>) -> Value {
    json!({ "title": title, "rows": rows })
}

fn host() -> String {
    std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .unwrap_or_default()
}

fn device_count() -> usize {
    super::snapshot().devices.len()
}

fn enabled_agents(s: &Settings) -> usize {
    s.agents.values().filter(|a| a.enabled).count()
}

impl SettingsView for DesktopSettings {
    fn overview(&self) -> Value {
        let Some(s) = current() else {
            return json!({ "host": host(), "items": {} });
        };
        json!({
            "host": host(),
            "items": {
                "general": label(&s.general.language),
                "appearance": label(&s.appearance.theme),
                "notifications": on_off(s.notifications.task_done),
                "shortcuts": s.shortcuts.len().to_string(),
                "agents": format!("{} enabled", enabled_agents(&s)),
                "orchestra": agent_name(&s.orchestra.default_agent),
                "memory": on_off(s.memory.workspace_memory_enabled),
                "remote": format!("{} devices", device_count()),
            }
        })
    }

    fn page(&self, page: &str) -> Value {
        let Some(s) = current() else {
            return json!({ "title": page, "subtitle": "", "sections": [] });
        };
        let (title, subtitle, sections) = match page {
            "general" => (
                "General",
                "Language, composer and conversation display",
                vec![
                    section(
                        "Basics",
                        vec![
                            row("Language", label(&s.general.language)),
                            row("Send with", label(&s.general.composer_send)),
                            row("Follow-up", label(&s.general.follow_up)),
                            toggle("Launch at startup", s.general.launch_at_startup),
                        ],
                    ),
                    section(
                        "Conversation",
                        vec![
                            toggle("Show reasoning", s.general.show_reasoning),
                            toggle("Show todos", s.general.show_todos),
                            toggle("Group exploration", s.general.group_exploration),
                            toggle("Group terminal", s.general.group_terminal),
                            toggle("Group file changes", s.general.group_file_changes),
                            toggle("Auto compact", s.general.auto_compact),
                            toggle("Auto archive", s.general.auto_archive),
                        ],
                    ),
                ],
            ),
            "appearance" => (
                "Appearance",
                "Theme, fonts and density",
                vec![
                    section(
                        "Theme",
                        vec![
                            row("Theme", label(&s.appearance.theme)),
                            row("Accent", s.appearance.accent_color.clone()),
                            row("Density", s.appearance.density.clone()),
                        ],
                    ),
                    section(
                        "Text",
                        vec![
                            row("Interface font", s.appearance.ui_font.clone()),
                            row("Font size", format!("{} px", s.appearance.ui_font_size)),
                            row("Zoom", format!("{}%", s.appearance.zoom_percent)),
                            row("Code font size", format!("{} px", s.appearance.code_font_size)),
                            toggle("Line numbers", s.appearance.line_numbers),
                            toggle("Wrap lines", s.appearance.wrap_lines),
                            toggle("Reduce motion", s.appearance.reduce_motion),
                            toggle("Show token usage", s.appearance.show_token_usage),
                        ],
                    ),
                ],
            ),
            "notifications" | "inbox" => (
                "Notifications",
                "When Harness tells you something finished",
                vec![section(
                    "Alerts",
                    vec![
                        toggle("Task done", s.notifications.task_done),
                        toggle("Build done", s.notifications.build_done),
                        toggle("PR review", s.notifications.pr_review),
                        toggle("Sound", s.notifications.sound),
                        toggle("Keep awake", s.notifications.keep_awake),
                        toggle("Close to tray", s.notifications.close_to_tray),
                    ],
                )],
            ),
            "shortcuts" => (
                "Shortcuts",
                "Keyboard bindings on the desktop",
                vec![section(
                    "Bindings",
                    s.shortcuts
                        .iter()
                        .map(|(id, keys)| {
                            json!({ "title": label(&id.as_str()), "value": keys.join(" / "), "kind": "code" })
                        })
                        .collect(),
                )],
            ),
            "agents" => (
                "Agents",
                "Coding agents Harness can run",
                vec![section(
                    "Installed agents",
                    s.agents
                        .iter()
                        .map(|(id, prefs)| {
                            json!({
                                "title": agent_name(id),
                                "detail": prefs.default_model.clone().unwrap_or_default(),
                                "badge": if prefs.enabled { "Enabled" } else { "Disabled" },
                                "tone": if prefs.enabled { "success" } else { "muted" },
                            })
                        })
                        .collect(),
                )],
            ),
            "orchestra" => (
                "Orchestra",
                "Task board and parallel agents",
                vec![section(
                    "Defaults",
                    vec![
                        row("Default agent", agent_name(&s.orchestra.default_agent)),
                        row("Default mode", s.orchestra.default_mode.clone()),
                        row("Max concurrent", s.orchestra.max_concurrent.to_string()),
                        toggle("Auto claim", s.orchestra.auto_claim),
                        toggle("Snap to grid", s.orchestra.snap_to_grid),
                    ],
                )],
            ),
            "memory" => (
                "Memory",
                "What agents remember between sessions",
                vec![section(
                    "Memory",
                    vec![toggle("Workspace memory", s.memory.workspace_memory_enabled)],
                )],
            ),
            "remote" => (
                "Remote",
                "Phone access to this desktop",
                vec![section(
                    "Server",
                    vec![
                        json!({ "title": "Server", "badge": "Running", "tone": "success" }),
                        row("Port", s.remote.port.to_string()),
                        row("Paired devices", device_count().to_string()),
                    ],
                )],
            ),
            _ => (page, "", Vec::new()),
        };
        json!({ "title": title, "subtitle": subtitle, "sections": sections })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys(value: &Value, out: &mut Vec<String>) {
        match value {
            Value::Object(map) => {
                for (k, v) in map {
                    out.push(k.to_lowercase());
                    keys(v, out);
                }
            }
            Value::Array(items) => items.iter().for_each(|v| keys(v, out)),
            _ => {}
        }
    }

    #[test]
    fn pages_carry_no_secret_fields() {
        mirror(Some(Arc::new(Settings::default())));
        for page in [
            "general",
            "appearance",
            "notifications",
            "shortcuts",
            "agents",
            "orchestra",
            "memory",
            "remote",
            "mcp",
        ] {
            let mut found = Vec::new();
            keys(&DesktopSettings.page(page), &mut found);
            for k in found {
                assert!(
                    !["key", "token", "secret", "subdomain", "proxy"]
                        .iter()
                        .any(|bad| k.contains(bad)),
                    "{page}: {k}"
                );
            }
        }
        assert_eq!(label(&"ui_font"), "Ui font");
    }
}
