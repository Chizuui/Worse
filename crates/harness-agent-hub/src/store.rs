//! SQLite session store (spec §5). Single writer: every call goes through [`Store::run`], which serializes
//! on one connection inside `spawn_blocking`.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

use harness_core_api::{
    AgentId, ErrorClass, PermissionDecision, QuotaState, SessionId, SessionSummary, ToolCallView,
    TurnAttempt, TurnId, TurnSegment, TurnStatus, TurnView,
};
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::Value;

use crate::error::HubError;

const SCHEMA_VERSION: i64 = 1;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS sessions(
  id TEXT PRIMARY KEY, title TEXT NOT NULL, workspace_path TEXT NOT NULL, workspace_key TEXT NOT NULL,
  active_agent TEXT NOT NULL, created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL);
CREATE INDEX IF NOT EXISTS sessions_ws ON sessions(workspace_key, updated_at);
CREATE TABLE IF NOT EXISTS turns(
  id TEXT PRIMARY KEY, session_id TEXT NOT NULL, seq INTEGER NOT NULL, client_message_id TEXT,
  user_text TEXT NOT NULL, agent_id TEXT, assistant_text TEXT NOT NULL DEFAULT '', status TEXT NOT NULL,
  stop_reason TEXT, error TEXT, created_at INTEGER NOT NULL, finished_at INTEGER,
  UNIQUE(session_id, seq));
CREATE TABLE IF NOT EXISTS turn_attempts(
  turn_id TEXT NOT NULL, attempt INTEGER NOT NULL, agent_id TEXT NOT NULL, status TEXT NOT NULL,
  partial_text TEXT NOT NULL DEFAULT '', error_class TEXT, error TEXT, resets_at INTEGER,
  PRIMARY KEY(turn_id, attempt));
-- 修复说明：原 UNIQUE(turn_id, agent_id) 会拒绝暂停后恢复轮次中同一 agent 的新尝试，写入失败只记 warn，
-- 导致该尝试未落盘。幂等规则（同一轮 failover 内每个 agent 至多一次）由会话 actor 这一唯一所有者执行（spec §6.4）。
CREATE TABLE IF NOT EXISTS turn_events(
  turn_id TEXT NOT NULL, idx INTEGER NOT NULL, kind TEXT NOT NULL, payload TEXT NOT NULL,
  PRIMARY KEY(turn_id, idx));
CREATE TABLE IF NOT EXISTS agent_handles(
  session_id TEXT NOT NULL, agent_id TEXT NOT NULL, native_session_id TEXT NOT NULL, last_seq INTEGER NOT NULL,
  PRIMARY KEY(session_id, agent_id));
CREATE TABLE IF NOT EXISTS agent_quota(
  agent_id TEXT PRIMARY KEY, state TEXT NOT NULL, resets_at INTEGER, reason TEXT, updated_at INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS settings(key TEXT PRIMARY KEY, value TEXT NOT NULL);
-- Round 8.2 (spec §15.2): git tree snapshots around a turn. Additive, so user_version stays 1.
CREATE TABLE IF NOT EXISTS turn_snapshots(
  turn_id TEXT PRIMARY KEY, before_tree TEXT, after_tree TEXT, error TEXT);
-- Phase 9.1 (phase-9-parity.md §9.1.5): mode/model/config choices per (session, agent). Additive.
CREATE TABLE IF NOT EXISTS session_prefs(
  session_id TEXT NOT NULL, agent_id TEXT NOT NULL, kind TEXT NOT NULL, key TEXT NOT NULL, value TEXT NOT NULL,
  PRIMARY KEY(session_id, agent_id, kind, key));
-- Phase 9.1 (§9.1.5a): removed projects (undo list). Additive; sessions.archived_at is added by
-- ensure_archive_column.
CREATE TABLE IF NOT EXISTS projects(
  workspace_key TEXT PRIMARY KEY, workspace_path TEXT NOT NULL, removed_at INTEGER);
";

/// Persisted session row.
#[derive(Debug, Clone)]
pub struct SessionRow {
    pub id: SessionId,
    pub title: String,
    pub workspace_path: String,
    pub workspace_key: String,
    pub active_agent: AgentId,
    pub created_at: i64,
    pub updated_at: i64,
}

/// Persisted native sub-session handle (cache).
#[derive(Debug, Clone)]
pub struct HandleRow {
    pub agent: AgentId,
    pub native: String,
    pub last_seq: u32,
}

/// Cloneable store handle.
#[derive(Clone)]
pub struct Store {
    connection: Arc<Mutex<Connection>>,
}

impl Store {
    /// Opens (and migrates) the store; turns left queued/running by a crash become `interrupted`.
    ///
    /// # Errors
    ///
    /// SQLite or I/O errors.
    pub fn open(path: &Path) -> Result<Self, HubError> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let connection = Connection::open(path)?;
        connection.pragma_update(None, "journal_mode", "WAL")?;
        connection.pragma_update(None, "synchronous", "NORMAL")?;
        connection.execute_batch(SCHEMA)?;
        ensure_archive_column(&connection)?;
        crate::store_chat::migrate(&connection)?;
        crate::journal::migrate(&connection)?;
        let version: i64 = connection.pragma_query_value(None, "user_version", |r| r.get(0))?;
        if version == 0 {
            connection.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        } else if version > SCHEMA_VERSION {
            return Err(HubError::Config(format!(
                "store schema {version} is newer than this build ({SCHEMA_VERSION})"
            )));
        }
        connection.execute(
            "UPDATE turns SET status='interrupted' WHERE status IN ('queued','running','paused')",
            [],
        )?;
        connection.execute(
            "UPDATE turn_attempts SET status='interrupted' WHERE status='running'",
            [],
        )?;
        crate::journal::mark_orphans(&connection)?;
        Ok(Self {
            connection: Arc::new(Mutex::new(connection)),
        })
    }

    /// Runs `f` on the connection in a blocking task.
    ///
    /// # Errors
    ///
    /// Whatever `f` returns, or a poisoned/cancelled task.
    pub async fn run<T, F>(&self, f: F) -> Result<T, HubError>
    where
        T: Send + 'static,
        F: FnOnce(&Connection) -> Result<T, HubError> + Send + 'static,
    {
        let connection = Arc::clone(&self.connection);
        tokio::task::spawn_blocking(move || {
            let guard = connection
                .lock()
                .map_err(|_| HubError::Config("store lock poisoned".into()))?;
            f(&guard)
        })
        .await
        .map_err(|e| HubError::Config(format!("store task failed: {e}")))?
    }
}

