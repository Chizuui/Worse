//! Store additions of API 0.5.0 (phase-9-parity.md §9.2.2): sidebar columns, groups, turn truncation and copies,
//! schedules. Additive migrations; `user_version` stays 1. Every function runs inside `Store::run` (single writer).

use harness_core_api::{
    ForkOrigin, GroupColor, ScheduledPrompt, SessionGroup, SessionId, SessionSummary, TurnId,
};
use rusqlite::{params, Connection, OptionalExtension};
use serde::de::DeserializeOwned;
use serde::Serialize;

use crate::error::HubError;

const TABLES: &str = "
CREATE TABLE IF NOT EXISTS session_groups(
  id TEXT PRIMARY KEY, name TEXT NOT NULL, color TEXT NOT NULL, position INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS schedules(
  id TEXT PRIMARY KEY, json TEXT NOT NULL, created_at INTEGER NOT NULL);
";

/// Adds the 0.5.0 columns and tables when missing.
pub(crate) fn migrate(c: &Connection) -> Result<(), HubError> {
    c.execute_batch(TABLES)?;
    c.execute_batch(crate::delegate::TABLE)?;
    c.execute_batch(crate::board::TABLE)?;
    c.execute_batch(crate::inbox::TABLES)?;
    c.execute_batch(crate::board_sync::TABLE)?;
    for (table, column, kind) in [
        ("sessions", "pinned_at", "INTEGER"),
        ("sessions", "group_id", "TEXT"),
        ("sessions", "sort_order", "INTEGER"),
        ("sessions", "unread_mark", "INTEGER"),
        ("sessions", "read_at", "INTEGER"),
        ("sessions", "forked_from_session", "TEXT"),
        ("sessions", "forked_from_seq", "INTEGER"),
        ("sessions", "imported_from", "TEXT"),
        ("sessions", "worktree_json", "TEXT"),
        ("turns", "context_json", "TEXT"),
        ("turns", "usage_json", "TEXT"),
        ("turns", "context_usage_json", "TEXT"),
        ("turns", "origin_json", "TEXT"),
        ("turn_snapshots", "undone", "INTEGER"),
    ] {
        if !has_column(c, table, column)? {
            c.execute(
                &format!("ALTER TABLE {table} ADD COLUMN {column} {kind}"),
                [],
            )?;
            if column == "read_at" {
                // 迁移前的会话视为已读，否则升级后所有历史会话都会突然显示为未读。
                c.execute("UPDATE sessions SET read_at=updated_at", [])?;
            }
        }
    }
    Ok(())
}

fn has_column(c: &Connection, table: &str, column: &str) -> Result<bool, HubError> {
    let mut statement = c.prepare(&format!("SELECT name FROM pragma_table_info('{table}')"))?;
    let found = statement
        .query_map([], |r| r.get::<_, String>(0))?
        .filter_map(Result::ok)
        .any(|name| name == column);
    Ok(found)
}

pub(crate) fn json_opt<T: Serialize>(value: Option<&T>) -> Result<Option<String>, HubError> {
    value
        .map(serde_json::to_string)
        .transpose()
        .map_err(HubError::from)
}

pub(crate) fn json_list<T: Serialize>(value: &[T]) -> Result<Option<String>, HubError> {
    if value.is_empty() {
        Ok(None)
    } else {
        Ok(Some(serde_json::to_string(value)?))
    }
}

pub(crate) fn parse_json<T: DeserializeOwned>(text: Option<String>) -> Option<T> {
    text.and_then(|t| serde_json::from_str(&t).ok())
}

/// Fills the 0.5.0 sidebar fields of a summary from the session row.
pub(crate) fn decorate(c: &Connection, summary: &mut SessionSummary) -> Result<(), HubError> {
    let row = c
        .query_row(
            "SELECT pinned_at, group_id, COALESCE(sort_order, 0), COALESCE(unread_mark, 0), read_at,
                    forked_from_session, forked_from_seq, imported_from,
                    (SELECT MAX(finished_at) FROM turns WHERE session_id=?1), worktree_json
             FROM sessions WHERE id=?1",
            params![summary.id.as_str()],
            |r| {
                Ok((
                    r.get::<_, Option<i64>>(0)?,
                    r.get::<_, Option<String>>(1)?,
                    r.get::<_, i64>(2)?,
                    r.get::<_, i64>(3)?,
                    r.get::<_, Option<i64>>(4)?,
                    r.get::<_, Option<String>>(5)?,
                    r.get::<_, Option<u32>>(6)?,
                    r.get::<_, Option<String>>(7)?,
                    r.get::<_, Option<i64>>(8)?,
                    r.get::<_, Option<String>>(9)?,
                ))
            },
        )
        .optional()?;
    let Some((
        pinned,
        group,
        order,
        mark,
        read_at,
        fork_session,
        fork_seq,
        imported,
        finished,
        worktree,
    )) = row
    else {
        return Ok(());
    };
    summary.parent_id = crate::delegate::parent_of(c, &summary.id)?;
    summary.pinned = pinned.is_some();
    summary.group_id = group;
    summary.order = order;
    summary.unread = mark != 0 || finished.is_some_and(|f| f > read_at.unwrap_or(i64::MIN));
    summary.forked_from = fork_session.map(|session_id| ForkOrigin {
        session_id: SessionId::new(session_id),
        turn_seq: fork_seq.unwrap_or(0),
    });
    summary.imported_from = imported;
    summary.worktree = parse_json(worktree);
    Ok(())
}

pub(crate) fn set_worktree(
    c: &Connection,
    id: &SessionId,
    info: &harness_core_api::WorktreeInfo,
) -> Result<(), HubError> {
    c.execute(
        "UPDATE sessions SET worktree_json=?2 WHERE id=?1",
        params![id.as_str(), serde_json::to_string(info)?],
    )?;
    Ok(())
}

pub(crate) fn session_cost_usd(c: &Connection, id: &SessionId) -> Result<Option<f64>, HubError> {
    let mut statement = c.prepare(
        "SELECT COALESCE(agent_id, ''), context_usage_json FROM turns
         WHERE session_id=?1 AND context_usage_json IS NOT NULL ORDER BY seq",
    )?;
    let rows = statement
        .query_map(params![id.as_str()], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })?
        .filter_map(Result::ok);
    let mut last: std::collections::HashMap<String, f64> = std::collections::HashMap::new();
    let mut total = 0.0_f64;
    let mut any = false;
    for (agent, json) in rows {
        let Some(usage) = parse_json::<harness_core_api::chat::ContextUsage>(Some(json)) else {
            continue;
        };
        let Some(cost) = usage.cost else {
            continue;
        };
        if !cost.currency.eq_ignore_ascii_case("USD") {
            continue;
        }
        let Ok(amount) = cost.amount.trim().parse::<f64>() else {
            continue;
        };
        if !amount.is_finite() || amount < 0.0 {
            continue;
        }
        let previous = last.insert(agent, amount).unwrap_or(0.0);
        total += if amount >= previous {
            amount - previous
        } else {
            amount
        };
        any = true;
    }
    Ok(any.then_some(total))
}

