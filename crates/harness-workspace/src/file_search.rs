//! [`FileSearchSource`] for the hub's `@` mentions and command center (phase-9-parity.md §9.2.1).
//!
//! Keeps at most one opened [`Workspace`] (the last searched root). Opening is cheap and the file index is built on
//! the first search only, so an idle app pays nothing.

use std::path::PathBuf;
use std::sync::Mutex;

use harness_core_api::workspace::{FileHit, WorkspaceServices};
use harness_core_api::{ApiResult, BoxFuture, FileSearchSource};

use crate::{lock, Workspace, WorkspaceConfig};

/// File-name search over the workspace index, one cached root.
#[derive(Default)]
pub struct WorkspaceFileSearch {
    cached: Mutex<Option<(String, Workspace)>>,
}

impl WorkspaceFileSearch {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    async fn workspace(&self, root: &str) -> ApiResult<Workspace> {
        if let Some((path, ws)) = lock(&self.cached).as_ref() {
            if path == root {
                return Ok(ws.clone());
            }
        }
        let ws = Workspace::open(PathBuf::from(root), WorkspaceConfig::default()).await?;
        *lock(&self.cached) = Some((root.to_owned(), ws.clone()));
        Ok(ws)
    }
}

impl FileSearchSource for WorkspaceFileSearch {
    fn search(
        &self,
        workspace_path: String,
        query: String,
        limit: u32,
    ) -> BoxFuture<'_, ApiResult<Vec<FileHit>>> {
        Box::pin(async move {
            let ws = self.workspace(&workspace_path).await?;
            ws.search_files(query, limit).await
        })
    }
}