// ---- sessions -------------------------------------------------------------------------------------

pub(crate) fn insert_session(c: &Connection, row: &SessionRow) -> Result<(), HubError> {
    c.execute(
        "INSERT INTO sessions(id,title,workspace_path,workspace_key,active_agent,created_at,updated_at)
         VALUES(?1,?2,?3,?4,?5,?6,?7)",
        params![
            row.id.as_str(),
            row.title,
            row.workspace_path,
            row.workspace_key,
            row.active_agent.as_str(),
            row.created_at,
            row.updated_at
        ],
    )?;
    Ok(())
}

pub(crate) fn update_session(c: &Connection, row: &SessionRow) -> Result<(), HubError> {
    c.execute(
        "UPDATE sessions SET title=?2, active_agent=?3, updated_at=?4 WHERE id=?1",
        params![
            row.id.as_str(),
            row.title,
            row.active_agent.as_str(),
            row.updated_at
        ],
    )?;
    Ok(())
}

pub(crate) fn load_session(c: &Connection, id: &SessionId) -> Result<Option<SessionRow>, HubError> {
    Ok(c.query_row(
        "SELECT id,title,workspace_path,workspace_key,active_agent,created_at,updated_at FROM sessions WHERE id=?1",
        params![id.as_str()],
        session_from_row,
    )
    .optional()?)
}

fn session_from_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<SessionRow> {
    Ok(SessionRow {
        id: SessionId::new(r.get::<_, String>(0)?),
        title: r.get(1)?,
        workspace_path: r.get(2)?,
        workspace_key: r.get(3)?,
        active_agent: AgentId::new(r.get::<_, String>(4)?),
        created_at: r.get(5)?,
        updated_at: r.get(6)?,
    })
}

pub(crate) fn turn_count(c: &Connection, id: &SessionId) -> Result<u32, HubError> {
    let n: i64 = c.query_row(
        "SELECT COUNT(*) FROM turns WHERE session_id=?1",
        params![id.as_str()],
        |r| r.get(0),
    )?;
    Ok(u32::try_from(n).unwrap_or(u32::MAX))
}

pub(crate) fn summary(c: &Connection, row: &SessionRow) -> Result<SessionSummary, HubError> {
    let mut summary = SessionSummary {
        id: row.id.clone(),
        title: row.title.clone(),
        workspace_path: row.workspace_path.clone(),
        workspace_key: row.workspace_key.clone(),
        active_agent: row.active_agent.clone(),
        created_at: row.created_at,
        updated_at: row.updated_at,
        turn_count: turn_count(c, &row.id)?,
        agents_used: agents_used(c, &row.id)?,
        pinned: false,
        group_id: None,
        order: 0,
        unread: false,
        forked_from: None,
        imported_from: None,
        parent_id: None,
        worktree: None,
    };
    crate::store_chat::decorate(c, &mut summary)?;
    Ok(summary)
}

pub(crate) fn list_sessions(
    c: &Connection,
    workspace_key: Option<&str>,
    query: Option<&str>,
    limit: u32,
) -> Result<Vec<SessionSummary>, HubError> {
    let pattern = query.map(|q| format!("%{}%", q.to_lowercase()));
    // Archived sessions and (unless filtered by key) removed projects are not listed (§9.1.5a).
    let mut statement = c.prepare(&format!(
        "SELECT s.id,s.title,s.workspace_path,s.workspace_key,s.active_agent,s.created_at,s.updated_at
         FROM sessions s
         WHERE s.archived_at IS NULL AND (?1 IS NULL OR s.workspace_key=?1)
           AND NOT EXISTS(SELECT 1 FROM subagents g WHERE g.child_id=s.id)
           AND NOT EXISTS(SELECT 1 FROM inbox_routers r WHERE r.session_id=s.id)
           AND (?1 IS NOT NULL OR {NOT_REMOVED})
           AND (?2 IS NULL OR lower(s.title) LIKE ?2 OR EXISTS(
                SELECT 1 FROM turns t WHERE t.session_id=s.id
                AND (lower(t.user_text) LIKE ?2 OR lower(t.assistant_text) LIKE ?2)))
         ORDER BY s.updated_at DESC LIMIT ?3"
    ))?;
    let rows = statement
        .query_map(
            params![workspace_key, pattern, i64::from(limit)],
            session_from_row,
        )?
        .collect::<Result<Vec<_>, _>>()?;
    rows.iter().map(|row| summary(c, row)).collect()
}

