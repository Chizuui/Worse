//! Crate tests against a small WAL database that a separate (test-only) writer connection edits,
//! standing in for the TS Host.

use std::path::PathBuf;

use rusqlite::Connection;
use serde_json::{json, Value};

use crate::schema::{PROJECT_LEDGER, TASKS_LEDGER};
use crate::StoreService;

const SCHEMA: &str = "
CREATE TABLE schema_migration (id text primary key, checksum text not null, app_version text, applied_at integer not null);
CREATE TABLE tasks_schema_migration (id text primary key, checksum text not null, time_applied integer not null);
CREATE TABLE hosts (host_id TEXT PRIMARY KEY, kind TEXT NOT NULL, position INTEGER NOT NULL DEFAULT 0);
CREATE TABLE projects (project_id TEXT PRIMARY KEY, host_id TEXT NOT NULL, name TEXT NOT NULL,
 name_overridden INTEGER NOT NULL DEFAULT 0, position INTEGER NOT NULL, pinned INTEGER NOT NULL DEFAULT 0,
 pinned_at INTEGER, deleted INTEGER NOT NULL DEFAULT 0, deleted_at INTEGER, created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL);
CREATE TABLE project_roots (project_id TEXT NOT NULL, root_path TEXT NOT NULL, position INTEGER NOT NULL,
 is_primary INTEGER NOT NULL DEFAULT 0, PRIMARY KEY(project_id,root_path));
CREATE TABLE tasks (workspace_key TEXT NOT NULL, workspace_path TEXT NOT NULL, workspace_identity TEXT, task_id TEXT NOT NULL,
 title TEXT NOT NULL DEFAULT '', provider TEXT, unread_at INTEGER, pinned INTEGER NOT NULL DEFAULT 0, archived INTEGER NOT NULL DEFAULT 0,
 deleted INTEGER NOT NULL DEFAULT 0, title_overridden INTEGER NOT NULL DEFAULT 0, meta_json TEXT NOT NULL DEFAULT '{}',
 host_id TEXT, root_path TEXT, project_id TEXT, cli_title TEXT, cli_title_source TEXT, cli_created_at INTEGER,
 cli_updated_at INTEGER, cli_last_outcome TEXT, cli_deleted INTEGER, PRIMARY KEY(workspace_key, task_id));
CREATE TABLE task_groups (group_id TEXT PRIMARY KEY, title TEXT NOT NULL, color TEXT, created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL);
CREATE TABLE task_group_members (group_id TEXT NOT NULL, workspace_key TEXT NOT NULL, task_id TEXT NOT NULL,
 sort_order INTEGER, added_at INTEGER NOT NULL);
CREATE TABLE task_group_view_node_orders (node_type TEXT NOT NULL, node_key TEXT NOT NULL, sort_order INTEGER NOT NULL,
 created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL);
";

fn fixture() -> (tempfile::TempDir, PathBuf, Connection) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("tasks-index.sqlite");
    let writer = Connection::open(&path).unwrap();
    writer.pragma_update(None, "journal_mode", "WAL").unwrap();
    writer.execute_batch(SCHEMA).unwrap();
    for (id, sum) in PROJECT_LEDGER {
        writer
            .execute("INSERT INTO schema_migration VALUES(?,?,'x',0)", [id, sum])
            .unwrap();
    }
    for (id, sum) in TASKS_LEDGER {
        writer
            .execute(
                "INSERT INTO tasks_schema_migration VALUES(?,?,0)",
                [id, sum],
            )
            .unwrap();
    }
    let root = dir.path().to_string_lossy().into_owned();
    writer
        .execute_batch(&format!(
            "INSERT INTO projects VALUES('p1','local','One',0,0,1,NULL,0,NULL,10,20);
             INSERT INTO projects VALUES('p2','local','Gone',1,1,0,NULL,1,5,10,20);
             INSERT INTO projects VALUES('p3','remote:x','Remote',0,2,0,NULL,0,NULL,10,20);
             INSERT INTO project_roots VALUES('p1','{root}',0,1);
             INSERT INTO tasks(workspace_key,workspace_path,task_id,provider,meta_json,host_id,root_path,project_id,cli_title,cli_created_at,cli_updated_at,cli_last_outcome,unread_at)
               VALUES('/w','/w','t1','glm','{{\"zeta\":1,\"title\":\"old\",\"traceId\":\"tr\"}}','local','/w','p1','Hello',1,5,'interrupted',7);
             INSERT INTO tasks(workspace_key,workspace_path,task_id,provider,meta_json,host_id,root_path,cli_created_at,cli_updated_at)
               VALUES('/w','/w','t2','glm','7','local','/w',1,9);
             INSERT INTO tasks(workspace_key,workspace_path,task_id,provider,host_id,root_path,deleted,cli_created_at,cli_updated_at)
               VALUES('/w','/w','t3','glm','local','/w',1,1,9);
             INSERT INTO task_groups VALUES('g1','G',NULL,1,2);"
        ))
        .unwrap();
    (dir, path, writer)
}

