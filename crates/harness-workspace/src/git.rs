//! Git: reads through `harness-git` (blocking, called inside `spawn_blocking`), writes (stage, unstage,
//! commit) through the git CLI asynchronously, and the OS opener.

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use harness_core_api::diff::{parse_unified, FileDiff, FileDiffStatus};
use harness_core_api::workspace::{
    BranchInfo, CommitResult, GitChangeKind, GitFileChange, GitSection, GitStatus,
};
use harness_core_api::{ApiError, ApiResult};
use harness_git::types::{ChangeKind, ChangeSection, DiffAvailability, FileChange, HeadRefType};
use harness_git::{never_cancelled, DiffQuery, GitError, GitService};
use tokio::process::Command;

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Upper bound of one git write command (hooks may run on commit).
const GIT_WRITE_LIMIT: Duration = Duration::from_secs(120);

fn git_error(error: &GitError) -> ApiError {
    ApiError::Internal(error.to_string())
}

fn change(c: &FileChange) -> GitFileChange {
    let kind = if c.is_conflicted {
        GitChangeKind::Conflicted
    } else if c.is_untracked {
        GitChangeKind::Untracked
    } else {
        match c.kind {
            ChangeKind::Modified => GitChangeKind::Modified,
            ChangeKind::Added => GitChangeKind::Added,
            ChangeKind::Deleted => GitChangeKind::Deleted,
            ChangeKind::Renamed => GitChangeKind::Renamed,
        }
    };
    let section = match c.section {
        ChangeSection::Staged => GitSection::Staged,
        ChangeSection::Untracked => GitSection::Untracked,
        ChangeSection::Conflicted => GitSection::Conflicted,
        ChangeSection::Unstaged | ChangeSection::Branch => GitSection::Unstaged,
    };
    GitFileChange {
        path: c.workspace_relative_path.replace('\\', "/"),
        old_path: None,
        kind,
        section,
        added: c.added,
        removed: c.removed,
    }
}

/// `git_status` (blocking).
pub(crate) fn status(service: &GitService, root: &str) -> ApiResult<GitStatus> {
    let refresh = service
        .refresh(root, false, false, &never_cancelled)
        .map_err(|e| git_error(&e))?;
    let summary = refresh.summary;
    Ok(GitStatus {
        is_repository: summary.is_repository,
        git_available: summary.is_git_available,
        branch: summary.branch_name,
        upstream: summary.tracking_branch_name,
        ahead: summary.ahead,
        behind: summary.behind,
        detached: summary.head_ref_type == HeadRefType::Detached,
        staged: refresh.staged_changes.iter().map(change).collect(),
        unstaged: refresh.unstaged_changes.iter().map(change).collect(),
    })
}

/// `git_file_diff` (blocking).
pub(crate) fn file_diff(
    service: &GitService,
    root: &str,
    relative: &str,
    staged: bool,
) -> ApiResult<FileDiff> {
    let result = service
        .diff(
            &DiffQuery {
                workspace_path: root.to_owned(),
                path: relative.to_owned(),
                staged: Some(staged),
                source_id: None,
            },
            &never_cancelled,
        )
        .map_err(|e| git_error(&e))?;
    let empty = |binary: bool| FileDiff {
        path: relative.to_owned(),
        old_path: None,
        status: FileDiffStatus::Modified,
        binary,
        added: 0,
        removed: 0,
        hunks: Vec::new(),
    };
    match result.availability {
        DiffAvailability::Binary => Ok(empty(true)),
        DiffAvailability::Unavailable | DiffAvailability::Truncated if result.patch.is_none() => {
            Ok(empty(false))
        }
        _ => {
            let patch = result.patch.unwrap_or_default();
            let mut file = parse_unified(&patch)
                .into_iter()
                .next()
                .unwrap_or_else(|| empty(false));
            // Patch paths are repository-relative; the API speaks workspace-relative paths.
            relative.clone_into(&mut file.path);
            Ok(file)
        }
    }
}