pub(crate) fn session_cost(
    c: &Connection,
    id: &SessionId,
) -> Result<Option<harness_core_api::SessionCost>, HubError> {
    let own = session_cost_usd(c, id)?;
    let mut statement = c.prepare("SELECT child_id FROM subagents WHERE parent_id=?1")?;
    let children: Vec<String> = statement
        .query_map(params![id.as_str()], |r| r.get::<_, String>(0))?
        .filter_map(Result::ok)
        .collect();
    let mut sub: Option<f64> = None;
    for child in children {
        if let Some(amount) = session_cost_usd(c, &SessionId::new(child))? {
            sub = Some(sub.unwrap_or(0.0) + amount);
        }
    }
    if own.is_none() && sub.is_none() {
        return Ok(None);
    }
    let usd = |amount: f64| harness_core_api::chat::Cost {
        amount: format!("{amount:.4}"),
        currency: "USD".into(),
    };
    let own_amount = own.unwrap_or(0.0);
    Ok(Some(harness_core_api::SessionCost {
        own: usd(own_amount),
        subagents: sub.map(usd),
        total: usd(own_amount + sub.unwrap_or(0.0)),
    }))
}
pub(crate) fn worktrees_of(
    c: &Connection,
    ids: &[SessionId],
) -> Result<Vec<harness_core_api::WorktreeInfo>, HubError> {
    let mut found = Vec::new();
    for id in ids {
        let json: Option<String> = c
            .query_row(
                "SELECT worktree_json FROM sessions WHERE id=?1",
                params![id.as_str()],
                |r| r.get(0),
            )
            .optional()?
            .flatten();
        if let Some(info) = parse_json(json) {
            found.push(info);
        }
    }
    Ok(found)
}

