//! Read-only connections to `tasks-index.sqlite` (spec phase-3c-store.md section 4).

use std::path::PathBuf;
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use rusqlite::{Connection, OpenFlags};

use crate::{schema, StoreError};

/// Upper bound on idle cached connections.
const POOL_LIMIT: usize = 4;
/// A WAL reader only waits on recovery or a checkpoint restart, never on TS write transactions.
const BUSY_TIMEOUT: Duration = Duration::from_millis(2000);

/// `{dataBaseDir}/.zcode/v2/tasks-index.sqlite` with `getDataBaseDir()`'s precedence
/// (`ZCODE_DATA_BASE_DIR` > `HOME` > OS home); `None` when no base directory is known.
#[must_use]
pub fn default_db_path() -> Option<PathBuf> {
    let from_env = |name: &str| {
        std::env::var(name)
            .ok()
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty())
    };
    let os_home = || {
        if cfg!(windows) {
            from_env("USERPROFILE")
        } else {
            None
        }
    };
    let base = from_env("ZCODE_DATA_BASE_DIR")
        .or_else(|| from_env("HOME"))
        .or_else(os_home)?;
    Some(
        PathBuf::from(base)
            .join(".zcode")
            .join("v2")
            .join("tasks-index.sqlite"),
    )
}

struct Cached {
    path: PathBuf,
    conn: Connection,
    /// `(data_version, schema_version)` at the last successful schema check.
    checked: Option<(i64, i64)>,
}

/// Pool of read-only connections.
#[derive(Default)]
pub struct Pool {
    idle: Mutex<Vec<Cached>>,
}

fn open(path: &PathBuf) -> Result<Connection, StoreError> {
    if !path.is_file() {
        return Err(StoreError::fallback("database file missing"));
    }
    // 只读打开：不创建 -wal/-shm、关闭时不做 checkpoint，保证 TS 是唯一写者（规格 S1）。
    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    conn.busy_timeout(BUSY_TIMEOUT)?;
    conn.pragma_update(None, "query_only", true)?;
    Ok(conn)
}

fn versions(conn: &Connection) -> Result<(i64, i64), StoreError> {
    let data: i64 = conn.query_row("PRAGMA data_version", [], |row| row.get(0))?;
    let schema: i64 = conn.query_row("PRAGMA schema_version", [], |row| row.get(0))?;
    Ok((data, schema))
}

impl Pool {
    /// Runs `body` inside one read transaction (a fresh snapshot that includes every commit made
    /// before it started) after the schema gate. Any error drops the connection.
    pub fn read<T>(
        &self,
        path: &PathBuf,
        body: impl FnOnce(&Connection) -> Result<T, StoreError>,
    ) -> Result<T, StoreError> {
        let reused = {
            let mut idle = self.idle.lock().unwrap_or_else(PoisonError::into_inner);
            idle.iter()
                .position(|cached| &cached.path == path)
                .map(|index| idle.swap_remove(index))
        };
        let mut cached = match reused {
            Some(cached) => cached,
            None => Cached {
                path: path.clone(),
                conn: open(path)?,
                checked: None,
            },
        };
        let result = Self::run(&mut cached, body);
        if result.is_ok() {
            let mut idle = self.idle.lock().unwrap_or_else(PoisonError::into_inner);
            if idle.len() < POOL_LIMIT {
                idle.push(cached);
            }
        }
        result
    }

    fn run<T>(
        cached: &mut Cached,
        body: impl FnOnce(&Connection) -> Result<T, StoreError>,
    ) -> Result<T, StoreError> {
        let conn = &cached.conn;
        conn.execute_batch("BEGIN")?;
        let outcome = (|| {
            // data_version 只在其他连接提交后变化：未变化时账本与修复状态不可能改变，跳过重复检查。
            let now = versions(conn)?;
            if cached.checked != Some(now) {
                schema::check(conn)?;
            }
            let value = body(conn)?;
            Ok::<_, StoreError>((value, now))
        })();
        let end = conn.execute_batch("COMMIT");
        let (value, now) = outcome?;
        end?;
        cached.checked = Some(now);
        Ok(value)
    }
}
