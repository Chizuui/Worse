//! Environment and path helpers shared by the Host launch resolvers.

use std::path::{Component, Path, PathBuf};

/// Returns the trimmed, non-empty value of `key`.
pub(super) fn env_value(key: &str) -> Option<String> {
    let value = std::env::var(key).ok()?;
    let trimmed = value.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_owned())
}

/// Returns `key` as a path when it is set and non-empty.
pub(super) fn env_path(key: &str) -> Option<PathBuf> {
    env_value(key).map(PathBuf::from)
}

/// Resolves `path` against the current directory and normalizes it.
pub(super) fn absolute(path: &Path) -> PathBuf {
    let joined = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().map_or_else(|_error| path.to_path_buf(), |cwd| cwd.join(path))
    };
    normalize(&joined)
}

/// Lexically removes `.` and `..` so paths handed to Node are canonical-looking.
pub(super) fn normalize(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    normalized
}

/// Strips the Windows verbatim prefix from a drive path (`\\?\C:\x` -> `C:\x`).
///
/// 修复原因：Tauri 的 `resource_dir()` 返回规范化的 `\\?\` 路径；把它作为 Node 的 cwd
/// 或入口传下去时，Host 再启动的 cmd.exe 等子进程不接受这种 cwd。只处理普通盘符路径，
/// UNC 形式（`\\?\UNC\...`）原样保留。
pub(super) fn simplify_verbatim(path: PathBuf) -> PathBuf {
    let simplified = path
        .to_str()
        .and_then(|text| text.strip_prefix(r"\\?\"))
        .filter(|rest| rest.as_bytes().get(1) == Some(&b':'))
        .map(PathBuf::from);
    simplified.unwrap_or(path)
}

/// Lossy UTF-8 rendering used for JSON payloads and environment values.
pub(super) fn path_string(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::{normalize, simplify_verbatim};

    #[test]
    fn verbatim_drive_paths_are_simplified() {
        assert_eq!(
            simplify_verbatim(PathBuf::from(r"\\?\C:\Program Files\Harness")),
            PathBuf::from(r"C:\Program Files\Harness")
        );
        let unc = PathBuf::from(r"\\?\UNC\server\share");
        assert_eq!(simplify_verbatim(unc.clone()), unc);
        assert_eq!(
            simplify_verbatim(PathBuf::from("/opt/x")),
            PathBuf::from("/opt/x")
        );
    }

    #[test]
    fn normalize_strips_parent_segments() {
        let input = Path::new("/repo/Worse/apps/harness-app/src-tauri/../../../..");
        assert_eq!(normalize(input), PathBuf::from("/repo"));
    }
}
