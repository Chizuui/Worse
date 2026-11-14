//! Workspace-root containment and symlink policy.
//!
//! The `ZCode` TS services accept absolute paths without a root check, so the
//! Host only applies a [`WorkspaceRoot`] when the caller supplies a root.
//! When it does, a path is accepted only if it stays inside the root both
//! lexically and after symlink resolution (unless [`SymlinkPolicy::AllowEscape`]).

use std::path::{Path, PathBuf};

use crate::error::FsError;
use crate::path_identity::{is_within, normalize_lexical, strip_verbatim, CaseSensitivity};

/// What to do with a symlink whose target lies outside the workspace root.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SymlinkPolicy {
    /// Reject paths that resolve outside the root (default).
    #[default]
    DenyEscape,
    /// Accept symlink targets outside the root; lexical containment still applies.
    AllowEscape,
}

/// A canonical workspace root with a containment policy.
#[derive(Debug, Clone)]
pub struct WorkspaceRoot {
    root: PathBuf,
    canonical: PathBuf,
    policy: SymlinkPolicy,
    case: CaseSensitivity,
}

impl WorkspaceRoot {
    /// Opens `root`, which must be an existing directory.
    ///
    /// # Errors
    /// Fails when the root does not exist or is not a directory.
    pub fn new(root: &Path, policy: SymlinkPolicy) -> Result<Self, FsError> {
        let canonical = canonicalize(root)?;
        if !canonical.is_dir() {
            return Err(FsError::app(format!(
                "Workspace root is not a directory: {}",
                root.display()
            )));
        }
        Ok(Self {
            root: normalize_lexical(root),
            canonical,
            policy,
            case: CaseSensitivity::platform_default(),
        })
    }

    /// Overrides the case sensitivity used for containment checks.
    #[must_use]
    pub fn with_case(mut self, case: CaseSensitivity) -> Self {
        self.case = case;
        self
    }

    /// The root as given (lexically normalised).
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.root
    }

    /// The canonical (symlink-free, non-verbatim) root.
    #[must_use]
    pub fn canonical(&self) -> &Path {
        &self.canonical
    }

    /// Resolves `path` (absolute, or relative to the root) and enforces the
    /// policy. The path itself need not exist: its deepest existing ancestor is
    /// canonicalised so that writes to new files are checked too.
    ///
    /// # Errors
    /// `OutsideRoot` when the path escapes, or an I/O error while resolving.
    pub fn resolve(&self, path: &Path) -> Result<PathBuf, FsError> {
        let joined = if path.is_absolute() {
            normalize_lexical(path)
        } else {
            normalize_lexical(&self.root.join(path))
        };
        let lexically_inside = is_within(&self.root, &joined, self.case)
            || is_within(&self.canonical, &joined, self.case);
        if !lexically_inside {
            return Err(outside(path));
        }
        if self.policy == SymlinkPolicy::AllowEscape {
            return Ok(joined);
        }
        let real = canonicalize_existing_prefix(&joined)?;
        if is_within(&self.canonical, &real, self.case) {
            Ok(joined)
        } else {
            tracing::debug!(path = %path.display(), real = %real.display(), "symlink escapes workspace root");
            Err(outside(path))
        }
    }
}

fn outside(path: &Path) -> FsError {
    FsError::app(format!(
        "Path is outside the workspace root: {}",
        path.display()
    ))
}

/// `std::fs::canonicalize` without the Windows verbatim prefix.
pub(crate) fn canonicalize(path: &Path) -> Result<PathBuf, FsError> {
    let real = std::fs::canonicalize(path).map_err(|e| FsError::from_io(&e, "realpath", path))?;
    Ok(PathBuf::from(
        strip_verbatim(&real.to_string_lossy()).into_owned(),
    ))
}

/// Canonicalises the deepest existing ancestor and re-appends the rest.
fn canonicalize_existing_prefix(path: &Path) -> Result<PathBuf, FsError> {
    let mut existing = path.to_path_buf();
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    loop {
        if std::fs::symlink_metadata(&existing).is_ok() {
            break;
        }
        match (existing.file_name(), existing.parent()) {
            (Some(name), Some(parent)) => {
                tail.push(name.to_os_string());
                existing = parent.to_path_buf();
            }
            _ => return Ok(path.to_path_buf()),
        }
    }
    let mut real = canonicalize(&existing)?;
    for name in tail.iter().rev() {
        real.push(name);
    }
    Ok(real)
}
