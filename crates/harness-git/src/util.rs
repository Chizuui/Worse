//! JS string and Node `path` semantics needed to reproduce the TS results
//! byte for byte (`config.ts` helpers, `path.resolve/relative/isAbsolute`).

use harness_fs::node_normalize;

const SEP: char = if cfg!(windows) { '\\' } else { '/' };

fn is_sep(c: char) -> bool {
    c == '/' || (cfg!(windows) && c == '\\')
}

/// JS `String.prototype.trim` (`WhiteSpace` + `LineTerminator`).
#[must_use]
pub fn js_trim(s: &str) -> &str {
    s.trim_matches(|c: char| c == '\u{feff}' || (c.is_whitespace() && c != '\u{85}'))
}

/// JS `Number.parseInt(value, 10)`; `None` for `NaN`.
#[must_use]
pub fn js_parse_int(value: &str) -> Option<i64> {
    let s = js_trim(value);
    let (negative, digits) = match s.as_bytes().first() {
        Some(b'-') => (true, &s[1..]),
        Some(b'+') => (false, &s[1..]),
        _ => (false, s),
    };
    let end = digits
        .bytes()
        .position(|b| !b.is_ascii_digit())
        .unwrap_or(digits.len());
    if end == 0 {
        return None;
    }
    let mut n: i64 = 0;
    for b in digits[..end].bytes() {
        n = n.saturating_mul(10).saturating_add(i64::from(b - b'0'));
    }
    Some(if negative { -n } else { n })
}

/// `normalizeGitPath`: backslashes to `/`.
#[must_use]
pub fn normalize_git_path(path: &str) -> String {
    path.replace('\\', "/")
}

fn strip_dot_slash(path: &str) -> &str {
    path.strip_prefix("./")
        .or_else(|| path.strip_prefix('/'))
        .unwrap_or(path)
}

/// `normalizeWorkspaceInRepoPath`.
#[must_use]
pub fn normalize_workspace_in_repo_path(path: &str) -> String {
    let normalized = normalize_git_path(path);
    let normalized = strip_dot_slash(&normalized).trim_end_matches('/');
    if normalized.is_empty() {
        ".".to_owned()
    } else {
        normalized.to_owned()
    }
}

/// `isPathInWorkspaceScope`.
#[must_use]
pub fn is_path_in_workspace_scope(repo_relative_path: &str, workspace_in_repo_path: &str) -> bool {
    let path = normalize_git_path(repo_relative_path);
    let path = strip_dot_slash(&path);
    let workspace = normalize_workspace_in_repo_path(workspace_in_repo_path);
    if workspace == "." {
        return true;
    }
    path == workspace
        || path
            .strip_prefix(workspace.as_str())
            .is_some_and(|rest| rest.starts_with('/'))
}

/// `toWorkspaceRelativeGitPath`.
#[must_use]
pub fn to_workspace_relative_git_path(
    repo_relative_path: &str,
    workspace_in_repo_path: &str,
) -> String {
    let path = normalize_git_path(repo_relative_path);
    let path = strip_dot_slash(&path);
    let workspace = normalize_workspace_in_repo_path(workspace_in_repo_path);
    if workspace == "." {
        return path.to_owned();
    }
    if path == workspace {
        return ".".to_owned();
    }
    match path.strip_prefix(workspace.as_str()) {
        Some(rest) if rest.starts_with('/') => rest[1..].to_owned(),
        _ => path.to_owned(),
    }
}

/// Node `path.isAbsolute` for the current platform.
#[must_use]
pub fn node_is_absolute(path: &str) -> bool {
    if path.starts_with(is_sep) {
        return true;
    }
    if cfg!(windows) {
        let b = path.as_bytes();
        return b.len() >= 3
            && b[0].is_ascii_alphabetic()
            && b[1] == b':'
            && is_sep(char::from(b[2]));
    }
    false
}

fn strip_trailing_sep(path: String) -> String {
    let is_root = path == "/" || path == "\\" || (path.len() == 3 && path.ends_with(":\\"));
    if is_root {
        return path;
    }
    let trimmed = path.trim_end_matches(SEP);
    if trimmed.is_empty() {
        path
    } else {
        trimmed.to_owned()
    }
}

