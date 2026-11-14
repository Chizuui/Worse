//! Read-only importer of `ZCode` and Claude Code session history (decision 14A, phase-9-parity.md §9.2.1).
//!
//! Pure reader: it never writes, renames or locks source files (the `ZCode` CLI store is opened `mode=ro`). Roots are
//! injected (gates use fixtures); only the documented subtrees are read:
//! - `<zcode>/v2/sessions/*/<taskId>.json` (`*.deleted.json` skipped),
//! - `<zcode>/cli/db/db.sqlite` (tables `session`, `message`, `part`),
//! - `<claude>/projects/**/*.jsonl`.

use std::path::{Path, PathBuf};

use harness_core_api::ImportSource;
use serde_json::Value;

mod claude;
mod codex;
mod zcode;

/// Where the importable histories live.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ImportRoots {
    /// `ZCode` data dir (`<ZCODE_DATA_BASE_DIR>/.zcode` or `~/.zcode`).
    pub zcode: Option<PathBuf>,
    /// Claude Code config dir (`CLAUDE_CONFIG_DIR` or `~/.claude`); only `projects/` is read.
    pub claude: Option<PathBuf>,
    pub codex: Option<PathBuf>,
}

impl ImportRoots {
    /// Default roots from the environment (paths only; nothing is read here).
    #[must_use]
    pub fn from_env() -> Self {
        let home = user_home();
        let zcode = env_path("ZCODE_DATA_BASE_DIR")
            .map(|base| base.join(".zcode"))
            .or_else(|| home.as_ref().map(|h| h.join(".zcode")));
        let claude =
            env_path("CLAUDE_CONFIG_DIR").or_else(|| home.as_ref().map(|h| h.join(".claude")));
        let codex = env_path("CODEX_HOME").or_else(|| home.map(|h| h.join(".codex")));
        Self {
            zcode,
            claude,
            codex,
        }
    }
}

fn env_path(key: &str) -> Option<PathBuf> {
    std::env::var_os(key)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

fn user_home() -> Option<PathBuf> {
    let keys: &[&str] = if cfg!(windows) {
        &["USERPROFILE", "HOME"]
    } else {
        &["HOME"]
    };
    keys.iter().find_map(|k| env_path(k))
}

/// One finished turn of an imported session.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ImportedTurn {
    pub user_text: String,
    /// Visible assistant text.
    pub assistant_text: String,
    /// Tool call titles in order.
    pub tools: Vec<String>,
    /// Unix ms (0 when unknown).
    pub created_at: i64,
}

/// One parsed session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportedSession {
    pub source: ImportSource,
    pub external_id: String,
    pub title: String,
    pub workspace_path: String,
    pub created_at: i64,
    pub updated_at: i64,
    pub turns: Vec<ImportedTurn>,
}

impl ImportedSession {
    /// `imported_from` key: `<source>:<external_id>`.
    #[must_use]
    pub fn key(&self) -> String {
        key(self.source, &self.external_id)
    }
}

/// `imported_from` key of a source id.
#[must_use]
pub fn key(source: ImportSource, external_id: &str) -> String {
    format!("{}:{external_id}", source.as_str())
}

/// Parses every session of `sources` (empty = all). Unreadable files are skipped (logged by the caller's count).
pub async fn scan(roots: &ImportRoots, sources: &[ImportSource]) -> Vec<ImportedSession> {
    let wants = |s: ImportSource| sources.is_empty() || sources.contains(&s);
    let mut out = Vec::new();
    if wants(ImportSource::Zcode) {
        if let Some(root) = &roots.zcode {
            out.extend(zcode::scan(root).await);
        }
    }
    if wants(ImportSource::ClaudeCode) {
        if let Some(root) = &roots.claude {
            out.extend(claude::scan(&root.join("projects")).await);
        }
    }
    if wants(ImportSource::Codex) {
        if let Some(root) = &roots.codex {
            out.extend(codex::scan(root).await);
        }
    }
    out.sort_by_key(|s| std::cmp::Reverse(s.updated_at));
    out
}