// ---- turns ----------------------------------------------------------------------------------------

pub(crate) fn insert_turn(
    c: &Connection,
    session: &SessionId,
    turn: &TurnView,
    client_message_id: Option<&str>,
) -> Result<(), HubError> {
    c.execute(
        "INSERT INTO turns(id,session_id,seq,client_message_id,user_text,agent_id,assistant_text,status,created_at,
                           context_json,origin_json)
         VALUES(?1,?2,?3,?4,?5,?6,'',?7,?8,?9,?10)",
        params![
            turn.id.as_str(),
            session.as_str(),
            turn.seq,
            client_message_id,
            turn.user_text,
            turn.agent.as_ref().map(AgentId::as_str),
            status_str(turn.status),
            turn.created_at,
            crate::store_chat::json_list(&turn.context)?,
            crate::store_chat::json_opt(turn.origin.as_ref())?
        ],
    )?;
    Ok(())
}

pub(crate) fn update_turn(c: &Connection, turn: &TurnView) -> Result<(), HubError> {
    c.execute(
        "UPDATE turns SET agent_id=?2, assistant_text=?3, status=?4, stop_reason=?5, error=?6, finished_at=?7,
           usage_json=?8, context_usage_json=?9 WHERE id=?1",
        params![
            turn.id.as_str(),
            turn.agent.as_ref().map(AgentId::as_str),
            turn.assistant_text,
            status_str(turn.status),
            turn.stop_reason,
            turn.error,
            turn.finished_at,
            crate::store_chat::json_opt(turn.usage.as_ref())?,
            crate::store_chat::json_opt(turn.context_usage.as_ref())?
        ],
    )?;
    Ok(())
}

pub(crate) fn upsert_attempt(
    c: &Connection,
    turn: &TurnId,
    attempt: &TurnAttempt,
) -> Result<(), HubError> {
    let (class, resets_at) = match &attempt.error_class {
        Some(class) => (
            Some(serde_json::to_string(class)?),
            match class {
                ErrorClass::QuotaExhausted { resets_at } => *resets_at,
                _ => None,
            },
        ),
        None => (None, None),
    };
    c.execute(
        "INSERT INTO turn_attempts(turn_id,attempt,agent_id,status,partial_text,error_class,error,resets_at)
         VALUES(?1,?2,?3,?4,?5,?6,?7,?8)
         ON CONFLICT(turn_id,attempt) DO UPDATE SET status=?4, partial_text=?5, error_class=?6, error=?7, resets_at=?8",
        params![
            turn.as_str(),
            attempt.attempt,
            attempt.agent.as_str(),
            attempt.status,
            attempt.partial_text,
            class,
            attempt.error,
            resets_at
        ],
    )?;
    Ok(())
}

pub(crate) fn append_turn_event(
    c: &Connection,
    turn: &TurnId,
    kind: &str,
    payload: &Value,
) -> Result<(), HubError> {
    c.execute(
        "INSERT INTO turn_events(turn_id,idx,kind,payload)
         VALUES(?1,(SELECT COALESCE(MAX(idx),0)+1 FROM turn_events WHERE turn_id=?1),?2,?3)",
        params![turn.as_str(), kind, payload.to_string()],
    )?;
    Ok(())
}

/// Turns of a session with attempts, tool calls and permission decisions; also returns client ids.
pub(crate) fn load_turns(
    c: &Connection,
    session: &SessionId,
) -> Result<Vec<(TurnView, Option<String>)>, HubError> {
    load_turns_inner(c, session, false)
}

pub(crate) fn load_turns_slim(
    c: &Connection,
    session: &SessionId,
) -> Result<Vec<(TurnView, Option<String>)>, HubError> {
    load_turns_inner(c, session, true)
}

