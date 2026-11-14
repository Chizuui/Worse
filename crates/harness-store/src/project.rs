//! Port of `ProjectRepository.readTree` (`packages/services/src/project-storage/project-repository.ts`)
//! and the service-level `readTree` / `readProjects` projections (`packages/services/src/project/index.ts`).

use std::collections::HashMap;

use rusqlite::{Connection, Row};

use crate::identity::is_remote_workspace_identity;
use crate::json::{is_nullish, spread_json, Obj, Sql, J};
use crate::roots::{is_absolute, RootProbe};
use crate::StoreError;

/// `ZCODE_AGENT_PROVIDER` (`packages/shared/src/zcode-agent-policy.ts`).
const ZCODE_AGENT_PROVIDER: &str = "glm";
/// The desktop Host's project-storage owner (`projectStorageHostId` unset).
pub const OWNER_HOST_ID: &str = "local";

/// A `SELECT *` row by column name, with node:sqlite's full-row conversion (an unsafe integer in
/// any column makes TS throw, so it makes Rust fall back).
struct RowMap(HashMap<String, Sql>);

impl RowMap {
    fn read(row: &Row<'_>, names: &[String]) -> Result<Self, StoreError> {
        let mut map = HashMap::with_capacity(names.len());
        for (index, name) in names.iter().enumerate() {
            map.insert(name.clone(), Sql::from_ref(row.get_ref(index)?)?);
        }
        Ok(Self(map))
    }

    /// Missing column = JS `undefined`; only columns the pinned schema has are read.
    fn col(&self, name: &str) -> Result<&Sql, StoreError> {
        self.0
            .get(name)
            .ok_or_else(|| StoreError::fallback(format!("column {name} missing")))
    }
}

fn select_all(
    conn: &Connection,
    sql: &str,
    params: &[&dyn rusqlite::ToSql],
) -> Result<Vec<RowMap>, StoreError> {
    let mut stmt = conn.prepare_cached(sql)?;
    let names: Vec<String> = stmt
        .column_names()
        .iter()
        .map(|name| (*name).to_owned())
        .collect();
    let mut rows = stmt.query(params)?;
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        out.push(RowMap::read(row, &names)?);
    }
    Ok(out)
}

/// A project as read from SQLite plus what the status projection needs.
struct Project {
    json: Obj,
    host_id: String,
    deleted: bool,
    primary: Option<String>,
}

fn read_projects(conn: &Connection, include_deleted: bool) -> Result<Vec<Project>, StoreError> {
    let roots = select_all(
        conn,
        "SELECT project_id,root_path,position,is_primary FROM project_roots ORDER BY position,root_path",
        &[],
    )?;
    let sql = if include_deleted {
        "SELECT * FROM projects  ORDER BY position,project_id"
    } else {
        "SELECT * FROM projects WHERE deleted=0 ORDER BY position,project_id"
    };
    let mut out = Vec::new();
    for row in select_all(conn, sql, &[])? {
        let project_id = row.col("project_id")?;
        let mut root_values = Vec::new();
        let mut primary = None;
        for root in &roots {
            if !root.col("project_id")?.strict_eq(project_id) {
                continue;
            }
            let root_path = root.col("root_path")?.js_string()?;
            let is_primary = root.col("is_primary")?.is_one();
            // TS: roots.find((root) => root.isPrimary)?.rootPath —— 取第一个主要目录。
            if is_primary && primary.is_none() {
                primary = Some(root_path.clone());
            }
            root_values.push(J::from(
                Obj::new()
                    .with("rootPath", J::str(&root_path))
                    .with("position", root.col("position")?.js_number()?)
                    .with("isPrimary", J::bool(is_primary)),
            ));
        }
        let host_id = row.col("host_id")?.js_string()?;
        let deleted = row.col("deleted")?.is_one();
        let json = Obj::new()
            .with("id", J::str(&project_id.js_string()?))
            .with("hostId", J::str(&host_id))
            .with("name", J::str(&row.col("name")?.js_string()?))
            .with(
                "nameOverridden",
                J::bool(row.col("name_overridden")?.is_one()),
            )
            .with("position", row.col("position")?.js_number()?)
            .with("pinned", J::bool(row.col("pinned")?.is_one()))
            .with("deleted", J::bool(deleted))
            .with("createdAt", row.col("created_at")?.js_number()?)
            .with("updatedAt", row.col("updated_at")?.js_number()?)
            .with("roots", J::Arr(root_values));
        out.push(Project {
            json,
            host_id,
            deleted,
            primary,
        });
    }
    Ok(out)
}