/// Node `path.resolve(...segments)` (segments resolved right to left until
/// absolute; a relative result is anchored at the process cwd).
#[must_use]
pub fn node_resolve(segments: &[&str]) -> String {
    let mut parts: Vec<&str> = Vec::new();
    let mut absolute = false;
    for segment in segments.iter().rev() {
        if segment.is_empty() {
            continue;
        }
        parts.push(segment);
        if node_is_absolute(segment) {
            absolute = true;
            break;
        }
    }
    parts.reverse();
    let cwd;
    if !absolute {
        cwd = std::env::current_dir()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default();
        parts.insert(0, cwd.as_str());
    }
    let joined = parts.join(&SEP.to_string());
    strip_trailing_sep(node_normalize(&joined))
}

/// Node `path.relative(from, to)` for the current platform (case-insensitive
/// root/segment comparison on Windows, like Node's win32 implementation).
#[must_use]
pub fn node_relative(from: &str, to: &str) -> String {
    let from = node_resolve(&[from]);
    let to = node_resolve(&[to]);
    let eq = |a: &str, b: &str| {
        if cfg!(windows) {
            a.to_lowercase() == b.to_lowercase()
        } else {
            a == b
        }
    };
    if eq(&from, &to) {
        return String::new();
    }
    let from_parts: Vec<&str> = from.split(is_sep).filter(|p| !p.is_empty()).collect();
    let to_parts: Vec<&str> = to.split(is_sep).filter(|p| !p.is_empty()).collect();
    let common = from_parts
        .iter()
        .zip(&to_parts)
        .take_while(|(a, b)| eq(a, b))
        .count();
    if cfg!(windows) && common == 0 {
        // 不同盘符/UNC 根时 Node 直接返回解析后的 `to`。
        return to;
    }
    let mut out: Vec<&str> = vec![".."; from_parts.len() - common];
    out.extend_from_slice(&to_parts[common..]);
    out.join(&SEP.to_string())
}

/// Splits on `\r?\n` after `replace(/\r\n/g, "\n")`.
#[must_use]
pub fn normalized_lines(text: &str) -> Vec<&str> {
    text.split('\n')
        .map(|line| line.strip_suffix('\r').unwrap_or(line))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_int_matches_js() {
        assert_eq!(js_parse_int("12"), Some(12));
        assert_eq!(js_parse_int(" 7abc"), Some(7));
        assert_eq!(js_parse_int("-"), None);
        assert_eq!(js_parse_int(""), None);
        assert_eq!(js_parse_int("-3"), Some(-3));
    }

    #[test]
    fn workspace_scope() {
        assert_eq!(normalize_workspace_in_repo_path(""), ".");
        assert_eq!(normalize_workspace_in_repo_path("pkg/a/"), "pkg/a");
        assert!(is_path_in_workspace_scope("pkg/a/x.ts", "pkg/a/"));
        assert!(!is_path_in_workspace_scope("pkg/ab/x.ts", "pkg/a"));
        assert_eq!(
            to_workspace_relative_git_path("pkg/a/x.ts", "pkg/a/"),
            "x.ts"
        );
        assert_eq!(to_workspace_relative_git_path("pkg/a", "pkg/a"), ".");
        assert_eq!(
            to_workspace_relative_git_path("other/x", "pkg/a"),
            "other/x"
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_paths() {
        assert_eq!(
            node_resolve(&["C:/Repo", "src", "a.ts"]),
            "C:\\Repo\\src\\a.ts"
        );
        assert_eq!(node_resolve(&["C:/Repo", "dir/"]), "C:\\Repo\\dir");
        assert_eq!(node_resolve(&["C:\\"]), "C:\\");
        assert_eq!(node_relative("C:/Repo", "c:\\repo\\Src\\x"), "Src\\x");
        assert_eq!(node_relative("C:/Repo/a", "C:/Repo/b"), "..\\b");
        assert_eq!(node_relative("C:/Repo", "D:/x"), "D:\\x");
        assert_eq!(node_relative("C:/Repo", "C:/Repo"), "");
        assert!(node_is_absolute("C:/x"));
        assert!(!node_is_absolute("C:x"));
    }

    #[cfg(unix)]
    #[test]
    fn unix_paths() {
        assert_eq!(node_resolve(&["/repo", "src", "a.ts"]), "/repo/src/a.ts");
        assert_eq!(node_relative("/repo", "/repo/a/b"), "a/b");
        assert_eq!(node_relative("/repo/a", "/repo/b"), "../b");
    }
}
