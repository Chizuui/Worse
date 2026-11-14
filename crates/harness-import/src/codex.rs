use std::path::Path;

use harness_core_api::ImportSource;
use serde_json::Value;

use crate::{iso_ms, strip_elements, title_of, turns_of, walk, ImportedSession, Role};

const STRIP: &[&str] = &[
    "environment_context",
    "user_instructions",
    "turn_aborted",
    "harness-memory",
    "system-reminder",
];

const TOOL_TITLE: usize = 120;

type Message = (Role, String, Vec<String>, i64);

pub(crate) async fn scan(root: &Path) -> Vec<ImportedSession> {
    let is_jsonl = |p: &Path| p.extension().is_some_and(|e| e == "jsonl");
    let mut files = walk(&root.join("sessions"), &is_jsonl).await;
    files.extend(walk(&root.join("archived_sessions"), &is_jsonl).await);
    let mut out: Vec<ImportedSession> = Vec::new();
    for file in files {
        let Some(stem) = file.file_stem().map(|s| s.to_string_lossy().into_owned()) else {
            continue;
        };
        let Ok(text) = tokio::fs::read_to_string(&file).await else {
            continue;
        };
        if let Some(session) = parse(&stem, &text) {
            if !out.iter().any(|s| s.external_id == session.external_id) {
                out.push(session);
            }
        }
    }
    out
}

pub(crate) fn parse(stem: &str, text: &str) -> Option<ImportedSession> {
    let records: Vec<Value> = text
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .collect();
    let mut id = String::new();
    let mut cwd = String::new();
    for record in &records {
        if record.get("type").and_then(Value::as_str) != Some("session_meta") {
            continue;
        }
        let meta = record.get("payload").unwrap_or(&Value::Null);
        if meta.get("originator").and_then(Value::as_str) == Some("harness") {
            return None;
        }
        if meta
            .get("source")
            .is_some_and(|s| s.get("subagent").is_some())
        {
            return None;
        }
        if let Some(value) = meta.get("id").and_then(Value::as_str) {
            value.clone_into(&mut id);
        }
        if let Some(value) = meta.get("cwd").and_then(Value::as_str) {
            value.clone_into(&mut cwd);
        }
        break;
    }
    if id.is_empty() {
        stem.clone_into(&mut id);
    }
    let (mut first, mut last) = (i64::MAX, 0_i64);
    for record in &records {
        let at = record
            .get("timestamp")
            .and_then(Value::as_str)
            .and_then(iso_ms)
            .unwrap_or(0);
        if at > 0 {
            first = first.min(at);
            last = last.max(at);
        }
    }
    let items = from_items(&records);
    let messages = if items.iter().any(|m| m.0 == Role::User) {
        items
    } else {
        from_responses(&records)
    };
    let turns = turns_of(messages);
    if turns.is_empty() {
        return None;
    }
    Some(ImportedSession {
        source: ImportSource::Codex,
        external_id: id,
        title: title_of(&turns),
        workspace_path: cwd,
        created_at: if first == i64::MAX { 0 } else { first },
        updated_at: last,
        turns,
    })
}

fn at_of(record: &Value) -> i64 {
    record
        .get("timestamp")
        .and_then(Value::as_str)
        .and_then(iso_ms)
        .unwrap_or(0)
}

fn texts(content: Option<&Value>) -> String {
    content
        .and_then(Value::as_array)
        .map(|blocks| {
            blocks
                .iter()
                .filter_map(|b| b.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default()
}

fn user_text(raw: &str) -> String {
    let text = strip_elements(raw, STRIP);
    if text.starts_with("# AGENTS.md instructions") {
        return String::new();
    }
    text
}

fn short(text: &str) -> String {
    let line = text.lines().next().unwrap_or_default().trim();
    if line.chars().count() > TOOL_TITLE {
        let mut out: String = line.chars().take(TOOL_TITLE).collect();
        out.push('…');
        out
    } else {
        line.to_owned()
    }
}

fn command_title(command: Option<&Value>) -> String {
    match command {
        Some(Value::String(text)) => short(text),
        Some(Value::Array(parts)) => short(
            &parts
                .iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join(" "),
        ),
        _ => "Run command".to_owned(),
    }
}

fn push_tool(messages: &mut Vec<Message>, title: String, at: i64) {
    if title.is_empty() {
        return;
    }
    match messages.last_mut() {
        Some(last) if last.0 == Role::Assistant => last.2.push(title),
        _ => messages.push((Role::Assistant, String::new(), vec![title], at)),
    }
}

fn from_items(records: &[Value]) -> Vec<Message> {
    let mut out: Vec<Message> = Vec::new();
    for record in records {
        if record.get("type").and_then(Value::as_str) != Some("event_msg") {
            continue;
        }
        let payload = record.get("payload").unwrap_or(&Value::Null);
        if payload.get("type").and_then(Value::as_str) != Some("item_completed") {
            continue;
        }
        let item = payload.get("item").unwrap_or(&Value::Null);
        let at = at_of(record);
        match item.get("type").and_then(Value::as_str) {
            Some("UserMessage") => {
                let text = user_text(&texts(item.get("content")));
                if !text.trim().is_empty() {
                    out.push((Role::User, text, Vec::new(), at));
                }
            }
            Some("AgentMessage") => {
                let text = texts(item.get("content"));
                out.push((Role::Assistant, text, Vec::new(), at));
            }
            Some("CommandExecution") => push_tool(&mut out, command_title(item.get("command")), at),
            Some("McpToolCall") => {
                let server = item
                    .get("server")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let tool = item.get("tool").and_then(Value::as_str).unwrap_or_default();
                let title = if server.is_empty() {
                    tool.to_owned()
                } else {
                    format!("{server}.{tool}")
                };
                push_tool(&mut out, title, at);
            }
            Some("ImageView") => push_tool(&mut out, "View image".to_owned(), at),
            _ => {}
        }
    }
    out
}

fn from_responses(records: &[Value]) -> Vec<Message> {
    let mut out: Vec<Message> = Vec::new();
    for record in records {
        if record.get("type").and_then(Value::as_str) != Some("response_item") {
            continue;
        }
        let payload = record.get("payload").unwrap_or(&Value::Null);
        let at = at_of(record);
        match payload.get("type").and_then(Value::as_str) {
            Some("message") => match payload.get("role").and_then(Value::as_str) {
                Some("user") => {
                    let text = user_text(&texts(payload.get("content")));
                    if !text.trim().is_empty() {
                        out.push((Role::User, text, Vec::new(), at));
                    }
                }
                Some("assistant") => {
                    out.push((
                        Role::Assistant,
                        texts(payload.get("content")),
                        Vec::new(),
                        at,
                    ));
                }
                _ => {}
            },
            Some("function_call" | "custom_tool_call" | "local_shell_call") => {
                let name = payload
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("Run command")
                    .to_owned();
                push_tool(&mut out, name, at);
            }
            _ => {}
        }
    }
    out
}
