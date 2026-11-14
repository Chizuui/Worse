//! Read-only port of `createGitService` (`packages/services/src/git/gitService.ts`):
//! workspace scoping of status entries and branch-comparison changes.

use std::thread;

use crate::diff::DiffQuery;
use crate::error::GitError;
use crate::parse::{LineStat, StatusEntry};
use crate::repo::{
    join, to_absolute_path, BranchChange, BranchComparisonSnapshot, GitRepo, StatusSnapshot,
};
use crate::runner::CancelFn;
use crate::types::{
    BranchComparison, ChangeSection, CommitGraph, DiffResult, FileChange, Identity,
    LocalBranchList, RefreshResult, RepositorySummary, WorkspaceRepositoryInfo,
};
use crate::util::{is_path_in_workspace_scope, to_workspace_relative_git_path};

/// Which status section `getChanges` returns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangeSource {
    /// Unstaged, untracked and conflicted entries.
    Unstaged,
    /// Staged entries.
    Staged,
}

/// The git service (one per process).
#[derive(Debug, Default)]
pub struct GitService {
    repo: GitRepo,
}

fn matches_scope(workspace_in_repo_path: &str, path: &str, original: Option<&str>) -> bool {
    is_path_in_workspace_scope(path, workspace_in_repo_path)
        || original.is_some_and(|o| is_path_in_workspace_scope(o, workspace_in_repo_path))
}

fn build_change(
    snapshot: &StatusSnapshot,
    entry: &StatusEntry,
    section: ChangeSection,
    stat: Option<&LineStat>,
) -> FileChange {
    let (added, removed) = stat.map_or((0, 0), |s| (s.added, s.removed));
    FileChange {
        path: to_absolute_path(&snapshot.resolution.repo_root, &entry.path),
        repo_relative_path: entry.path.clone(),
        workspace_relative_path: to_workspace_relative_git_path(
            &entry.path,
            &snapshot.summary.workspace_in_repo_path,
        ),
        x: entry.x.clone(),
        y: entry.y.clone(),
        kind: entry.kind,
        section,
        added,
        removed,
        is_staged: section == ChangeSection::Staged,
        is_untracked: section == ChangeSection::Untracked,
        is_conflicted: section == ChangeSection::Conflicted,
    }
}

/// `toChangeForSource`.
fn change_for_source(
    snapshot: &StatusSnapshot,
    entry: &StatusEntry,
    source: ChangeSource,
) -> Option<FileChange> {
    if !matches_scope(
        &snapshot.summary.workspace_in_repo_path,
        &entry.path,
        entry.original_path.as_deref(),
    ) {
        return None;
    }
    if source == ChangeSource::Staged {
        if entry.is_untracked
            || entry.is_conflicted
            || entry.x.as_deref().map_or(true, |x| x == ".")
        {
            return None;
        }
        let stat = snapshot.staged_stats.get(&entry.path);
        return Some(build_change(snapshot, entry, ChangeSection::Staged, stat));
    }
    if entry.is_conflicted {
        return Some(build_change(
            snapshot,
            entry,
            ChangeSection::Conflicted,
            None,
        ));
    }
    if entry.is_untracked {
        let stat = snapshot.untracked_stats.get(&entry.path);
        let (added, removed) = stat.map_or((0, 0), |s| (s.added, s.removed));
        // 构建产物里大量 +0/-0 的未跟踪条目不进入 Review；但折叠目录（末尾 `/`）必须保留。
        if !(entry.path.ends_with('/') || added > 0 || removed > 0) {
            return None;
        }
        return Some(build_change(
            snapshot,
            entry,
            ChangeSection::Untracked,
            stat,
        ));
    }
    if entry.y.as_deref().map_or(true, |y| y == ".") {
        return None;
    }
    let stat = snapshot.unstaged_stats.get(&entry.path);
    Some(build_change(snapshot, entry, ChangeSection::Unstaged, stat))
}

/// `getChangesForSource`.
#[must_use]
pub fn changes_for_source(snapshot: &StatusSnapshot, source: ChangeSource) -> Vec<FileChange> {
    snapshot
        .entries
        .iter()
        .filter_map(|entry| change_for_source(snapshot, entry, source))
        .collect()
}

fn branch_change(snapshot: &BranchComparisonSnapshot, change: &BranchChange) -> Option<FileChange> {
    let scope = &snapshot.resolution.workspace_in_repo_path;
    if !matches_scope(scope, &change.path, change.original_path.as_deref()) {
        return None;
    }
    Some(FileChange {
        path: to_absolute_path(&snapshot.resolution.repo_root, &change.path),
        repo_relative_path: change.path.clone(),
        workspace_relative_path: to_workspace_relative_git_path(&change.path, scope),
        x: None,
        y: None,
        kind: change.kind,
        section: ChangeSection::Branch,
        added: change.added,
        removed: change.removed,
        is_staged: false,
        is_untracked: false,
        is_conflicted: false,
    })
}

fn to_branch_comparison(snapshot: &BranchComparisonSnapshot) -> BranchComparison {
    BranchComparison {
        base_ref: snapshot.base_ref.clone(),
        head_ref: snapshot.head_ref.clone(),
        comparison_label: snapshot.comparison_label.clone(),
        changes: snapshot
            .changes
            .iter()
            .filter_map(|change| branch_change(snapshot, change))
            .collect(),
    }
}

