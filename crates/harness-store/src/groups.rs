//! Port of `TaskGroupRepository.read` (`packages/services/src/task-groups/repository.ts`):
//! three queries in one read snapshot, rows returned as node:sqlite objects (column order, raw values).

use rusqlite::Connection;

use crate::json::{Obj, Sql, J};
use crate::StoreError;

const GROUPS_SQL: &str = "SELECT group_id AS id,title,coalesce(color,'gray') AS color,created_at AS createdAt,updated_at AS updatedAt FROM task_groups ORDER BY created_at,group_id";
const MEMBERS_SQL: &str = "SELECT group_id AS groupId,workspace_key AS workspaceKey,task_id AS taskId,coalesce(sort_order,-added_at) AS sortOrder FROM task_group_members ORDER BY sortOrder,workspace_key,task_id";
const ORDER_SQL: &str = "SELECT node_type AS type,node_key AS key,sort_order AS sortOrder FROM task_group_view_node_orders ORDER BY sort_order,node_type,node_key";

fn rows(conn: &Connection, sql: &str) -> Result<J, StoreError> {
    let mut stmt = conn.prepare_cached(sql)?;
    let names: Vec<String> = stmt
        .column_names()
        .iter()
        .map(|name| (*name).to_owned())
        .collect();
    let mut cursor = stmt.query([])?;
    let mut out = Vec::new();
    while let Some(row) = cursor.next()? {
        let mut obj = Obj::new();
        for (index, name) in names.iter().enumerate() {
            obj.set(name, Sql::from_ref(row.get_ref(index)?)?.raw_json()?);
        }
        out.push(J::from(obj));
    }
    Ok(J::Arr(out))
}

/// `{groups, members, order}`.
pub(crate) fn read(conn: &Connection) -> Result<J, StoreError> {
    Ok(J::from(
        Obj::new()
            .with("groups", rows(conn, GROUPS_SQL)?)
            .with("members", rows(conn, MEMBERS_SQL)?)
            .with("order", rows(conn, ORDER_SQL)?),
    ))
}