fn load_turns_inner(
    c: &Connection,
    session: &SessionId,
    slim: bool,
) -> Result<Vec<(TurnView, Option<String>)>, HubError> {
    let mut statement = c.prepare(
        "SELECT id,seq,user_text,agent_id,assistant_text,status,stop_reason,error,created_at,finished_at,client_message_id,
                context_json,usage_json,context_usage_json,origin_json
         FROM turns WHERE session_id=?1 ORDER BY seq",
    )?;
    let mut turns = statement
        .query_map(params![session.as_str()], |r| {
            Ok((
                TurnView {
                    id: TurnId::new(r.get::<_, String>(0)?),
                    seq: r.get(1)?,
                    user_text: r.get(2)?,
                    agent: r.get::<_, Option<String>>(3)?.map(AgentId::new),
                    assistant_text: r.get(4)?,
                    status: parse_status(&r.get::<_, String>(5)?),
                    stop_reason: r.get(6)?,
                    error: r.get(7)?,
                    attempts: Vec::new(),
                    tool_calls: Vec::new(),
                    permissions: Vec::new(),
                    created_at: r.get(8)?,
                    finished_at: r.get(9)?,
                    segments: Vec::new(),
                    context: crate::store_chat::parse_json(r.get::<_, Option<String>>(11)?)
                        .unwrap_or_default(),
                    plan: Vec::new(),
                    usage: crate::store_chat::parse_json(r.get::<_, Option<String>>(12)?),
                    context_usage: crate::store_chat::parse_json(r.get::<_, Option<String>>(13)?),
                    origin: crate::store_chat::parse_json(r.get::<_, Option<String>>(14)?),
                    thoughts: Vec::new(),
                },
                r.get::<_, Option<String>>(10)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    let mut attempts_statement = c.prepare(
        "SELECT attempt,agent_id,status,partial_text,error_class,error FROM turn_attempts WHERE turn_id=?1 ORDER BY attempt",
    )?;
    let mut events_statement =
        c.prepare("SELECT kind,payload FROM turn_events WHERE turn_id=?1 ORDER BY idx")?;
    let keep_from = if slim {
        turns.len().saturating_sub(FULL_TURNS)
    } else {
        0
    };
    for (index, (turn, _)) in turns.iter_mut().enumerate() {
        turn.attempts = attempts_statement
            .query_map(params![turn.id.as_str()], |r| {
                Ok(TurnAttempt {
                    attempt: r.get(0)?,
                    agent: AgentId::new(r.get::<_, String>(1)?),
                    status: r.get(2)?,
                    partial_text: r.get(3)?,
                    error_class: r
                        .get::<_, Option<String>>(4)?
                        .and_then(|s| serde_json::from_str(&s).ok()),
                    error: r.get(5)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        let events = events_statement
            .query_map(params![turn.id.as_str()], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        let mut marks: Vec<ToolMark> = Vec::new();
        for (kind, payload) in events {
            if let Ok(value) = serde_json::from_str::<Value>(&payload) {
                apply_event(turn, &mut marks, &kind, value);
            }
        }
        let last_attempt = turn.attempts.last().map_or(1, |a| a.attempt);
        turn.segments = build_segments(&turn.assistant_text, last_attempt, &marks);
        if index < keep_from {
            slim_turn(turn);
        }
    }
    Ok(turns)
}

const FULL_TURNS: usize = 8;
const SLIM_TEXT: usize = 256;

fn clip(text: &mut String) {
    if text.len() > SLIM_TEXT {
        let mut end = SLIM_TEXT;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
        text.push_str("\n…");
        text.shrink_to_fit();
    }
}

fn slim_turn(turn: &mut TurnView) {
    {
        for tool in &mut turn.tool_calls {
            tool.raw_output = None;
            if let Some(input) = tool.raw_input.as_mut() {
                clip(input);
            }
            for content in &mut tool.content {
                match content {
                    harness_core_api::ToolContent::Text { text } => clip(text),
                    harness_core_api::ToolContent::Diff {
                        path,
                        old_text,
                        new_text,
                    } => {
                        let size = new_text.len() + old_text.as_ref().map_or(0, String::len);
                        if size > SLIM_TEXT * 2 {
                            *content = harness_core_api::ToolContent::Text {
                                text: format!("Edited {path} (diff not kept for older turns)"),
                            };
                        }
                    }
                    harness_core_api::ToolContent::Terminal { .. } => {}
                }
            }
            tool.content.shrink_to_fit();
        }
    }
    turn.tool_calls.shrink_to_fit();
}

/// Folds one stored turn event into the view.
fn apply_event(turn: &mut TurnView, marks: &mut Vec<ToolMark>, kind: &str, value: Value) {
    match kind {
        "tool_call" => {
            let mark = ToolMark::from_payload(&value);
            if let Ok(tool) = serde_json::from_value::<ToolCallView>(value) {
                if !marks.iter().any(|m| m.tool_call_id == tool.tool_call_id) {
                    marks.push(ToolMark {
                        tool_call_id: tool.tool_call_id.clone(),
                        ..mark
                    });
                }
                merge_tool(&mut turn.tool_calls, tool);
            }
        }
        "permission" => {
            let title = value
                .get("title")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            if let Some(Ok(decision)) = value
                .get("decision")
                .cloned()
                .map(serde_json::from_value::<PermissionDecision>)
            {
                turn.permissions.push((title, decision));
            }
        }
        "steer" => {
            if let Some(text) = value.get("text").and_then(Value::as_str) {
                marks.push(ToolMark {
                    steer: Some(text.to_owned()),
                    ..ToolMark::from_payload(&value)
                });
            }
        }
        "plan" => {
            if let Ok(entries) = serde_json::from_value(value) {
                turn.plan = entries;
            }
        }
        "thought" => {
            if let Ok(thought) = serde_json::from_value(value) {
                turn.thoughts.push(thought);
            }
        }
        _ => {}
    }
}

/// Inserts or merges a tool call by id (later non-empty fields win).
pub(crate) fn merge_tool(tools: &mut Vec<ToolCallView>, tool: ToolCallView) {
    if let Some(existing) = tools
        .iter_mut()
        .find(|t| t.tool_call_id == tool.tool_call_id)
    {
        if !tool.title.is_empty() {
            existing.title = tool.title;
        }
        if tool.kind.is_some() {
            existing.kind = tool.kind;
        }
        if tool.status.is_some() {
            existing.status = tool.status;
        }
        if tool.raw_input.is_some() {
            existing.raw_input = tool.raw_input;
        }
        if !tool.content.is_empty() {
            existing.content = tool.content;
        }
        if !tool.locations.is_empty() {
            existing.locations = tool.locations;
        }
        if tool.raw_output.is_some() {
            existing.raw_output = tool.raw_output;
        }
    } else {
        tools.push(tool);
    }
}

/// Where a tool call first appeared in a turn's live stream (spec §15.1 request 4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ToolMark {
    pub tool_call_id: String,
    /// Attempt number; None for rows written before API 0.2.0.
    pub attempt: Option<u32>,
    /// Byte length of the attempt's assistant text when the call appeared.
    pub text_offset: Option<usize>,
    pub steer: Option<String>,
}

impl ToolMark {
    fn from_payload(value: &Value) -> Self {
        Self {
            tool_call_id: String::new(),
            attempt: value
                .get("attempt")
                .and_then(Value::as_u64)
                .and_then(|a| u32::try_from(a).ok()),
            text_offset: value
                .get("textOffset")
                .and_then(Value::as_u64)
                .and_then(|o| usize::try_from(o).ok()),
            steer: None,
        }
    }
}

/// Ordered text/tool segments: tools of earlier attempts first, then the last attempt's text split at the
/// offsets where its tool calls appeared; marks without an offset go after the text.
pub(crate) fn build_segments(
    text: &str,
    last_attempt: u32,
    marks: &[ToolMark],
) -> Vec<TurnSegment> {
    let mut segments = Vec::new();
    let tool = |m: &ToolMark| match &m.steer {
        Some(text) => TurnSegment::Steer { text: text.clone() },
        None => TurnSegment::Tool {
            tool_call_id: m.tool_call_id.clone(),
        },
    };
    for mark in marks
        .iter()
        .filter(|m| m.attempt.is_some_and(|a| a < last_attempt))
    {
        segments.push(tool(mark));
    }
    let mut current: Vec<(usize, &ToolMark)> = marks
        .iter()
        .filter(|m| m.attempt.map_or(true, |a| a >= last_attempt))
        .map(|m| (m.text_offset.unwrap_or(usize::MAX), m))
        .collect();
    current.sort_by_key(|(offset, _)| *offset);
    let mut position = 0;
    for (offset, mark) in current {
        let mut cut = offset.min(text.len());
        while cut > position && !text.is_char_boundary(cut) {
            cut -= 1;
        }
        if cut > position {
            segments.push(TurnSegment::Text {
                text: text[position..cut].to_owned(),
            });
            position = cut;
        }
        if offset == usize::MAX && position < text.len() {
            segments.push(TurnSegment::Text {
                text: text[position..].to_owned(),
            });
            position = text.len();
        }
        segments.push(tool(mark));
    }
    if position < text.len() {
        segments.push(TurnSegment::Text {
            text: text[position..].to_owned(),
        });
    }
    segments
}

/// Distinct agents that served a session, in order of first use (attempts first, then turn agents).
pub(crate) fn agents_used(c: &Connection, session: &SessionId) -> Result<Vec<AgentId>, HubError> {
    let mut statement = c.prepare(
        "SELECT a.agent_id FROM turn_attempts a JOIN turns t ON t.id=a.turn_id
         WHERE t.session_id=?1 ORDER BY t.seq, a.attempt",
    )?;
    let mut agents: Vec<String> = statement
        .query_map(params![session.as_str()], |r| r.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    let mut statement = c.prepare(
        "SELECT agent_id FROM turns WHERE session_id=?1 AND agent_id IS NOT NULL ORDER BY seq",
    )?;
    agents.extend(
        statement
            .query_map(params![session.as_str()], |r| r.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?,
    );
    let mut out: Vec<AgentId> = Vec::new();
    for agent in agents {
        if !out.iter().any(|a| a.as_str() == agent) {
            out.push(AgentId::new(agent));
        }
    }
    Ok(out)
}

// ---- turn snapshots (spec §15.2) ------------------------------------------------------------------

/// Snapshot row of a turn.
#[derive(Debug, Clone, Default)]
pub struct SnapshotRow {
    pub before_tree: Option<String>,
    pub after_tree: Option<String>,
    pub error: Option<String>,
}

pub(crate) fn set_snapshot_before(
    c: &Connection,
    turn: &TurnId,
    tree: Option<&str>,
    error: Option<&str>,
) -> Result<(), HubError> {
    c.execute(
        "INSERT INTO turn_snapshots(turn_id,before_tree,error) VALUES(?1,?2,?3)
         ON CONFLICT(turn_id) DO NOTHING",
        params![turn.as_str(), tree, error],
    )?;
    Ok(())
}

pub(crate) fn set_snapshot_after(
    c: &Connection,
    turn: &TurnId,
    tree: Option<&str>,
    error: Option<&str>,
) -> Result<(), HubError> {
    c.execute(
        "UPDATE turn_snapshots SET after_tree=?2, error=COALESCE(error, ?3) WHERE turn_id=?1",
        params![turn.as_str(), tree, error],
    )?;
    Ok(())
}

pub(crate) fn load_snapshot(
    c: &Connection,
    turn: &TurnId,
) -> Result<Option<SnapshotRow>, HubError> {
    Ok(c.query_row(
        "SELECT before_tree,after_tree,error FROM turn_snapshots WHERE turn_id=?1",
        params![turn.as_str()],
        |r| {
            Ok(SnapshotRow {
                before_tree: r.get(0)?,
                after_tree: r.get(1)?,
                error: r.get(2)?,
            })
        },
    )
    .optional()?)
}

/// Session of a turn.
pub(crate) fn turn_session(c: &Connection, turn: &TurnId) -> Result<Option<SessionId>, HubError> {
    Ok(c.query_row(
        "SELECT session_id FROM turns WHERE id=?1",
        params![turn.as_str()],
        |r| r.get::<_, String>(0),
    )
    .optional()?
    .map(SessionId::new))
}

// ---- handles, quota, settings ---------------------------------------------------------------------

pub(crate) fn upsert_handle(
    c: &Connection,
    session: &SessionId,
    handle: &HandleRow,
) -> Result<(), HubError> {
    c.execute(
        "INSERT INTO agent_handles(session_id,agent_id,native_session_id,last_seq) VALUES(?1,?2,?3,?4)
         ON CONFLICT(session_id,agent_id) DO UPDATE SET native_session_id=?3, last_seq=?4",
        params![session.as_str(), handle.agent.as_str(), handle.native, handle.last_seq],
    )?;
    Ok(())
}

pub(crate) fn load_handles(
    c: &Connection,
    session: &SessionId,
) -> Result<Vec<HandleRow>, HubError> {
    let mut statement = c.prepare(
        "SELECT agent_id,native_session_id,last_seq FROM agent_handles WHERE session_id=?1",
    )?;
    let rows = statement
        .query_map(params![session.as_str()], |r| {
            Ok(HandleRow {
                agent: AgentId::new(r.get::<_, String>(0)?),
                native: r.get(1)?,
                last_seq: r.get(2)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

pub(crate) fn load_quota(c: &Connection) -> Result<HashMap<AgentId, QuotaState>, HubError> {
    let mut statement = c.prepare("SELECT agent_id,state,resets_at,reason FROM agent_quota")?;
    let rows = statement
        .query_map([], |r| {
            let agent = AgentId::new(r.get::<_, String>(0)?);
            let state: String = r.get(1)?;
            let quota = if state == "exhausted" {
                QuotaState::Exhausted {
                    resets_at: r.get(2)?,
                    reason: r.get::<_, Option<String>>(3)?.unwrap_or_default(),
                }
            } else {
                QuotaState::Ok
            };
            Ok((agent, quota))
        })?
        .collect::<Result<HashMap<_, _>, _>>()?;
    Ok(rows)
}

pub(crate) fn save_quota(
    c: &Connection,
    agent: &AgentId,
    quota: &QuotaState,
    now: i64,
) -> Result<(), HubError> {
    let (state, resets_at, reason) = match quota {
        QuotaState::Ok => ("ok", None, None),
        QuotaState::Exhausted { resets_at, reason } => {
            ("exhausted", *resets_at, Some(reason.as_str()))
        }
    };
    c.execute(
        "INSERT INTO agent_quota(agent_id,state,resets_at,reason,updated_at) VALUES(?1,?2,?3,?4,?5)
         ON CONFLICT(agent_id) DO UPDATE SET state=?2, resets_at=?3, reason=?4, updated_at=?5",
        params![agent.as_str(), state, resets_at, reason, now],
    )?;
    Ok(())
}

pub(crate) fn get_setting(c: &Connection, key: &str) -> Result<Option<String>, HubError> {
    Ok(c.query_row(
        "SELECT value FROM settings WHERE key=?1",
        params![key],
        |r| r.get(0),
    )
    .optional()?)
}

pub(crate) fn set_setting(c: &Connection, key: &str, value: &str) -> Result<(), HubError> {
    c.execute(
        "INSERT INTO settings(key,value) VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=?2",
        params![key, value],
    )?;
    Ok(())
}

/// Mode/model/config choices of one session, per agent.
pub(crate) fn load_prefs(
    c: &Connection,
    session: &SessionId,
) -> Result<HashMap<AgentId, crate::options::Prefs>, HubError> {
    let mut statement =
        c.prepare("SELECT agent_id, kind, key, value FROM session_prefs WHERE session_id=?1")?;
    let rows = statement.query_map(params![session.as_str()], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, String>(3)?,
        ))
    })?;
    let mut out: HashMap<AgentId, crate::options::Prefs> = HashMap::new();
    for row in rows {
        let (agent, kind, key, value) = row?;
        let prefs = out.entry(AgentId::new(agent)).or_default();
        match kind.as_str() {
            "mode" => prefs.mode = Some(value),
            "model" => prefs.model = Some(value),
            "config" => {
                prefs.config.insert(key, value);
            }
            _ => {}
        }
    }
    Ok(out)
}

pub(crate) fn save_pref(
    c: &Connection,
    session: &SessionId,
    agent: &AgentId,
    (kind, key, value): (&str, &str, &str),
) -> Result<(), HubError> {
    c.execute(
        "INSERT INTO session_prefs(session_id,agent_id,kind,key,value) VALUES(?1,?2,?3,?4,?5)
         ON CONFLICT(session_id,agent_id,kind,key) DO UPDATE SET value=?5",
        params![session.as_str(), agent.as_str(), kind, key, value],
    )?;
    Ok(())
}

// ---- projects and archive (phase-9-parity.md §9.1.5a) ---------------------------------------------

/// Adds `sessions.archived_at` when missing (additive; `user_version` stays 1).
fn ensure_archive_column(c: &Connection) -> Result<(), HubError> {
    let mut statement = c.prepare("SELECT name FROM pragma_table_info('sessions')")?;
    let has = statement
        .query_map([], |r| r.get::<_, String>(0))?
        .filter_map(Result::ok)
        .any(|name| name == "archived_at");
    if !has {
        c.execute("ALTER TABLE sessions ADD COLUMN archived_at INTEGER", [])?;
    }
    Ok(())
}

/// Keys of removed projects are hidden from unfiltered lists.
const NOT_REMOVED: &str =
    "s.workspace_key NOT IN (SELECT workspace_key FROM projects WHERE removed_at IS NOT NULL)";

/// Project rows: (key, path, sessions, archived, `last_used`, `removed_at`).
pub(crate) type ProjectRow = (String, String, u32, u32, Option<i64>, Option<i64>);

pub(crate) fn list_projects(c: &Connection) -> Result<Vec<ProjectRow>, HubError> {
    let mut statement = c.prepare(
        "WITH keys AS (SELECT workspace_key AS k FROM sessions s
                        WHERE NOT EXISTS(SELECT 1 FROM inbox_routers r WHERE r.session_id=s.id)
                       UNION SELECT workspace_key FROM projects)
         SELECT k,
           COALESCE((SELECT workspace_path FROM sessions WHERE workspace_key=k ORDER BY updated_at DESC LIMIT 1),
                    (SELECT workspace_path FROM projects WHERE workspace_key=k), k),
           (SELECT COUNT(*) FROM sessions WHERE workspace_key=k AND archived_at IS NULL),
           (SELECT COUNT(*) FROM sessions WHERE workspace_key=k AND archived_at IS NOT NULL),
           (SELECT MAX(updated_at) FROM sessions WHERE workspace_key=k),
           (SELECT removed_at FROM projects WHERE workspace_key=k)
         FROM keys",
    )?;
    let count = |n: i64| u32::try_from(n).unwrap_or(u32::MAX);
    let rows = statement
        .query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                count(r.get(2)?),
                count(r.get(3)?),
                r.get(4)?,
                r.get(5)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// Marks a project removed (`Some(ms)`) or restores it (`None`). Returns true when the state changed.
pub(crate) fn set_project_removed(
    c: &Connection,
    key: &str,
    removed_at: Option<i64>,
) -> Result<bool, HubError> {
    let before: Option<Option<i64>> = c
        .query_row(
            "SELECT removed_at FROM projects WHERE workspace_key=?1",
            params![key],
            |r| r.get(0),
        )
        .optional()?;
    if before.flatten().is_some() == removed_at.is_some() {
        return Ok(false);
    }
    c.execute(
        "INSERT INTO projects(workspace_key, workspace_path, removed_at)
         VALUES(?1, COALESCE((SELECT workspace_path FROM sessions WHERE workspace_key=?1
                              ORDER BY updated_at DESC LIMIT 1), ?1), ?2)
         ON CONFLICT(workspace_key) DO UPDATE SET removed_at=?2",
        params![key, removed_at],
    )?;
    Ok(true)
}

pub(crate) fn session_ids_of_project(
    c: &Connection,
    key: &str,
) -> Result<Vec<SessionId>, HubError> {
    let mut statement = c.prepare("SELECT id FROM sessions WHERE workspace_key=?1")?;
    let ids = statement
        .query_map(params![key], |r| r.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(ids.into_iter().map(SessionId::new).collect())
}

/// Of `ids`, the sessions that exist and are archived (or not archived when `archived = false`).
pub(crate) fn filter_archived(
    c: &Connection,
    ids: &[SessionId],
    archived: bool,
) -> Result<Vec<SessionId>, HubError> {
    let mut out = Vec::new();
    for id in ids {
        let state: Option<Option<i64>> = c
            .query_row(
                "SELECT archived_at FROM sessions WHERE id=?1",
                params![id.as_str()],
                |r| r.get(0),
            )
            .optional()?;
        if state.is_some_and(|at| at.is_some() == archived) {
            out.push(id.clone());
        }
    }
    Ok(out)
}

pub(crate) fn archived_of_project(c: &Connection, key: &str) -> Result<Vec<SessionId>, HubError> {
    let mut statement =
        c.prepare("SELECT id FROM sessions WHERE workspace_key=?1 AND archived_at IS NOT NULL")?;
    let ids = statement
        .query_map(params![key], |r| r.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(ids.into_iter().map(SessionId::new).collect())
}

/// Sets (`Some`) or clears (`None`) `archived_at`; returns how many rows changed state.
pub(crate) fn set_archived(
    c: &Connection,
    ids: &[SessionId],
    at: Option<i64>,
) -> Result<u32, HubError> {
    let mut changed = 0u32;
    for id in ids {
        let n = if at.is_some() {
            c.execute(
                "UPDATE sessions SET archived_at=?2 WHERE id=?1 AND archived_at IS NULL",
                params![id.as_str(), at],
            )?
        } else {
            c.execute(
                "UPDATE sessions SET archived_at=NULL WHERE id=?1 AND archived_at IS NOT NULL",
                params![id.as_str()],
            )?
        };
        changed += u32::try_from(n).unwrap_or(0);
    }
    Ok(changed)
}

/// Deletes sessions with every dependent row, in one transaction. Returns how many sessions were deleted.
pub(crate) fn delete_sessions(c: &Connection, ids: &[SessionId]) -> Result<u32, HubError> {
    let tx = c.unchecked_transaction()?;
    let mut deleted = 0u32;
    for id in ids {
        let id = id.as_str();
        for sql in [
            "DELETE FROM turn_events WHERE turn_id IN (SELECT id FROM turns WHERE session_id=?1)",
            "DELETE FROM turn_attempts WHERE turn_id IN (SELECT id FROM turns WHERE session_id=?1)",
            "DELETE FROM turn_snapshots WHERE turn_id IN (SELECT id FROM turns WHERE session_id=?1)",
            "DELETE FROM turns WHERE session_id=?1",
            "DELETE FROM agent_handles WHERE session_id=?1",
            "DELETE FROM session_prefs WHERE session_id=?1",
        ] {
            tx.execute(sql, params![id])?;
        }
        deleted += u32::try_from(tx.execute("DELETE FROM sessions WHERE id=?1", params![id])?)
            .unwrap_or(0);
    }
    tx.commit()?;
    Ok(deleted)
}

/// Rows left in any table for `id` (gate check after delete).
pub(crate) fn rows_of_session(c: &Connection, id: &SessionId) -> Result<i64, HubError> {
    Ok(c.query_row(
        "SELECT (SELECT COUNT(*) FROM sessions WHERE id=?1) + (SELECT COUNT(*) FROM turns WHERE session_id=?1)
              + (SELECT COUNT(*) FROM agent_handles WHERE session_id=?1)
              + (SELECT COUNT(*) FROM session_prefs WHERE session_id=?1)",
        params![id.as_str()],
        |r| r.get(0),
    )?)
}

/// One page of archived sessions and the total.
pub(crate) fn list_archived(
    c: &Connection,
    request: &harness_core_api::ListArchived,
    limit: u32,
) -> Result<(Vec<(SessionSummary, i64)>, u32), HubError> {
    let order = match request.sort {
        harness_core_api::ArchiveSort::ArchivedNewest => "s.archived_at DESC, s.id",
        harness_core_api::ArchiveSort::ArchivedOldest => "s.archived_at ASC, s.id",
        harness_core_api::ArchiveSort::Title => "lower(s.title) ASC, s.id",
    };
    let filter = format!(
        "FROM sessions s WHERE s.archived_at IS NOT NULL AND (?1 IS NULL OR s.workspace_key=?1)
         AND (?1 IS NOT NULL OR {NOT_REMOVED}) AND (?2 IS NULL OR lower(s.title) LIKE ?2)"
    );
    let pattern = request
        .query
        .as_deref()
        .filter(|q| !q.trim().is_empty())
        .map(|q| format!("%{}%", q.trim().to_lowercase()));
    let key = request.workspace_key.as_deref();
    let total: i64 = c.query_row(
        &format!("SELECT COUNT(*) {filter}"),
        params![key, pattern],
        |r| r.get(0),
    )?;
    let mut statement = c.prepare(&format!(
        "SELECT s.id,s.title,s.workspace_path,s.workspace_key,s.active_agent,s.created_at,s.updated_at,
                s.archived_at {filter} ORDER BY {order} LIMIT ?3 OFFSET ?4"
    ))?;
    let rows = statement
        .query_map(
            params![key, pattern, i64::from(limit), i64::from(request.offset)],
            |r| Ok((session_from_row(r)?, r.get::<_, i64>(7)?)),
        )?
        .collect::<Result<Vec<_>, _>>()?;
    let rows = rows
        .iter()
        .map(|(row, at)| Ok((summary(c, row)?, *at)))
        .collect::<Result<Vec<_>, HubError>>()?;
    Ok((rows, u32::try_from(total).unwrap_or(u32::MAX)))
}

pub(crate) fn status_str(status: TurnStatus) -> &'static str {
    match status {
        TurnStatus::Queued => "queued",
        TurnStatus::Running => "running",
        TurnStatus::Paused => "paused",
        TurnStatus::Completed => "completed",
        TurnStatus::Cancelled => "cancelled",
        TurnStatus::Failed => "failed",
        TurnStatus::Interrupted => "interrupted",
    }
}

fn parse_status(s: &str) -> TurnStatus {
    match s {
        "queued" => TurnStatus::Queued,
        "running" => TurnStatus::Running,
        "paused" => TurnStatus::Paused,
        "completed" => TurnStatus::Completed,
        "cancelled" => TurnStatus::Cancelled,
        "failed" => TurnStatus::Failed,
        _ => TurnStatus::Interrupted,
    }
}
