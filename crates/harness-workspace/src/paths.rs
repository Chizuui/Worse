//! Workspace root and path containment (spec §15.3: paths outside the root are rejected).

use std::path::{Path, PathBuf};

use harness_core_api::{ApiError, ApiResult};
use harness_fs::{SymlinkPolicy, WorkspaceRoot};

/// The canonical root of a workspace.
#[derive(Debug, Clone)]
pub(crate) struct Root {
    root: WorkspaceRoot,
    text: String,
}

impl Root {
    /// Canonicalizes `root` (blocking).
    pub(crate) fn open(root: &Path) -> ApiResult<Self> {
        let root = WorkspaceRoot::new(root, SymlinkPolicy::DenyEscape)
            .map_err(|e| ApiError::InvalidArgument(e.to_string()))?;
        let text = root.canonical().to_string_lossy().into_owned();
        Ok(Self { root, text })
    }

    /// The canonical root as text.
    pub(crate) fn as_str(&self) -> &str {
        &self.text
    }

    /// Resolves a workspace-relative (`/` or `\`) or absolute path inside the root (blocking: symlinks are
    /// checked). Returns the absolute path and the workspace-relative path with `/`.
    pub(crate) fn resolve(&self, path: &str) -> ApiResult<(PathBuf, String)> {
        let trimmed = path.trim();
        if trimmed.is_empty() {
            return Err(ApiError::InvalidArgument("empty path".into()));
        }
        let absolute = self
            .root
            .resolve(Path::new(trimmed))
            .map_err(|e| ApiError::InvalidArgument(e.to_string()))?;
        let relative = self.relative(&absolute.to_string_lossy()).ok_or_else(|| {
            ApiError::InvalidArgument(format!("Path is outside the workspace root: {trimmed}"))
        })?;
        Ok((absolute, relative))
    }

    /// Workspace-relative form (`/` separators) of an absolute path under the root (given or canonical form).
    pub(crate) fn relative(&self, absolute: &str) -> Option<String> {
        let path = Path::new(absolute);
        let stripped = path
            .strip_prefix(self.root.canonical())
            .or_else(|_| path.strip_prefix(self.root.path()))
            .ok()
            .or_else(|| strip_case_insensitive(path, self.root.canonical()))?;
        Some(
            stripped
                .components()
                .map(|c| c.as_os_str().to_string_lossy().into_owned())
                .collect::<Vec<_>>()
                .join("/"),
        )
    }
}

/// Windows paths compare case-insensitively (`c:\` vs `C:\` from different APIs).
fn strip_case_insensitive<'a>(path: &'a Path, root: &Path) -> Option<&'a Path> {
    if !cfg!(windows) {
        return None;
    }
    let text = path.to_str()?;
    let root_text = root.to_str()?;
    if text.len() < root_text.len() || !text.is_char_boundary(root_text.len()) {
        return None;
    }
    let (head, tail) = text.split_at(root_text.len());
    if !head.eq_ignore_ascii_case(root_text) {
        return None;
    }
    let tail = tail.trim_start_matches(['\\', '/']);
    if tail.len() == text.len() - root_text.len() && !tail.is_empty() {
        // No separator between the root and the rest: a sibling such as `C:\work2` for root `C:\work`.
        return None;
    }
    Some(Path::new(&text[text.len() - tail.len()..]))
}