fn parse(text: &str) -> Value {
    serde_json::from_str(text).unwrap()
}

#[test]
fn read_tree_matches_ts_shape_and_order() {
    let (_dir, path, _writer) = fixture();
    let store = StoreService::default();
    let text = store
        .read_tree(
            &json!({"includeDeleted": true, "includeDeletedTasks": 1}),
            Some(&path),
        )
        .unwrap();
    // 键顺序即 TS 对象属性顺序：projects、deletedTasks、tasks；meta 原有键保持在前。
    assert!(text.starts_with("{\"projects\":[{\"id\":\"p1\",\"hostId\":\"local\""));
    assert!(text.contains(
        "\"deletedTasks\":[{\"taskId\":\"t3\",\"workspacePath\":\"/w\",\"hostId\":\"local\"}]"
    ));
    assert!(text.contains("{\"zeta\":1,\"title\":\"Hello\",\"traceId\":\"tr\",\"taskId\":\"t1\""));
    let value = parse(&text);
    let projects = value["projects"].as_array().unwrap();
    assert_eq!(
        projects.len(),
        2,
        "remote-host project filtered, deleted kept"
    );
    assert_eq!(projects[0]["primaryRootStatus"], "available");
    assert!(
        projects[1].get("primaryRootStatus").is_none(),
        "deleted project is not probed"
    );
    let tasks = value["tasks"].as_array().unwrap();
    assert_eq!(tasks[0]["taskId"], "t2", "ordered by cli_updated_at DESC");
    assert_eq!(tasks[0]["mode"], "default");
    assert!(tasks[0].get("status").is_none());
    assert!(tasks[0].get("unreadAt").is_none());
    assert_eq!(tasks[0]["projectId"], Value::Null);
    assert_eq!(tasks[0]["traceId"].as_str().unwrap().len(), 36);
    assert_eq!(tasks[1]["status"], "completed");
    assert_eq!(tasks[1]["unreadAt"], 7);
    assert!(tasks[1].get("workspaceIdentity").is_none());
}

#[test]
fn read_projects_and_groups() {
    let (_dir, path, _writer) = fixture();
    let store = StoreService::default();
    let projects = parse(&store.read_projects(&json!({}), Some(&path)).unwrap());
    assert_eq!(projects.as_array().unwrap().len(), 1);
    let groups = store.read_groups(Some(&path)).unwrap();
    assert_eq!(
        groups,
        "{\"groups\":[{\"id\":\"g1\",\"title\":\"G\",\"color\":\"gray\",\"createdAt\":1,\"updatedAt\":2}],\"members\":[],\"order\":[]}"
    );
}

#[test]
fn writer_commits_are_visible_immediately() {
    let (_dir, path, writer) = fixture();
    let store = StoreService::default();
    let before = parse(&store.read_groups(Some(&path)).unwrap());
    assert_eq!(before["groups"].as_array().unwrap().len(), 1);
    writer
        .execute("INSERT INTO task_groups VALUES('g2','H','red',3,4)", [])
        .unwrap();
    let after = parse(&store.read_groups(Some(&path)).unwrap());
    assert_eq!(after["groups"].as_array().unwrap().len(), 2);
}

#[test]
fn unknown_schema_and_remote_host_fall_back() {
    let (_dir, path, writer) = fixture();
    let store = StoreService::default();
    assert!(store.read_groups(Some(&path)).is_ok());
    writer
        .execute(
            "INSERT INTO schema_migration VALUES('0010_future','abc','9.9.9',0)",
            [],
        )
        .unwrap();
    let error = store.read_groups(Some(&path)).unwrap_err().to_string();
    assert!(error.contains("schema_migration ledger"), "{error}");
    assert!(store
        .read_tree(&json!({"hostId": "remote:server:x"}), Some(&path))
        .is_err());
    writer
        .execute("DELETE FROM schema_migration WHERE id='0010_future'", [])
        .unwrap();
    writer
        .execute("UPDATE tasks SET meta_json='null' WHERE task_id='t2'", [])
        .unwrap();
    let error = store
        .read_tree(&json!({}), Some(&path))
        .unwrap_err()
        .to_string();
    assert!(error.contains("meta_json is null"), "{error}");
    writer
        .execute(
            "UPDATE tasks SET meta_json='{}', host_id=NULL WHERE task_id='t2'",
            [],
        )
        .unwrap();
    assert!(store.read_tree(&json!({}), Some(&path)).is_err());
}

#[test]
fn never_writes() {
    let (_dir, path, _writer) = fixture();
    let store = StoreService::default();
    store.read_tree(&json!({}), Some(&path)).unwrap();
    let probe =
        Connection::open_with_flags(&path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    probe.pragma_update(None, "query_only", true).unwrap();
    assert!(probe.execute("DELETE FROM task_groups", []).is_err());
}