impl GitService {
    /// Service with default git discovery.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Service with a fixed git binary (`None` simulates a missing git).
    #[must_use]
    pub fn with_binary(binary: Option<String>) -> Self {
        Self {
            repo: GitRepo::with_binary(binary),
        }
    }

    /// `getRepositorySummary`.
    ///
    /// # Errors
    /// TS-equivalent `GitError::App` messages; `Cancelled`.
    pub fn repository_summary(
        &self,
        workspace_path: &str,
        cancel: CancelFn<'_>,
    ) -> Result<RepositorySummary, GitError> {
        Ok(self.repo.status(workspace_path, cancel)?.summary)
    }

    /// `getWorkspaceRepositoryInfo`.
    ///
    /// # Errors
    /// See [`GitService::repository_summary`].
    pub fn workspace_repository_info(
        &self,
        workspace_path: &str,
        cancel: CancelFn<'_>,
    ) -> Result<WorkspaceRepositoryInfo, GitError> {
        self.repo.workspace_repository_info(workspace_path, cancel)
    }

    /// `getLocalBranches`.
    ///
    /// # Errors
    /// See [`GitService::repository_summary`].
    pub fn local_branches(
        &self,
        workspace_path: &str,
        cancel: CancelFn<'_>,
    ) -> Result<LocalBranchList, GitError> {
        self.repo.local_branches(workspace_path, cancel)
    }

    /// `getCommitGraph`.
    ///
    /// # Errors
    /// See [`GitService::repository_summary`].
    pub fn commit_graph(
        &self,
        workspace_path: &str,
        max_count: Option<f64>,
        skip: Option<f64>,
        cancel: CancelFn<'_>,
    ) -> Result<CommitGraph, GitError> {
        self.repo
            .commit_graph(workspace_path, max_count, skip, cancel)
    }

    /// `getChanges`.
    ///
    /// # Errors
    /// See [`GitService::repository_summary`].
    pub fn changes(
        &self,
        workspace_path: &str,
        source: ChangeSource,
        cancel: CancelFn<'_>,
    ) -> Result<Vec<FileChange>, GitError> {
        let status = self.repo.status(workspace_path, cancel)?;
        Ok(changes_for_source(&status, source))
    }

    /// `getIgnoredPaths`.
    ///
    /// # Errors
    /// See [`GitService::repository_summary`].
    pub fn ignored_paths(
        &self,
        workspace_path: &str,
        paths: &[String],
        cancel: CancelFn<'_>,
    ) -> Result<Vec<String>, GitError> {
        self.repo.ignored_paths(workspace_path, paths, cancel)
    }

    /// `getDiff`.
    ///
    /// # Errors
    /// See [`GitService::repository_summary`].
    pub fn diff(&self, query: &DiffQuery, cancel: CancelFn<'_>) -> Result<DiffResult, GitError> {
        self.repo.diff(query, cancel)
    }

    /// `getBranchComparison`.
    ///
    /// # Errors
    /// See [`GitService::repository_summary`].
    pub fn branch_comparison(
        &self,
        workspace_path: &str,
        cancel: CancelFn<'_>,
    ) -> Result<BranchComparison, GitError> {
        let status = self.repo.status(workspace_path, cancel)?;
        Ok(to_branch_comparison(
            &self.repo.branch_comparison(&status, cancel)?,
        ))
    }

    /// `getIdentity`.
    ///
    /// # Errors
    /// See [`GitService::repository_summary`].
    pub fn identity(
        &self,
        workspace_path: &str,
        cancel: CancelFn<'_>,
    ) -> Result<Identity, GitError> {
        self.repo.identity(workspace_path, cancel)
    }

    /// `refresh`: one status snapshot for summary + unstaged + staged (and the
    /// branch comparison), identity read concurrently.
    ///
    /// # Errors
    /// See [`GitService::repository_summary`].
    pub fn refresh(
        &self,
        workspace_path: &str,
        include_identity: bool,
        include_branch_comparison: bool,
        cancel: CancelFn<'_>,
    ) -> Result<RefreshResult, GitError> {
        // 修复说明（性能）：旧实现 status 与 identity 各自调用 resolve_repository，一次 refresh
        // 跑两遍 `git rev-parse`（fixes/ts.md：5.33 vs 4.33 个进程）。TS 通过 in-flight 复用只解析
        // 一次；这里先解析一次，再把同一个 Resolution 交给两条并发分支。
        let resolution = self.repo.resolve_repository(workspace_path, cancel)?;
        let (status, identity) = thread::scope(|scope| {
            let identity = include_identity
                .then(|| scope.spawn(|| self.repo.identity_for(&resolution, cancel)));
            let status = self.repo.status_for(resolution.clone(), cancel);
            (status, identity.map(join))
        });
        let status = status?;
        let identity = identity.transpose()?;
        let branch_comparison = if include_branch_comparison {
            Some(to_branch_comparison(
                &self.repo.branch_comparison(&status, cancel)?,
            ))
        } else {
            None
        };
        Ok(RefreshResult {
            unstaged_changes: changes_for_source(&status, ChangeSource::Unstaged),
            staged_changes: changes_for_source(&status, ChangeSource::Staged),
            summary: status.summary,
            identity,
            branch_comparison,
        })
    }
}