fn read_deleted_tasks(conn: &Connection) -> Result<Vec<J>, StoreError> {
    let rows = select_all(
        conn,
        "SELECT task_id AS taskId,workspace_path AS workspacePath,workspace_identity AS workspaceIdentity,host_id AS hostId \
         FROM tasks WHERE host_id=? AND (deleted=1 OR coalesce(cli_deleted,0)=1)",
        &[&OWNER_HOST_ID],
    )?;
    rows.iter()
        .map(|row| {
            let mut obj = Obj::new()
                .with("taskId", J::str(&row.col("taskId")?.js_string()?))
                .with(
                    "workspacePath",
                    J::str(&row.col("workspacePath")?.js_string()?),
                )
                .with("hostId", J::str(&row.col("hostId")?.js_string()?));
            if let Some(identity) = row.col("workspaceIdentity")?.as_text() {
                obj.set("workspaceIdentity", J::str(identity));
            }
            Ok(J::from(obj))
        })
        .collect()
}

fn new_trace_id() -> J {
    // generateTraceId 每次读取都生成随机 UUID（crypto.randomUUID，v4 小写）。
    J::str(&uuid::Uuid::new_v4().to_string())
}

fn read_tasks(conn: &Connection) -> Result<Vec<J>, StoreError> {
    let rows = select_all(
        conn,
        "SELECT * FROM tasks WHERE provider=? AND deleted=0 AND coalesce(cli_deleted,0)=0 AND cli_created_at IS NOT NULL ORDER BY cli_updated_at DESC,task_id",
        &[&ZCODE_AGENT_PROVIDER],
    )?;
    let mut out = Vec::with_capacity(rows.len());
    for row in &rows {
        let mut task = match row.col("meta_json")?.as_text() {
            Some(text) => spread_json(text)?,
            None => Obj::new(),
        };
        // meta.traceId ?? generateTraceId(...) / meta.mode ?? "default"
        let trace_id = match task.get("traceId") {
            Some(value) if !is_nullish(Some(value)) => value.clone(),
            _ => new_trace_id(),
        };
        let mode = match task.get("mode") {
            Some(value) if !is_nullish(Some(value)) => value.clone(),
            _ => J::str("default"),
        };
        let task_id = row.col("task_id")?.js_string()?;
        let workspace_key = row.col("workspace_key")?;
        let workspace_identity = row.col("workspace_identity")?;
        let identity = if workspace_identity.strict_eq(workspace_key) {
            J::str(&workspace_identity.js_string()?)
        } else {
            let key = workspace_key.js_string()?;
            if is_remote_workspace_identity(&key) {
                J::str(&key)
            } else {
                J::Undef
            }
        };
        let title_overridden = row.col("title_overridden")?.is_one();
        let title = if title_overridden {
            row.col("title")?.js_string()?
        } else {
            match row.col("cli_title")? {
                Sql::Null => String::new(),
                other => other.js_string()?,
            }
        };
        let outcome = row.col("cli_last_outcome")?;
        let status = if outcome.as_text() == Some("error") {
            J::str("error")
        } else if outcome.truthy() {
            J::str("completed")
        } else {
            J::Undef
        };
        let unread_at = row.col("unread_at")?;
        let project_id = row.col("project_id")?;
        let host_id = row.col("host_id")?.js_string()?;
        task.set("taskId", J::str(&task_id));
        task.set("traceId", trace_id);
        task.set("workspacePath", J::str(&row.col("root_path")?.js_string()?));
        task.set("workspaceIdentity", identity);
        task.set("title", J::str(&title));
        task.set(
            "titleOverridden",
            J::bool(title_overridden || row.col("cli_title_source")?.as_text() == Some("custom")),
        );
        task.set("createdAt", row.col("cli_created_at")?.js_number()?);
        task.set("updatedAt", row.col("cli_updated_at")?.js_number()?);
        task.set("status", status);
        task.set("mode", mode);
        task.set("pinned", J::bool(row.col("pinned")?.is_one()));
        task.set("archived", J::bool(row.col("archived")?.is_one()));
        task.set(
            "unreadAt",
            if unread_at.is_null() {
                J::Undef
            } else {
                unread_at.js_number()?
            },
        );
        task.set(
            "projectId",
            if project_id.is_null() {
                J::null()
            } else {
                J::str(&project_id.js_string()?)
            },
        );
        task.set("hostId", J::str(&host_id));
        // TS: readTasks(...).filter((task) => task.hostId === owner)
        if host_id == OWNER_HOST_ID {
            out.push(J::from(task));
        }
    }
    Ok(out)
}