/// Marks a new session read at creation (new sessions start read).
pub(crate) fn mark_read(c: &Connection, id: &SessionId, now: i64) -> Result<bool, HubError> {
    Ok(c.execute(
        "UPDATE sessions SET read_at=?2, unread_mark=0 WHERE id=?1",
        params![id.as_str(), now],
    )? > 0)
}

pub(crate) fn mark_unread(c: &Connection, id: &SessionId) -> Result<bool, HubError> {
    Ok(c.execute(
        "UPDATE sessions SET unread_mark=1 WHERE id=?1",
        params![id.as_str()],
    )? > 0)
}

pub(crate) fn set_pinned(
    c: &Connection,
    ids: &[SessionId],
    at: Option<i64>,
) -> Result<Vec<SessionId>, HubError> {
    let mut changed = Vec::new();
    for id in ids {
        if c.execute(
            "UPDATE sessions SET pinned_at=?2 WHERE id=?1",
            params![id.as_str(), at],
        )? > 0
        {
            changed.push(id.clone());
        }
    }
    Ok(changed)
}

pub(crate) fn session_exists(c: &Connection, id: &SessionId) -> Result<bool, HubError> {
    Ok(c.query_row(
        "SELECT 1 FROM sessions WHERE id=?1",
        params![id.as_str()],
        |_| Ok(()),
    )
    .optional()?
    .is_some())
}

// ---- groups ---------------------------------------------------------------------------------------

