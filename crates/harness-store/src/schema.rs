//! Schema gate: Rust only reads a database whose migration ledgers it knows exactly
//! (spec phase-3c-store.md section 4).

use rusqlite::Connection;

use crate::StoreError;

/// `schema_migration` (project storage, `packages/services/src/project-storage/index.ts`).
pub const PROJECT_LEDGER: &[(&str, &str)] = &[
    (
        "0005_projects",
        "ac00a61c7c45a5ceb3814b1487901372c5580eb6eb8d31bde5bcdfbd1ed0f0da",
    ),
    (
        "0006_default_workspace_tasks",
        "688bfe3cb7548ffb5ccbade2f5d71c05b14452a2aa32e5ace2e60f313680eaa4",
    ),
    (
        "0007_initial_workspace_order",
        "cac45e440b4b54f588d7611bd1512232e18dda3c87144949568e45251edbcf1b",
    ),
    (
        "0008_remote_entry_deleted_cache",
        "ecb3ae060f6fe3b2e8e40da94055c2b053ef6a0434f3afc4b433c0111c4bedea",
    ),
    (
        "0009_session_creation_commands",
        "961b30b1391ea81139ffda2dc8e42ef1ddc5a5fcc46b20de44b62f53ee5d09fb",
    ),
];

/// `tasks_schema_migration` (`packages/services/src/session/tasksDatabase/migrations.ts`).
pub const TASKS_LEDGER: &[(&str, &str)] = &[
    (
        "0001_adopt_task_schema",
        "3e8337b015d94b05dd31a6003f3acc649e821794cfa288bc0af3022698bd4d17",
    ),
    (
        "0002_provider_selection",
        "7244ef7c351f8d02750ab1953fff09f493a71befbf1b6e2d4bab726b0c6b48fc",
    ),
    (
        "0003_official_glm_selection",
        "8987adb50ae412a46c294141c1af89ccfc252f22d41351bdf4c7528f56edc8b4",
    ),
    (
        "0004_project_execution_scopes",
        "caae5e91fc3abfeee6508f782890ad8d67cebeb80611775d39dc13a7e6fea3f0",
    ),
    (
        "0006_task_group_admissions",
        "0af559d04bb3f1a1dab12d6533d59292d232b09ea3ef3ef6ca3c9373bd663019",
    ),
];

fn table_exists(conn: &Connection, name: &str) -> Result<bool, StoreError> {
    let mut stmt =
        conn.prepare_cached("SELECT 1 FROM sqlite_master WHERE type='table' AND name=?")?;
    Ok(stmt.exists([name])?)
}

fn check_ledger(
    conn: &Connection,
    table: &str,
    expected: &[(&str, &str)],
) -> Result<(), StoreError> {
    if !table_exists(conn, table)? {
        return Err(StoreError::fallback(format!("schema: {table} missing")));
    }
    let mut stmt = conn.prepare_cached(&format!("SELECT id, checksum FROM {table} ORDER BY id"))?;
    let rows = stmt
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    let mut wanted: Vec<(&str, &str)> = expected.to_vec();
    wanted.sort_unstable();
    let same = rows.len() == wanted.len()
        && rows
            .iter()
            .zip(&wanted)
            .all(|((id, sum), (want_id, want_sum))| id == want_id && sum == want_sum);
    if same {
        Ok(())
    } else {
        Err(StoreError::fallback(format!(
            "schema: {table} ledger differs from the known version"
        )))
    }
}

/// Ledger + readiness check; runs inside the caller's read transaction.
pub fn check(conn: &Connection) -> Result<(), StoreError> {
    check_ledger(conn, "schema_migration", PROJECT_LEDGER)?;
    check_ledger(conn, "tasks_schema_migration", TASKS_LEDGER)?;
    // TS 启动时才补齐旧版本写入的 host_id 为 NULL 的行、并迁移 acp_session_id 旧 ID；
    // 未完成这些修复前 Rust 读到的是 TS 读不到的中间态，必须回退。
    let mut columns = conn.prepare_cached("SELECT name FROM pragma_table_info('tasks')")?;
    let names = columns
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    if names.iter().any(|name| name == "acp_session_id") {
        return Err(StoreError::fallback("schema: legacy acp_session_id column"));
    }
    let mut legacy = conn.prepare_cached("SELECT 1 FROM tasks WHERE host_id IS NULL LIMIT 1")?;
    if legacy.exists([])? {
        return Err(StoreError::fallback(
            "schema: tasks not reconciled (host_id IS NULL)",
        ));
    }
    Ok(())
}
