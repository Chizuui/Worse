//! Opt-in ignore rules. The TS service reports every path; the Rust hub only
//! filters when the caller passes rules, so the default stays drop-in.

use std::path::Path;

use ignore::gitignore::{Gitignore, GitignoreBuilder};

/// Compiled ignore rules rooted at the watched directory.
#[derive(Debug, Clone)]
pub struct IgnoreRules {
    matcher: Gitignore,
}

impl IgnoreRules {
    /// Builds rules from the root's `.gitignore` (when `respect_gitignore`)
    /// plus gitignore-style `excludes` (for example `node_modules/`, `.git/`).
    ///
    /// # Errors
    /// Returns the glob error text for an invalid exclude pattern.
    pub fn build(root: &str, respect_gitignore: bool, excludes: &[String]) -> Result<Self, String> {
        let root = Path::new(root);
        let mut builder = GitignoreBuilder::new(root);
        if respect_gitignore {
            let file = root.join(".gitignore");
            if file.is_file() {
                if let Some(error) = builder.add(&file) {
                    tracing::debug!(%error, "partial .gitignore for watcher");
                }
            }
        }
        for pattern in excludes {
            builder
                .add_line(None, pattern)
                .map_err(|e| format!("invalid exclude '{pattern}': {e}"))?;
        }
        let matcher = builder.build().map_err(|e| e.to_string())?;
        Ok(Self { matcher })
    }

    /// `rel` uses `/` separators and is relative to the watched root.
    #[must_use]
    pub fn is_ignored(&self, rel: &str, is_dir: bool) -> bool {
        self.matcher
            .matched_path_or_any_parents(Path::new(rel), is_dir)
            .is_ignore()
    }
}