/// `git_branches` (blocking).
pub(crate) fn branches(service: &GitService, root: &str) -> ApiResult<Vec<BranchInfo>> {
    let list = service
        .local_branches(root, &never_cancelled)
        .map_err(|e| git_error(&e))?;
    Ok(list
        .branches
        .into_iter()
        .map(|b| BranchInfo {
            name: b.name,
            current: b.is_current,
            upstream: b.upstream_name,
        })
        .collect())
}

pub(crate) struct Run {
    pub(crate) ok: bool,
    pub(crate) stdout: String,
    pub(crate) stderr: String,
}

pub(crate) async fn git(root: &str, args: &[&str]) -> ApiResult<Run> {
    let mut command = Command::new("git");
    command
        .args(["-c", "core.quotepath=false"])
        .args(args)
        .current_dir(root)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(windows)]
    command.creation_flags(CREATE_NO_WINDOW);
    let output = tokio::time::timeout(GIT_WRITE_LIMIT, command.output())
        .await
        .map_err(|_| ApiError::Internal(format!("git {} did not finish in time", args.join(" "))))?
        .map_err(|e| ApiError::Internal(format!("git: {e}")))?;
    Ok(Run {
        ok: output.status.success(),
        stdout: String::from_utf8_lossy(&output.stdout).trim().to_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
    })
}

fn with_paths<'a>(head: &[&'a str], paths: &'a [String]) -> Vec<&'a str> {
    let mut args: Vec<&str> = head.to_vec();
    args.push("--");
    args.extend(paths.iter().map(String::as_str));
    args
}

pub(crate) fn failed(label: &str, run: &Run) -> ApiError {
    ApiError::Internal(format!("git {label} failed: {}", run.stderr))
}

/// `git add -A -- paths`.
pub(crate) async fn stage(root: &str, paths: &[String]) -> ApiResult<()> {
    let run = git(root, &with_paths(&["add", "-A"], paths)).await?;
    if run.ok {
        Ok(())
    } else {
        Err(failed("add", &run))
    }
}

/// `git restore --staged -- paths`, or `git rm --cached` before the first commit.
pub(crate) async fn unstage(root: &str, paths: &[String]) -> ApiResult<()> {
    let has_head = git(root, &["rev-parse", "--verify", "-q", "HEAD"])
        .await?
        .ok;
    let run = if has_head {
        git(root, &with_paths(&["restore", "--staged"], paths)).await?
    } else {
        git(root, &with_paths(&["rm", "--cached", "-r", "-q"], paths)).await?
    };
    if run.ok {
        Ok(())
    } else {
        Err(failed("unstage", &run))
    }
}

/// `git commit -m message`.
pub(crate) async fn commit(root: &str, message: &str) -> ApiResult<CommitResult> {
    if message.trim().is_empty() {
        return Err(ApiError::InvalidArgument("commit message is empty".into()));
    }
    let staged = git(root, &["diff", "--cached", "--quiet"]).await?;
    if staged.ok {
        return Err(ApiError::InvalidArgument("nothing staged to commit".into()));
    }
    let run = git(root, &["commit", "-q", "-m", message]).await?;
    if !run.ok {
        return Err(failed("commit", &run));
    }
    let hash = git(root, &["rev-parse", "HEAD"]).await?;
    let summary = git(root, &["log", "-1", "--format=%s"]).await?;
    Ok(CommitResult {
        hash: hash.stdout,
        summary: summary.stdout,
    })
}

/// Opens a path with the OS default application (no shell string is built).
pub(crate) fn open_with_os(path: &Path) -> ApiResult<()> {
    let program = if cfg!(windows) {
        "explorer.exe"
    } else if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    let mut command = Command::new(program);
    command
        .arg(path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(windows)]
    command.creation_flags(CREATE_NO_WINDOW);
    let mut child = command
        .spawn()
        .map_err(|e| ApiError::Internal(format!("{program}: {e}")))?;
    // explorer.exe exits with 1 even on success, so the exit code is not an error signal; reap the child.
    tokio::spawn(async move {
        let _ = child.wait().await;
    });
    Ok(())
}
