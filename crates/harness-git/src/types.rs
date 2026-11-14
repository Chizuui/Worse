//! Wire types mirroring `packages/shared/src/git.ts` (camelCase JSON).

use serde::Serialize;

/// `GitHeadRefType`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum HeadRefType {
    /// On a branch.
    Branch,
    /// Detached HEAD.
    Detached,
}

/// `GitChangeKind`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ChangeKind {
    /// Modified.
    Modified,
    /// Added.
    Added,
    /// Deleted.
    Deleted,
    /// Renamed or copied.
    Renamed,
}

/// `GitChangeSectionId` values produced by the read paths.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ChangeSection {
    /// Staged.
    Staged,
    /// Unstaged.
    Unstaged,
    /// Untracked.
    Untracked,
    /// Conflicted.
    Conflicted,
    /// Branch comparison.
    Branch,
}

/// `GitRepositoryAutoRefreshWatchPath`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WatchPath {
    /// Watched path.
    pub path: String,
    /// Recursive watch.
    pub recursive: bool,
}

/// `GitRepositorySummary`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RepositorySummary {
    /// Workspace path as given.
    pub workspace_path: String,
    /// Repository root as printed by git.
    pub repo_root: String,
    /// Workspace prefix inside the repository (`.` for the root).
    pub workspace_in_repo_path: String,
    /// Git metadata directories to watch.
    pub auto_refresh_watch_paths: Vec<WatchPath>,
    /// Current branch.
    pub branch_name: Option<String>,
    /// Upstream branch.
    pub tracking_branch_name: Option<String>,
    /// HEAD kind.
    pub head_ref_type: HeadRefType,
    /// Commits ahead of upstream.
    pub ahead: u64,
    /// Commits behind upstream.
    pub behind: u64,
    /// Any status entry present.
    pub is_dirty: bool,
    /// Git binary found.
    pub is_git_available: bool,
    /// Workspace is inside a repository.
    pub is_repository: bool,
}

/// `GitFileChange`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FileChange {
    /// Absolute path with platform separators.
    pub path: String,
    /// Repository-relative path (`/`).
    pub repo_relative_path: String,
    /// Workspace-relative path (`/`).
    pub workspace_relative_path: String,
    /// Index status letter.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub x: Option<String>,
    /// Worktree status letter.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub y: Option<String>,
    /// Change kind.
    pub kind: ChangeKind,
    /// Section.
    pub section: ChangeSection,
    /// Added lines.
    pub added: u64,
    /// Removed lines.
    pub removed: u64,
    /// `section == staged`.
    pub is_staged: bool,
    /// `section == untracked`.
    pub is_untracked: bool,
    /// `section == conflicted`.
    pub is_conflicted: bool,
}

/// `GitDiffAvailability`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum DiffAvailability {
    /// Text patch.
    Patch,
    /// Binary.
    Binary,
    /// Over the preview limit.
    Truncated,
    /// No diff.
    Unavailable,
}

/// `GitDiffResult`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DiffResult {
    /// Absolute path.
    pub path: String,
    /// Availability.
    pub availability: DiffAvailability,
    /// Patch text.
    pub patch: Option<String>,
    /// Full before content.
    pub before_content: Option<String>,
    /// Full after content.
    pub after_content: Option<String>,
    /// Human summary.
    pub summary: Option<String>,
}

impl DiffResult {
    pub(crate) fn without_patch(
        path: &str,
        availability: DiffAvailability,
        summary: impl Into<String>,
    ) -> Self {
        Self {
            path: path.to_owned(),
            availability,
            patch: None,
            before_content: None,
            after_content: None,
            summary: Some(summary.into()),
        }
    }

    pub(crate) fn unavailable(path: &str, summary: impl Into<String>) -> Self {
        Self::without_patch(path, DiffAvailability::Unavailable, summary)
    }
}

/// `GitIdentity`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct Identity {
    /// `user.name`.
    pub user_name: Option<String>,
    /// `user.email`.
    pub user_email: Option<String>,
    /// Origin of `user.name`.
    pub name_source: Option<String>,
    /// Origin of `user.email`.
    pub email_source: Option<String>,
    /// Config scope label.
    pub scope_label: Option<String>,
}

/// `GitCommitGraphRefKind`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum GraphRefKind {
    /// Local branch.
    Branch,
    /// Remote branch.
    Remote,
    /// Tag.
    Tag,
    /// HEAD.
    Head,
}

/// `GitCommitGraphRef`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct GraphRef {
    /// Ref name.
    pub name: String,
    /// Ref kind.
    pub kind: GraphRefKind,
}

/// `GitCommitGraphCommit`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GraphCommit {
    /// Commit hash.
    pub hash: String,
    /// Parent hashes.
    pub parents: Vec<String>,
    /// Decorations.
    pub refs: Vec<GraphRef>,
    /// Subject line.
    pub subject: String,
    /// Author name.
    pub author_name: Option<String>,
    /// Author timestamp (ms).
    pub authored_at_ms: Option<i64>,
}

/// `GitCommitGraphResult`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CommitGraph {
    /// Commits, newest first.
    pub commits: Vec<GraphCommit>,
    /// More commits beyond this page.
    pub has_more: bool,
}

/// `GitWorkspaceRepositoryKind`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum WorkspaceRepositoryKind {
    /// Not inside a repository.
    NotRepository,
    /// Main working tree.
    MainTree,
    /// Linked worktree.
    LinkedWorktree,
}

/// `GitWorkspaceRepositoryInfo`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceRepositoryInfo {
    /// Workspace path.
    pub workspace_path: String,
    /// Kind.
    pub kind: WorkspaceRepositoryKind,
    /// Git binary found.
    pub is_git_available: bool,
}

/// `GitLocalBranch`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LocalBranch {
    /// Branch name.
    pub name: String,
    /// Checked out.
    pub is_current: bool,
    /// Upstream.
    pub upstream_name: Option<String>,
    /// Tip commit.
    pub commit_hash: Option<String>,
    /// Committer timestamp (ms).
    pub commit_timestamp_ms: Option<i64>,
}

/// `GitLocalBranchListResult`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LocalBranchList {
    /// HEAD kind.
    pub head_ref_type: HeadRefType,
    /// Current branch.
    pub current_branch_name: Option<String>,
    /// Branches.
    pub branches: Vec<LocalBranch>,
}

/// `GitBranchComparison`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BranchComparison {
    /// Upstream ref.
    pub base_ref: Option<String>,
    /// Head ref.
    pub head_ref: Option<String>,
    /// Display label.
    pub comparison_label: Option<String>,
    /// Changes in workspace scope.
    pub changes: Vec<FileChange>,
}

/// `GitRefreshResult`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RefreshResult {
    /// Summary.
    pub summary: RepositorySummary,
    /// Identity when requested.
    pub identity: Option<Identity>,
    /// Unstaged section.
    pub unstaged_changes: Vec<FileChange>,
    /// Staged section.
    pub staged_changes: Vec<FileChange>,
    /// Branch comparison when requested.
    pub branch_comparison: Option<BranchComparison>,
}
