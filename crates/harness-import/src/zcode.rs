//! `ZCode` sources: legacy JSON task snapshots and the CLI session store (read-only).

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use harness_core_api::ImportSource;
use rusqlite::{Connection, OpenFlags};
use serde_json::Value;

use crate::{content_text, int, role_of, title_of, turns_of, walk, ImportedSession, Role};

pub(crate) async fn scan(root: &Path) -> Vec<ImportedSession> {
    let mut by_id: HashMap<String, ImportedSession> = HashMap::new();
    let sessions = root.join("v2").join("sessions");
    let files = walk(&sessions, &|p: &Path| {
        let name = p
            .file_name()
            .map(|n| n.to_string_lossy().to_ascii_lowercase())
            .unwrap_or_default();
        let json = p
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("json"));
        json && !name.ends_with(".deleted.json")
    })
    .await;
    for file in files {
        if let Some(session) = legacy(&file).await {
            insert(&mut by_id, session);
        }
    }
    let db = root.join("cli").join("db").join("db.sqlite");
    if tokio::fs::metadata(&db).await.is_ok_and(|m| m.is_file()) {
        let read = tokio::task::spawn_blocking(move || cli_store(&db)).await;
        match read {
            Ok(Ok(list)) => {
                for session in list {
                    insert(&mut by_id, session);
                }
            }
            Ok(Err(error)) => tracing::warn!(%error, "ZCode CLI store unreadable"),
            Err(error) => tracing::warn!(%error, "ZCode CLI store task failed"),
        }
    }
    by_id.into_values().collect()
}

/// Same id in both sources: the more recently updated copy wins.
fn insert(by_id: &mut HashMap<String, ImportedSession>, session: ImportedSession) {
    match by_id.get(&session.external_id) {
        Some(existing) if existing.updated_at >= session.updated_at => {}
        _ => {
            by_id.insert(session.external_id.clone(), session);
        }
    }
}

async fn legacy(file: &PathBuf) -> Option<ImportedSession> {
    let bytes = tokio::fs::read(file).await.ok()?;
    let json: Value = serde_json::from_slice(&bytes).ok()?;
    let meta = json.get("meta").unwrap_or(&Value::Null);
    let stem = file.file_stem()?.to_string_lossy().into_owned();
    let id = meta
        .get("taskId")
        .and_then(Value::as_str)
        .map_or(stem, str::to_owned);
    let messages = json.get("messages").and_then(Value::as_array)?;
    let parsed = messages
        .iter()
        .filter_map(|m| {
            let role = role_of(m.get("role").and_then(Value::as_str))?;
            let tools = m
                .get("tools")
                .and_then(Value::as_array)
                .map(|list| {
                    list.iter()
                        .filter_map(|t| {
                            t.get("title")
                                .or_else(|| t.get("toolName"))
                                .and_then(Value::as_str)
                                .map(str::to_owned)
                        })
                        .collect()
                })
                .unwrap_or_default();
            let at = int(m.get("createdAt").or_else(|| m.get("timestamp")));
            Some((role, content_text(m.get("content")), tools, at))
        })
        .collect();
    let turns = turns_of(parsed);
    if turns.is_empty() {
        return None;
    }
    let title = meta
        .get("title")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map_or_else(|| title_of(&turns), str::to_owned);
    Some(ImportedSession {
        source: ImportSource::Zcode,
        external_id: id,
        title,
        workspace_path: meta
            .get("workspacePath")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        created_at: int(meta.get("createdAt")),
        updated_at: int(meta.get("updatedAt")),
        turns,
    })
}

fn has_column(c: &Connection, table: &str, column: &str) -> bool {
    c.prepare(&format!("SELECT name FROM pragma_table_info('{table}')"))
        .and_then(|mut s| {
            let names = s
                .query_map([], |r| r.get::<_, String>(0))?
                .filter_map(Result::ok)
                .collect::<Vec<_>>();
            Ok(names.iter().any(|n| n == column))
        })
        .unwrap_or(false)
}

/// Visible text (synthetic parts skipped) and tool titles of one message's parts.
fn text_and_tools(parts: &[String]) -> (String, Vec<String>) {
    let mut text = Vec::new();
    let mut tools = Vec::new();
    for part in parts {
        let Ok(part) = serde_json::from_str::<Value>(part) else {
            continue;
        };
        match part.get("type").and_then(Value::as_str) {
            Some("text")
                if !part
                    .get("synthetic")
                    .and_then(Value::as_bool)
                    .unwrap_or(false) =>
            {
                if let Some(t) = part.get("text").and_then(Value::as_str) {
                    text.push(t.to_owned());
                }
            }
            Some("tool") => {
                let name = part
                    .pointer("/state/title")
                    .and_then(Value::as_str)
                    .filter(|t| !t.is_empty())
                    .or_else(|| part.get("tool").and_then(Value::as_str));
                if let Some(name) = name {
                    tools.push(name.to_owned());
                }
            }
            _ => {}
        }
    }
    (text.join("\n"), tools)
}

/// The CLI store, opened read-only; archived, deleted and child (sub-agent) sessions are skipped.
fn cli_store(db: &Path) -> Result<Vec<ImportedSession>, rusqlite::Error> {
    let c = Connection::open_with_flags(
        db,
        OpenFlags::SQLITE_OPEN_READ_ONLY
            | OpenFlags::SQLITE_OPEN_NO_MUTEX
            | OpenFlags::SQLITE_OPEN_URI,
    )?;
    let mut filter = String::from("time_archived IS NULL AND parent_id IS NULL");
    if has_column(&c, "session", "time_deleted") {
        filter.push_str(" AND time_deleted IS NULL");
    }
    let mut sessions = c.prepare(&format!(
        "SELECT id, title, directory, time_created, time_updated FROM session WHERE {filter}"
    ))?;
    let rows = sessions
        .query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, i64>(3)?,
                r.get::<_, i64>(4)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    let mut messages = c.prepare(
        "SELECT id, time_created, data FROM message WHERE session_id=?1
         ORDER BY COALESCE(sequence, 0), time_created, id",
    )?;
    let mut parts = c.prepare(
        "SELECT data FROM part WHERE message_id=?1 ORDER BY COALESCE(sequence, 0), time_created, id",
    )?;
    let mut out = Vec::new();
    for (id, title, directory, created, updated) in rows {
        let list = messages
            .query_map([&id], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, String>(2)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        let mut parsed: Vec<(Role, String, Vec<String>, i64)> = Vec::new();
        for (message_id, at, data) in list {
            let Ok(data) = serde_json::from_str::<Value>(&data) else {
                continue;
            };
            let Some(role) = role_of(data.get("role").and_then(Value::as_str)) else {
                continue;
            };
            let part_rows = parts
                .query_map([&message_id], |r| r.get::<_, String>(0))?
                .collect::<Result<Vec<_>, _>>()?;
            let (text, tools) = text_and_tools(&part_rows);
            parsed.push((role, text, tools, at));
        }
        let turns = turns_of(parsed);
        if turns.is_empty() {
            continue;
        }
        let title = if title.trim().is_empty() {
            title_of(&turns)
        } else {
            title.trim().to_owned()
        };
        out.push(ImportedSession {
            source: ImportSource::Zcode,
            external_id: id,
            title,
            workspace_path: directory,
            created_at: created,
            updated_at: updated,
            turns,
        });
    }
    Ok(out)
}
