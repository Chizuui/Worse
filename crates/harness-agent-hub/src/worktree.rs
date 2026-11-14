use std::path::{Path, PathBuf};

use harness_core_api::{WorktreeInfo, WorktreeRequest};

use crate::snapshot::git_text;

const ROOT: &str = "worktrees";

pub(crate) struct Created {
    pub info: WorktreeInfo,
    pub cwd: PathBuf,
}

fn fnv1a(text: &str) -> u32 {
    let mut hash: u32 = 0x811c_9dc5;
    for byte in text.as_bytes() {
        hash ^= u32::from(*byte);
        hash = hash.wrapping_mul(0x0100_0193);
    }
    hash
}

pub(crate) fn repo_key(repo_root: &str) -> String {
    let name = Path::new(repo_root)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let clean: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .take(40)
        .collect();
    let clean = if clean.is_empty() {
        "repo".to_owned()
    } else {
        clean
    };
    format!(
        "{clean}-{:08x}",
        fnv1a(&repo_root.replace('\\', "/").to_lowercase())
    )
}

fn default_branch(session_id: &str) -> String {
    let tail: String = session_id
        .chars()
        .rev()
        .take(8)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    format!("harness/{tail}")
}

pub(crate) async fn create(
    home: &Path,
    workspace: &Path,
    session_id: &str,
    request: &WorktreeRequest,
) -> Result<Created, String> {
    let top = git_text(workspace, &["rev-parse", "--show-toplevel"])
        .await
        .map_err(|e| {
            if e.is_empty() {
                "the workspace is not a git repository".to_owned()
            } else {
                format!("the workspace is not a git repository ({e})")
            }
        })?;
    if top.is_empty() {
        return Err("the workspace is not a git repository".into());
    }
    let repo_root = PathBuf::from(&top);
    let prefix = git_text(workspace, &["rev-parse", "--show-prefix"])
        .await
        .unwrap_or_default();
    let branch = request
        .branch
        .as_deref()
        .map(str::trim)
        .filter(|b| !b.is_empty())
        .map_or_else(|| default_branch(session_id), str::to_owned);
    git_text(&repo_root, &["check-ref-format", "--branch", &branch])
        .await
        .map_err(|_| format!("invalid branch name: {branch}"))?;
    let reference = format!("refs/heads/{branch}");
    if git_text(&repo_root, &["show-ref", "--verify", "--quiet", &reference])
        .await
        .is_ok()
    {
        return Err(format!("branch {branch} already exists"));
    }
    let base = request
        .base
        .as_deref()
        .map(str::trim)
        .filter(|b| !b.is_empty())
        .unwrap_or("HEAD")
        .to_owned();
    let dir = home.join(ROOT).join(repo_key(&top)).join(session_id);
    if let Some(parent) = dir.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|e| format!("{}: {e}", parent.display()))?;
    }
    let dir_text = dir.to_string_lossy().into_owned();
    git_text(
        &repo_root,
        &["worktree", "add", "-b", &branch, &dir_text, &base],
    )
    .await
    .map_err(|e| format!("git worktree add failed: {e}"))?;
    let cwd = prefix
        .split('/')
        .filter(|p| !p.is_empty())
        .fold(dir.clone(), |path, part| path.join(part));
    Ok(Created {
        info: WorktreeInfo {
            path: dir_text,
            branch,
            base,
            repo_root: top,
        },
        cwd,
    })
}

pub(crate) async fn remove(info: &WorktreeInfo) -> Result<(), String> {
    git_text(
        Path::new(&info.repo_root),
        &["worktree", "remove", &info.path],
    )
    .await
    .map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repo_key_is_stable_and_clean() {
        let a = repo_key("C:/Users/x/My Repo");
        assert!(a.starts_with("My-Repo-"));
        // Backslash is a separator only on Windows.
        if cfg!(windows) {
            assert_eq!(a, repo_key("C:\\Users\\x\\My Repo"));
        }
        assert_eq!(default_branch("ses_0123456789abcdef"), "harness/89abcdef");
    }
}
