//! Path identity: comparable keys for paths that may differ in spelling.
//!
//! Two spellings name the same workspace when their identity keys are equal:
//! verbatim prefixes (`\\?\C:\`, `\\?\UNC\`) are stripped, separators are
//! unified, `.`/`..` are resolved lexically, trailing separators are dropped
//! and, on case-insensitive platforms (Windows, macOS), the key is lowercased.
//! Identity keys never touch the disk; combine with [`crate::realpath`] when
//! symlink aliases must collapse too.

use std::borrow::Cow;
use std::path::{Component, Path, PathBuf};

/// How path components are compared.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaseSensitivity {
    /// Byte-exact comparison (Linux and most Unix file systems).
    Sensitive,
    /// Unicode lowercase comparison (default on Windows and macOS).
    Insensitive,
}

impl CaseSensitivity {
    /// The default for the current platform.
    #[must_use]
    pub const fn platform_default() -> Self {
        if cfg!(any(windows, target_os = "macos", target_os = "ios")) {
            Self::Insensitive
        } else {
            Self::Sensitive
        }
    }
}

/// Removes Windows verbatim prefixes: `\\?\C:\x` → `C:\x`,
/// `\\?\UNC\server\share` → `\\server\share`, `\??\C:\x` → `C:\x`.
/// Verbatim paths without a drive or UNC form (e.g. `\\?\Volume{..}`) are
/// returned unchanged because they have no non-verbatim spelling.
#[must_use]
pub fn strip_verbatim(path: &str) -> Cow<'_, str> {
    for prefix in [r"\\?\", r"\??\", "//?/"] {
        if let Some(rest) = path.strip_prefix(prefix) {
            if let Some(unc) = rest
                .strip_prefix("UNC\\")
                .or_else(|| rest.strip_prefix("UNC/"))
            {
                return Cow::Owned(format!(r"\\{unc}"));
            }
            let bytes = rest.as_bytes();
            if bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' {
                return Cow::Borrowed(rest);
            }
        }
    }
    Cow::Borrowed(path)
}

/// Resolves `.` and `..` lexically without touching the disk. `..` never
/// climbs above the root.
#[must_use]
pub fn normalize_lexical(path: &Path) -> PathBuf {
    let stripped = strip_verbatim(&path.to_string_lossy()).into_owned();
    let mut out = PathBuf::new();
    for component in Path::new(&stripped).components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                let can_pop = matches!(out.components().next_back(), Some(Component::Normal(_)));
                if can_pop {
                    out.pop();
                } else if !out.has_root() {
                    out.push("..");
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Comparable identity key for `path` under `case`.
#[must_use]
pub fn identity_key(path: &Path, case: CaseSensitivity) -> String {
    let normalized = normalize_lexical(path);
    let mut key = normalized.to_string_lossy().replace('\\', "/");
    while key.len() > 1 && key.ends_with('/') && !key.ends_with(":/") {
        key.pop();
    }
    match case {
        CaseSensitivity::Sensitive => key,
        CaseSensitivity::Insensitive => key.to_lowercase(),
    }
}

/// Whether `path` equals `root` or lies below it, compared by identity key.
#[must_use]
pub fn is_within(root: &Path, path: &Path, case: CaseSensitivity) -> bool {
    let root_key = identity_key(root, case);
    let path_key = identity_key(path, case);
    if path_key == root_key {
        return true;
    }
    let prefix = if root_key.ends_with('/') {
        root_key
    } else {
        format!("{root_key}/")
    };
    path_key.starts_with(&prefix)
}