// ---- shared parsing helpers -------------------------------------------------------------------------

/// Builds turns from `(role, text, tools, at)` messages: a user message opens a turn.
pub(crate) fn turns_of(messages: Vec<(Role, String, Vec<String>, i64)>) -> Vec<ImportedTurn> {
    let mut turns: Vec<ImportedTurn> = Vec::new();
    for (role, text, tools, at) in messages {
        match role {
            Role::User => {
                if text.trim().is_empty() {
                    continue;
                }
                turns.push(ImportedTurn {
                    user_text: text,
                    created_at: at,
                    ..ImportedTurn::default()
                });
            }
            Role::Assistant => {
                if turns.is_empty() {
                    turns.push(ImportedTurn {
                        created_at: at,
                        ..ImportedTurn::default()
                    });
                }
                if let Some(turn) = turns.last_mut() {
                    if !text.trim().is_empty() {
                        if !turn.assistant_text.is_empty() {
                            turn.assistant_text.push_str("\n\n");
                        }
                        turn.assistant_text.push_str(text.trim());
                    }
                    turn.tools.extend(tools);
                }
            }
        }
    }
    turns
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Role {
    User,
    Assistant,
}

pub(crate) fn role_of(value: Option<&str>) -> Option<Role> {
    match value {
        Some("user") => Some(Role::User),
        Some("assistant") => Some(Role::Assistant),
        _ => None,
    }
}

/// First user text, 80 chars.
pub(crate) fn title_of(turns: &[ImportedTurn]) -> String {
    let first = turns
        .iter()
        .map(|t| t.user_text.trim())
        .find(|t| !t.is_empty())
        .unwrap_or("Imported session");
    let line = first.lines().next().unwrap_or(first);
    line.chars().take(80).collect()
}

/// Text of a message `content`: a string, or the `text` blocks of an array.
pub(crate) fn content_text(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(blocks)) => blocks
            .iter()
            .filter(|b| {
                b.get("type")
                    .and_then(Value::as_str)
                    .map_or(true, |t| t == "text")
            })
            .filter_map(|b| b.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

pub(crate) fn strip_elements(text: &str, prefixes: &[&str]) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(open) = rest.find('<') {
        let after = &rest[open + 1..];
        let name_len = after
            .find(|c: char| !(c.is_ascii_alphanumeric() || c == '-' || c == '_'))
            .unwrap_or(after.len());
        let name = &after[..name_len];
        if !name.is_empty() && prefixes.iter().any(|p| name.starts_with(p)) {
            let close = format!("</{name}>");
            if let Some(end) = rest[open..].find(&close) {
                out.push_str(&rest[..open]);
                rest = &rest[open + end + close.len()..];
                continue;
            }
        }
        out.push_str(&rest[..=open]);
        rest = &rest[open + 1..];
    }
    out.push_str(rest);
    out.trim().to_owned()
}

pub(crate) fn int(value: Option<&Value>) -> i64 {
    match value {
        Some(Value::Number(n)) => n.as_i64().unwrap_or(0),
        Some(Value::String(s)) => s.parse().ok().or_else(|| iso_ms(s)).unwrap_or(0),
        _ => 0,
    }
}

pub(crate) fn iso_ms(text: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(text)
        .ok()
        .map(|t| t.timestamp_millis())
}

/// Files under `dir` (recursive) whose name satisfies `keep`.
pub(crate) async fn walk(dir: &Path, keep: &(dyn Fn(&Path) -> bool + Sync)) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut pending = vec![dir.to_path_buf()];
    while let Some(next) = pending.pop() {
        let Ok(mut entries) = tokio::fs::read_dir(&next).await else {
            continue;
        };
        while let Ok(Some(entry)) = entries.next_entry().await {
            let Ok(kind) = entry.file_type().await else {
                continue;
            };
            let path = entry.path();
            if kind.is_dir() {
                pending.push(path);
            } else if kind.is_file() && keep(&path) {
                out.push(path);
            }
        }
    }
    out.sort();
    out
}

#[cfg(test)]
mod tests;