/// Options of the service-level reads.
#[derive(Debug, Clone, Copy, Default)]
pub struct TreeOptions {
    /// `includeDeleted` (truthy).
    pub include_deleted: bool,
    /// `includeDeletedTasks` (truthy).
    pub include_deleted_tasks: bool,
    /// `false` for `readProjects` (`includeTasks: false`).
    pub include_tasks: bool,
}

/// The repository read, inside the caller's read transaction.
pub(crate) struct Tree {
    projects: Vec<Project>,
    deleted_tasks: Option<Vec<J>>,
    tasks: Vec<J>,
}

pub(crate) fn read_tree(conn: &Connection, options: TreeOptions) -> Result<Tree, StoreError> {
    let projects = read_projects(conn, options.include_deleted)?
        .into_iter()
        .filter(|project| project.host_id == OWNER_HOST_ID)
        .collect();
    let deleted_tasks = if options.include_deleted_tasks {
        Some(read_deleted_tasks(conn)?)
    } else {
        None
    };
    let tasks = if options.include_tasks {
        read_tasks(conn)?
    } else {
        Vec::new()
    };
    Ok(Tree {
        projects,
        deleted_tasks,
        tasks,
    })
}

/// `projectPrimaryRootStatuses`: appended `primaryRootStatus` for live projects with a primary root.
fn with_root_statuses(projects: Vec<Project>, probe: &RootProbe) -> Result<Vec<J>, StoreError> {
    let wanted: Vec<&str> = projects
        .iter()
        .filter(|project| !project.deleted)
        .filter_map(|project| project.primary.as_deref())
        .filter(|path| !path.is_empty())
        .collect();
    if let Some(relative) = wanted.iter().find(|path| !is_absolute(path)) {
        return Err(StoreError::fallback(format!(
            "relative primary root {relative:?}"
        )));
    }
    let statuses = probe.statuses(&wanted);
    let mut next = statuses.into_iter();
    let mut out = Vec::with_capacity(projects.len());
    for project in projects {
        let mut json = project.json;
        let probed = !project.deleted
            && project
                .primary
                .as_deref()
                .is_some_and(|path| !path.is_empty());
        if probed {
            let status = next
                .next()
                .ok_or_else(|| StoreError::fallback("root status count mismatch"))?;
            json.set("primaryRootStatus", J::str(status.as_str()));
        }
        out.push(J::from(json));
    }
    Ok(out)
}

impl Tree {
    /// `{ ...tree, projects }` in TS key order: projects, deletedTasks?, tasks.
    pub(crate) fn into_json(self, probe: &RootProbe) -> Result<J, StoreError> {
        let mut obj = Obj::new().with(
            "projects",
            J::Arr(with_root_statuses(self.projects, probe)?),
        );
        if let Some(deleted) = self.deleted_tasks {
            obj.set("deletedTasks", J::Arr(deleted));
        }
        obj.set("tasks", J::Arr(self.tasks));
        Ok(J::from(obj))
    }

    pub(crate) fn into_projects_json(self, probe: &RootProbe) -> Result<J, StoreError> {
        Ok(J::Arr(with_root_statuses(self.projects, probe)?))
    }
}
