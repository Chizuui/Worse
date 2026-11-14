//! Canonical workspace identity key.

use core::fmt;

use serde::{Deserialize, Serialize};

use crate::CoreError;

/// Key used for workspace identity, isolation, and deduplication.
///
/// The product invariant is `workspaceIdentity?.trim() || workspacePath`: a
/// non-empty trimmed identity wins, otherwise the path is used verbatim. The
/// path is kept untrimmed because it addresses real files on disk.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct WorkspaceKey(String);

impl WorkspaceKey {
    /// Resolves the canonical workspace key for one workspace target.
    ///
    /// A `Some` identity that is empty or whitespace-only is ignored so the
    /// caller falls back to the path.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::EmptyWorkspaceKey`] when no trimmed identity is
    /// available and the path is empty or whitespace-only.
    pub fn resolve(identity: Option<&str>, path: &str) -> Result<Self, CoreError> {
        match identity.map(str::trim) {
            Some(identity) if !identity.is_empty() => Ok(Self(identity.to_owned())),
            _ if path.trim().is_empty() => Err(CoreError::EmptyWorkspaceKey),
            _ => Ok(Self(path.to_owned())),
        }
    }

    /// Returns the key as a string slice.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for WorkspaceKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::WorkspaceKey;
    use crate::CoreError;

    #[test]
    fn trims_surrounding_identity_whitespace() {
        let key = WorkspaceKey::resolve(Some("  workspace-1  "), "/repo/ignored").unwrap();
        assert_eq!(key.as_str(), "workspace-1");
        assert_eq!(key.to_string(), "workspace-1");
    }

    #[test]
    fn identity_wins_over_path() {
        let key = WorkspaceKey::resolve(Some("remote:session-9"), "/local/path").unwrap();
        assert_eq!(key.as_str(), "remote:session-9");
    }

    #[test]
    fn whitespace_only_identity_falls_back_to_path() {
        let key = WorkspaceKey::resolve(Some("   "), "/repo/project").unwrap();
        assert_eq!(key.as_str(), "/repo/project");
    }

    #[test]
    fn empty_identity_falls_back_to_path() {
        let key = WorkspaceKey::resolve(Some(""), "/repo/project").unwrap();
        assert_eq!(key.as_str(), "/repo/project");
    }

    #[test]
    fn missing_identity_uses_path() {
        let key = WorkspaceKey::resolve(None, "C:\\work\\project").unwrap();
        assert_eq!(key.as_str(), "C:\\work\\project");
    }

    #[test]
    fn keeps_path_verbatim_when_used_as_fallback() {
        let key = WorkspaceKey::resolve(None, " /tmp/repo ").unwrap();
        assert_eq!(key.as_str(), " /tmp/repo ");
    }

    #[test]
    fn rejects_empty_identity_and_empty_path() {
        assert_eq!(
            WorkspaceKey::resolve(None, ""),
            Err(CoreError::EmptyWorkspaceKey)
        );
        assert_eq!(
            WorkspaceKey::resolve(Some(""), ""),
            Err(CoreError::EmptyWorkspaceKey)
        );
        assert_eq!(
            WorkspaceKey::resolve(Some("   "), ""),
            Err(CoreError::EmptyWorkspaceKey)
        );
    }

    #[test]
    fn rejects_whitespace_only_path_without_identity() {
        assert_eq!(
            WorkspaceKey::resolve(None, "   "),
            Err(CoreError::EmptyWorkspaceKey)
        );
        assert_eq!(
            WorkspaceKey::resolve(Some("\u{3000}"), " \t\r\n"),
            Err(CoreError::EmptyWorkspaceKey)
        );
    }

    #[test]
    fn trims_unicode_whitespace_around_unicode_identity() {
        let key = WorkspaceKey::resolve(Some("\u{3000} 工作区/α \u{00A0}"), "/ignored").unwrap();
        assert_eq!(key.as_str(), "工作区/α");
    }

    #[test]
    fn preserves_unicode_path_fallback() {
        let key = WorkspaceKey::resolve(None, "C:\\项目\\café").unwrap();
        assert_eq!(key.as_str(), "C:\\项目\\café");
    }

    #[test]
    fn whitespace_only_unicode_identity_uses_unicode_path() {
        let key = WorkspaceKey::resolve(Some("\u{3000}\u{00A0}"), "/рабочая/область").unwrap();
        assert_eq!(key.as_str(), "/рабочая/область");
    }
}