pub(crate) fn list_groups(c: &Connection) -> Result<Vec<SessionGroup>, HubError> {
    let mut statement =
        c.prepare("SELECT id,name,color,position FROM session_groups ORDER BY position, id")?;
    let rows = statement
        .query_map([], |r| {
            Ok(SessionGroup {
                id: r.get(0)?,
                name: r.get(1)?,
                color: GroupColor::parse(&r.get::<_, String>(2)?),
                position: r.get(3)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

pub(crate) fn insert_group(c: &Connection, group: &SessionGroup) -> Result<(), HubError> {
    c.execute(
        "INSERT INTO session_groups(id,name,color,position) VALUES(?1,?2,?3,?4)",
        params![group.id, group.name, group.color.as_str(), group.position],
    )?;
    Ok(())
}

pub(crate) fn update_group(c: &Connection, group: &SessionGroup) -> Result<(), HubError> {
    c.execute(
        "UPDATE session_groups SET name=?2, color=?3, position=?4 WHERE id=?1",
        params![group.id, group.name, group.color.as_str(), group.position],
    )?;
    Ok(())
}

/// Deletes a group and ungroups its sessions (one transaction). Returns the ungrouped sessions.
pub(crate) fn delete_group(c: &Connection, id: &str) -> Result<Vec<SessionId>, HubError> {
    let tx = c.unchecked_transaction()?;
    let members = {
        let mut statement = tx.prepare("SELECT id FROM sessions WHERE group_id=?1")?;
        let rows = statement
            .query_map(params![id], |r| r.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        rows
    };
    tx.execute(
        "UPDATE sessions SET group_id=NULL WHERE group_id=?1",
        params![id],
    )?;
    tx.execute("DELETE FROM session_groups WHERE id=?1", params![id])?;
    tx.commit()?;
    Ok(members.into_iter().map(SessionId::new).collect())
}

pub(crate) fn set_group_positions(c: &Connection, ids: &[String]) -> Result<(), HubError> {
    let tx = c.unchecked_transaction()?;
    for (position, id) in ids.iter().enumerate() {
        tx.execute(
            "UPDATE session_groups SET position=?2 WHERE id=?1",
            params![id, i64::try_from(position).unwrap_or(i64::MAX)],
        )?;
    }
    tx.commit()?;
    Ok(())
}

/// Moves sessions into a group on top of the section: the first id gets the highest order.
pub(crate) fn move_sessions(
    c: &Connection,
    ids: &[SessionId],
    group: Option<&str>,
    now: i64,
) -> Result<(), HubError> {
    let tx = c.unchecked_transaction()?;
    let count = i64::try_from(ids.len()).unwrap_or(i64::MAX);
    // 修复：同一毫秒内连续两次"移到顶部"会得到相同的 now_ms，后移的会话不在最上面（phase9-chat 门禁复现）。
    // 以现有最大排序值 + 1 为下限，保证每次移动都严格位于所有已有会话之上。
    let highest: Option<i64> =
        tx.query_row("SELECT MAX(sort_order) FROM sessions", [], |row| row.get(0))?;
    let now = highest.map_or(now, |h| now.max(h.saturating_add(1)));
    for (index, id) in ids.iter().enumerate() {
        let order = now + count - i64::try_from(index).unwrap_or(0);
        tx.execute(
            "UPDATE sessions SET group_id=?2, sort_order=?3 WHERE id=?1",
            params![id.as_str(), group, order],
        )?;
    }
    tx.commit()?;
    Ok(())
}

// ---- turn truncation and copies -------------------------------------------------------------------

/// Deletes turns with `seq >= from_seq` (all turn tables) and every native handle of the session.
pub(crate) fn truncate_turns(
    c: &Connection,
    session: &SessionId,
    from_seq: u32,
) -> Result<(), HubError> {
    let tx = c.unchecked_transaction()?;
    let id = session.as_str();
    for sql in [
        "DELETE FROM turn_events WHERE turn_id IN (SELECT id FROM turns WHERE session_id=?1 AND seq>=?2)",
        "DELETE FROM turn_attempts WHERE turn_id IN (SELECT id FROM turns WHERE session_id=?1 AND seq>=?2)",
        "DELETE FROM turn_snapshots WHERE turn_id IN (SELECT id FROM turns WHERE session_id=?1 AND seq>=?2)",
        "DELETE FROM turns WHERE session_id=?1 AND seq>=?2",
    ] {
        tx.execute(sql, params![id, from_seq])?;
    }
    tx.execute("DELETE FROM agent_handles WHERE session_id=?1", params![id])?;
    tx.commit()?;
    Ok(())
}

/// Copies turns of `from` (seq ≤ `up_to_seq`) to `to` with new ids; attempts and events are copied too.
pub(crate) fn copy_turns(
    c: &Connection,
    from: &SessionId,
    to: &SessionId,
    up_to_seq: u32,
    new_id: &mut dyn FnMut() -> String,
) -> Result<(), HubError> {
    let tx = c.unchecked_transaction()?;
    let turns: Vec<String> = {
        let mut statement = tx.prepare(
            "SELECT id FROM turns WHERE session_id=?1 AND seq<=?2
             AND status NOT IN ('queued','running','paused') ORDER BY seq",
        )?;
        let rows = statement
            .query_map(params![from.as_str(), up_to_seq], |r| r.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        rows
    };
    for old in turns {
        let new = new_id();
        tx.execute(
            "INSERT INTO turns(id,session_id,seq,client_message_id,user_text,agent_id,assistant_text,status,
                               stop_reason,error,created_at,finished_at,context_json,usage_json,
                               context_usage_json,origin_json)
             SELECT ?2,?3,seq,NULL,user_text,agent_id,assistant_text,status,stop_reason,error,created_at,
                    finished_at,context_json,usage_json,context_usage_json,origin_json FROM turns WHERE id=?1",
            params![old, new, to.as_str()],
        )?;
        tx.execute(
            "INSERT INTO turn_attempts(turn_id,attempt,agent_id,status,partial_text,error_class,error,resets_at)
             SELECT ?2,attempt,agent_id,status,partial_text,error_class,error,resets_at
             FROM turn_attempts WHERE turn_id=?1",
            params![old, new],
        )?;
        tx.execute(
            "INSERT INTO turn_events(turn_id,idx,kind,payload)
             SELECT ?2,idx,kind,payload FROM turn_events WHERE turn_id=?1",
            params![old, new],
        )?;
    }
    tx.commit()?;
    Ok(())
}

pub(crate) fn set_fork_origin(
    c: &Connection,
    session: &SessionId,
    origin: &ForkOrigin,
) -> Result<(), HubError> {
    c.execute(
        "UPDATE sessions SET forked_from_session=?2, forked_from_seq=?3 WHERE id=?1",
        params![
            session.as_str(),
            origin.session_id.as_str(),
            origin.turn_seq
        ],
    )?;
    Ok(())
}

pub(crate) fn set_undone(c: &Connection, turn: &TurnId, undone: bool) -> Result<(), HubError> {
    c.execute(
        "UPDATE turn_snapshots SET undone=?2 WHERE turn_id=?1",
        params![turn.as_str(), i64::from(undone)],
    )?;
    Ok(())
}

pub(crate) fn is_undone(c: &Connection, turn: &TurnId) -> Result<bool, HubError> {
    Ok(c.query_row(
        "SELECT COALESCE(undone,0) FROM turn_snapshots WHERE turn_id=?1",
        params![turn.as_str()],
        |r| r.get::<_, i64>(0),
    )
    .optional()?
    .is_some_and(|v| v != 0))
}

// ---- import ---------------------------------------------------------------------------------------

/// Every `imported_from` value in the store.
pub(crate) fn imported_keys(c: &Connection) -> Result<Vec<String>, HubError> {
    let mut statement =
        c.prepare("SELECT imported_from FROM sessions WHERE imported_from IS NOT NULL")?;
    let rows = statement
        .query_map([], |r| r.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

pub(crate) fn native_session_ids(c: &Connection) -> Result<Vec<String>, HubError> {
    let mut statement = c.prepare("SELECT DISTINCT native_session_id FROM agent_handles")?;
    let rows = statement
        .query_map([], |r| r.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

pub(crate) fn set_imported_from(
    c: &Connection,
    session: &SessionId,
    key: &str,
) -> Result<(), HubError> {
    c.execute(
        "UPDATE sessions SET imported_from=?2 WHERE id=?1",
        params![session.as_str(), key],
    )?;
    Ok(())
}

// ---- schedules ------------------------------------------------------------------------------------

pub(crate) fn list_schedules(c: &Connection) -> Result<Vec<ScheduledPrompt>, HubError> {
    let mut statement = c.prepare("SELECT json FROM schedules ORDER BY created_at, id")?;
    let rows = statement
        .query_map([], |r| r.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows
        .into_iter()
        .filter_map(|json| serde_json::from_str(&json).ok())
        .collect())
}

pub(crate) fn save_schedule(c: &Connection, schedule: &ScheduledPrompt) -> Result<(), HubError> {
    c.execute(
        "INSERT INTO schedules(id,json,created_at) VALUES(?1,?2,?3)
         ON CONFLICT(id) DO UPDATE SET json=?2",
        params![
            schedule.id,
            serde_json::to_string(schedule)?,
            schedule.created_at
        ],
    )?;
    Ok(())
}

pub(crate) fn delete_schedule(c: &Connection, id: &str) -> Result<bool, HubError> {
    Ok(c.execute("DELETE FROM schedules WHERE id=?1", params![id])? > 0)
}

/// Sessions whose transcript contains `pattern` (case-insensitive), newest first, with a short snippet.
pub(crate) fn transcript_matches(
    c: &Connection,
    pattern: &str,
    limit: u32,
) -> Result<Vec<(SessionId, String)>, HubError> {
    let needle = pattern.to_lowercase();
    let mut stmt = c.prepare(
        "SELECT t.session_id, t.user_text, t.assistant_text FROM turns t
         WHERE instr(lower(t.user_text), ?1) > 0 OR instr(lower(t.assistant_text), ?1) > 0
         ORDER BY t.created_at DESC LIMIT 2000",
    )?;
    let rows = stmt.query_map(params![needle], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
        ))
    })?;
    let mut out: Vec<(SessionId, String)> = Vec::new();
    for row in rows {
        let (id, user, assistant) = row?;
        let id = SessionId::new(id);
        if out.iter().any(|(seen, _)| seen == &id) {
            continue;
        }
        let text = if user.to_lowercase().contains(&needle) {
            user
        } else {
            assistant
        };
        out.push((id, snippet(&text, &needle)));
        if out.len() >= limit as usize {
            break;
        }
    }
    Ok(out)
}

fn snippet(text: &str, needle: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let lower: Vec<char> = text
        .chars()
        .map(|ch| ch.to_lowercase().next().unwrap_or(ch))
        .collect();
    let needle: Vec<char> = needle.chars().collect();
    let start = (0..=lower.len().saturating_sub(needle.len()))
        .find(|&i| lower.get(i..i + needle.len()) == Some(&needle[..]))
        .unwrap_or(0);
    let from = start.saturating_sub(30);
    let to = (start + needle.len() + 50).min(chars.len());
    chars[from..to]
        .iter()
        .collect::<String>()
        .replace('\n', " ")
}

/// Replaces the user text of a (queued) turn (phase-9-parity.md §9.2.5 R12).
pub(crate) fn set_user_text(c: &Connection, turn: &TurnId, text: &str) -> Result<(), HubError> {
    c.execute(
        "UPDATE turns SET user_text=?2 WHERE id=?1",
        params![turn.as_str(), text],
    )?;
    Ok(())
}
