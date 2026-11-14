//! Claude Code transcripts: `<claude>/projects/**/*.jsonl` only (never settings or credentials files).

use std::path::Path;

use harness_core_api::ImportSource;
use serde_json::Value;

use crate::{int, role_of, title_of, turns_of, walk, ImportedSession, Role};

/// Tags whose whole element is local noise, not conversation text.
const STRIP: &[&str] = &[
    "command-",
    "local-command-",
    "ide_opened_file",
    "system-reminder",
];

pub(crate) async fn scan(projects: &Path) -> Vec<ImportedSession> {
    let files = walk(projects, &|p: &Path| {
        p.extension().is_some_and(|e| e == "jsonl")
    })
    .await;
    let mut out = Vec::new();
    for file in files {
        let Some(stem) = file.file_stem().map(|s| s.to_string_lossy().into_owned()) else {
            continue;
        };
        let Ok(text) = tokio::fs::read_to_string(&file).await else {
            continue;
        };
        if let Some(session) = parse(&stem, &text) {
            out.push(session);
        }
    }
    out
}

pub(crate) fn parse(id: &str, text: &str) -> Option<ImportedSession> {
    let mut messages: Vec<(Role, String, Vec<String>, i64)> = Vec::new();
    let mut cwd = String::new();
    let (mut first, mut last) = (i64::MAX, 0_i64);
    for line in text.lines() {
        let Ok(entry) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let flag = |k: &str| entry.get(k).and_then(Value::as_bool).unwrap_or(false);
        if flag("isMeta") || flag("isSidechain") || flag("isApiErrorMessage") {
            continue;
        }
        let Some(role) = role_of(entry.get("type").and_then(Value::as_str)) else {
            continue;
        };
        if cwd.is_empty() {
            if let Some(dir) = entry.get("cwd").and_then(Value::as_str) {
                dir.clone_into(&mut cwd);
            }
        }
        let at = int(entry.get("timestamp"));
        if at > 0 {
            first = first.min(at);
            last = last.max(at);
        }
        let content = entry.pointer("/message/content");
        let (text, tools) = blocks(content);
        let text = strip_tags(&text);
        if role == Role::User && text.trim().is_empty() {
            // 只含 tool_result 的 user 条目不是新的一轮。
            continue;
        }
        messages.push((role, text, tools, at));
    }
    let turns = turns_of(messages);
    if turns.is_empty() {
        return None;
    }
    Some(ImportedSession {
        source: ImportSource::ClaudeCode,
        external_id: id.to_owned(),
        title: title_of(&turns),
        workspace_path: cwd,
        created_at: if first == i64::MAX { 0 } else { first },
        updated_at: last,
        turns,
    })
}

/// Text (string or `text` blocks) and `tool_use` names; `tool_result` blocks are skipped.
fn blocks(content: Option<&Value>) -> (String, Vec<String>) {
    match content {
        Some(Value::String(text)) => (text.clone(), Vec::new()),
        Some(Value::Array(list)) => {
            let mut text = Vec::new();
            let mut tools = Vec::new();
            for block in list {
                match block.get("type").and_then(Value::as_str) {
                    Some("text") => {
                        if let Some(t) = block.get("text").and_then(Value::as_str) {
                            text.push(t.to_owned());
                        }
                    }
                    Some("tool_use") => {
                        if let Some(name) = block.get("name").and_then(Value::as_str) {
                            tools.push(name.to_owned());
                        }
                    }
                    _ => {}
                }
            }
            (text.join("\n"), tools)
        }
        _ => (String::new(), Vec::new()),
    }
}

/// Removes `<command-*>…</command-*>`, `<local-command-*>…`, `<ide_opened_file>…` elements.
pub(crate) fn strip_tags(text: &str) -> String {
    crate::strip_elements(text, STRIP)
}
