//! Harness task-index store: read-only port of the Host's `tasks-index.sqlite` reads
//! (`project.readTree`, `project.readProjects`, `task-groups.read`).
//!
//! Spec: `Worse/docs/specs/harness/phase-3c-store.md`. TS stays the only writer; every
//! condition this crate does not reproduce exactly is a [`StoreError::Fallback`], which the
//! bridge answers by replaying the request on the TS Host.
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

mod db;
mod groups;
mod identity;
mod json;
mod project;
mod roots;
mod schema;
#[cfg(test)]
mod tests;

use std::path::PathBuf;

use serde_json::Value;

use project::TreeOptions;
pub use project::OWNER_HOST_ID;

/// Why Rust did not answer.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// TS must answer this request.
    #[error("store fallback: {0}")]
    Fallback(String),
}

impl StoreError {
    /// Fallback with a reason (logged by the bridge trace).
    #[must_use]
    pub fn fallback(reason: impl Into<String>) -> Self {
        Self::Fallback(reason.into())
    }
}

impl From<rusqlite::Error> for StoreError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Fallback(format!("sqlite: {error}"))
    }
}

/// Process-wide store state.
#[derive(Default)]
pub struct StoreService {
    pool: db::Pool,
    roots: roots::RootProbe,
}

fn db_path(explicit: Option<&PathBuf>) -> Result<PathBuf, StoreError> {
    explicit
        .cloned()
        .or_else(db::default_db_path)
        .ok_or_else(|| StoreError::fallback("no data base directory"))
}

/// `route(hostId)`: falsy or the owner Host is served locally; anything else goes remote (TS).
fn require_local_host(params: &Value) -> Result<(), StoreError> {
    let host = params.get("hostId");
    if !json::param_truthy(host) || host.and_then(Value::as_str) == Some(OWNER_HOST_ID) {
        Ok(())
    } else {
        Err(StoreError::fallback("hostId names another Host"))
    }
}

impl StoreService {
    /// `project.readTree(params)` as ordered JSON text.
    ///
    /// # Errors
    /// [`StoreError::Fallback`] whenever TS must answer instead.
    pub fn read_tree(&self, params: &Value, path: Option<&PathBuf>) -> Result<String, StoreError> {
        require_local_host(params)?;
        let options = TreeOptions {
            include_deleted: json::param_truthy(params.get("includeDeleted")),
            include_deleted_tasks: json::param_truthy(params.get("includeDeletedTasks")),
            include_tasks: true,
        };
        let tree = self
            .pool
            .read(&db_path(path)?, |conn| project::read_tree(conn, options))?;
        Ok(tree.into_json(&self.roots)?.to_text())
    }

    /// `project.readProjects(params)` as ordered JSON text.
    ///
    /// # Errors
    /// [`StoreError::Fallback`] whenever TS must answer instead.
    pub fn read_projects(
        &self,
        params: &Value,
        path: Option<&PathBuf>,
    ) -> Result<String, StoreError> {
        require_local_host(params)?;
        let options = TreeOptions {
            include_deleted: json::param_truthy(params.get("includeDeleted")),
            include_deleted_tasks: false,
            include_tasks: false,
        };
        let tree = self
            .pool
            .read(&db_path(path)?, |conn| project::read_tree(conn, options))?;
        Ok(tree.into_projects_json(&self.roots)?.to_text())
    }

    /// `task-groups.read()` as ordered JSON text.
    ///
    /// # Errors
    /// [`StoreError::Fallback`] whenever TS must answer instead.
    pub fn read_groups(&self, path: Option<&PathBuf>) -> Result<String, StoreError> {
        Ok(self.pool.read(&db_path(path)?, groups::read)?.to_text())
    }
}
