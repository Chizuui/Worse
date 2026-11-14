//! Read-only port of `GitCliRepo` (`packages/services/src/git/repo/gitCliRepo.ts`).

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::Read;
use std::sync::{Mutex, PoisonError};
use std::thread;

use crate::error::GitError;
use crate::parse::{
    kind_from_numstat, parse_branch_refs, parse_config_value, parse_graph_records, parse_numstat,
    parse_status_porcelain, ConfigValue, LineStat, StatMap, StatusEntry,
};
use crate::runner::{CancelFn, ExecResult, GitRunner, COMMAND_TIMEOUT, OUTPUT_BYTES};
use crate::types::{
    ChangeKind, CommitGraph, HeadRefType, Identity, LocalBranchList, RepositorySummary, WatchPath,
    WorkspaceRepositoryInfo, WorkspaceRepositoryKind,
};
use crate::util::{
    js_trim, node_is_absolute, node_relative, node_resolve, normalize_git_path,
    normalize_workspace_in_repo_path, normalized_lines,
};

/// `GIT_UNTRACKED_STAT_MAX_BYTES`.
const UNTRACKED_STAT_MAX_BYTES: u64 = 1024 * 1024;
/// `GIT_UNTRACKED_STAT_CHUNK_BYTES`.
const UNTRACKED_STAT_CHUNK_BYTES: usize = 64 * 1024;
/// `GIT_UNTRACKED_STAT_CONCURRENCY`.
const UNTRACKED_STAT_CONCURRENCY: usize = 4;
const DEFAULT_GRAPH_MAX_COUNT: u64 = 100;
const MAX_GRAPH_MAX_COUNT_F64: f64 = 200.0;

/// `GitResolvedRepository`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolution {
    /// Workspace path as given.
    pub workspace_path: String,
    /// Repository root (git output; the workspace path when not a repo).
    pub repo_root: String,
    /// Workspace prefix inside the repository.
    pub workspace_in_repo_path: String,
    /// Watch paths.
    pub auto_refresh_watch_paths: Vec<WatchPath>,
    /// Git binary found.
    pub is_git_available: bool,
    /// Inside a repository.
    pub is_repository: bool,
}

impl Resolution {
    fn unavailable(workspace_path: &str, is_git_available: bool) -> Self {
        Self {
            workspace_path: workspace_path.to_owned(),
            repo_root: workspace_path.to_owned(),
            workspace_in_repo_path: ".".to_owned(),
            auto_refresh_watch_paths: Vec::new(),
            is_git_available,
            is_repository: false,
        }
    }

    /// Usable repository.
    #[must_use]
    pub fn is_usable(&self) -> bool {
        self.is_git_available && self.is_repository
    }

    /// `createEmptySummary`.
    #[must_use]
    pub fn empty_summary(&self) -> RepositorySummary {
        RepositorySummary {
            workspace_path: self.workspace_path.clone(),
            repo_root: self.repo_root.clone(),
            workspace_in_repo_path: self.workspace_in_repo_path.clone(),
            auto_refresh_watch_paths: self.auto_refresh_watch_paths.clone(),
            branch_name: None,
            tracking_branch_name: None,
            head_ref_type: HeadRefType::Branch,
            ahead: 0,
            behind: 0,
            is_dirty: false,
            is_git_available: self.is_git_available,
            is_repository: self.is_repository,
        }
    }
}

/// `GitStatusSnapshot`.
#[derive(Debug, Clone)]
pub struct StatusSnapshot {
    /// Resolution.
    pub resolution: Resolution,
    /// Summary.
    pub summary: RepositorySummary,
    /// Entries.
    pub entries: Vec<StatusEntry>,
    /// `diff --cached --numstat`.
    pub staged_stats: StatMap,
    /// `diff --numstat`.
    pub unstaged_stats: StatMap,
    /// Line counts of untracked files.
    pub untracked_stats: HashMap<String, LineStat>,
}

/// `GitBranchComparisonChange`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BranchChange {
    /// Path.
    pub path: String,
    /// Rename source.
    pub original_path: Option<String>,
    /// Kind.
    pub kind: ChangeKind,
    /// Added.
    pub added: u64,
    /// Removed.
    pub removed: u64,
}

/// `GitBranchComparisonSnapshot`.
#[derive(Debug, Clone)]
pub struct BranchComparisonSnapshot {
    /// Resolution.
    pub resolution: Resolution,
    /// Upstream.
    pub base_ref: Option<String>,
    /// Head.
    pub head_ref: Option<String>,
    /// Label.
    pub comparison_label: Option<String>,
    /// Changes.
    pub changes: Vec<BranchChange>,
}

/// The repository reader. One per process (holds the binary cache and the
/// collapsed-untracked memory, like the TS closure state).
#[derive(Debug, Default)]
pub struct GitRepo {
    pub(crate) runner: GitRunner,
    collapsed_untracked_roots: Mutex<HashSet<String>>,
}

/// `normalizeWatchPath`.
fn normalize_watch_path(path: &str) -> String {
    let trimmed = js_trim(path);
    let b = trimmed.as_bytes();
    let is_drive_root = (b.len() == 2 || b.len() == 3)
        && b[0].is_ascii_alphabetic()
        && b[1] == b':'
        && (b.len() == 2 || b[2] == b'/' || b[2] == b'\\');
    if trimmed == "/" || is_drive_root {
        return trimmed.to_owned();
    }
    trimmed.trim_end_matches(['/', '\\']).to_owned()
}

fn add_watch_path(paths: &mut Vec<WatchPath>, path: &str) {
    let normalized = normalize_watch_path(path);
    if normalized.is_empty() || paths.iter().any(|p| p.path == normalized) {
        return;
    }
    paths.push(WatchPath {
        path: normalized,
        recursive: true,
    });
}

/// `buildAutoRefreshWatchPaths`.
fn build_watch_paths(
    workspace_path: &str,
    absolute_git_dir: &str,
    common_dir: &str,
) -> Vec<WatchPath> {
    let mut paths = Vec::new();
    add_watch_path(&mut paths, absolute_git_dir);
    let resolved_common = if common_dir.is_empty() {
        absolute_git_dir.to_owned()
    } else if node_is_absolute(common_dir) {
        common_dir.to_owned()
    } else {
        node_resolve(&[workspace_path, common_dir])
    };
    add_watch_path(&mut paths, &resolved_common);
    paths
}

fn is_not_repository(result: &ExecResult) -> bool {
    let stderr = result.stderr.to_lowercase();
    stderr.contains("not a git repository") || stderr.contains("outside repository")
}

fn is_missing_working_directory(result: &ExecResult) -> bool {
    let stderr = result.stderr.to_lowercase();
    (result.exit_code == Some(-2) && stderr.contains("enoent"))
        || stderr.contains("unable to read current working directory")
        || stderr.contains("no such file or directory")
}

/// `toAbsolutePath(repoRoot, repoRelativePath)`.
#[must_use]
pub fn to_absolute_path(repo_root: &str, repo_relative_path: &str) -> String {
    let normalized = normalize_git_path(repo_relative_path);
    let mut segments: Vec<&str> = vec![repo_root];
    segments.extend(normalized.split('/'));
    node_resolve(&segments)
}

/// Workspace-relative input (with `/`) resolved like
/// `resolve(workspacePath, path.split("/").join(sep))`.
#[must_use]
pub fn resolve_input_path(workspace_path: &str, path: &str) -> String {
    if node_is_absolute(path) {
        return path.to_owned();
    }
    let mut segments: Vec<&str> = vec![workspace_path];
    segments.extend(path.split('/'));
    node_resolve(&segments)
}

/// `countUntrackedFileLines` (errors count as 0 lines).
fn count_untracked_lines(absolute_path: &str, buffer: &mut [u8]) -> u64 {
    let Ok(meta) = std::fs::metadata(absolute_path) else {
        return 0;
    };
    if !meta.is_file() || meta.len() > UNTRACKED_STAT_MAX_BYTES {
        return 0;
    }
    let Ok(mut file) = File::open(absolute_path) else {
        return 0;
    };
    let mut total: u64 = 0;
    let mut newlines: u64 = 0;
    let mut last_byte = b'\n';
    loop {
        let budget = usize::try_from(UNTRACKED_STAT_MAX_BYTES + 1 - total).unwrap_or(usize::MAX);
        let length = buffer.len().min(budget);
        let Ok(read) = file.read(&mut buffer[..length]) else {
            return 0;
        };
        if read == 0 {
            return newlines + u64::from(last_byte != b'\n');
        }
        total += read as u64;
        if total > UNTRACKED_STAT_MAX_BYTES {
            return 0;
        }
        for &b in &buffer[..read] {
            if b == 0 {
                return 0;
            }
            if b == b'\n' {
                newlines += 1;
            }
        }
        last_byte = buffer[read - 1];
    }
}

/// `buildUntrackedStats`.
fn build_untracked_stats(repo_root: &str, entries: &[StatusEntry]) -> HashMap<String, LineStat> {
    let untracked: Vec<&StatusEntry> = entries.iter().filter(|e| e.is_untracked).collect();
    let next = std::sync::atomic::AtomicUsize::new(0);
    let results: Mutex<HashMap<String, LineStat>> = Mutex::new(HashMap::new());
    let workers = UNTRACKED_STAT_CONCURRENCY.min(untracked.len());
    thread::scope(|scope| {
        for _ in 0..workers {
            scope.spawn(|| {
                let mut buffer = vec![0u8; UNTRACKED_STAT_CHUNK_BYTES];
                loop {
                    let index = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    let Some(entry) = untracked.get(index) else {
                        break;
                    };
                    let absolute = to_absolute_path(repo_root, &entry.path);
                    let added = count_untracked_lines(&absolute, &mut buffer);
                    results
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .insert(
                            entry.path.clone(),
                            LineStat {
                                added,
                                ..LineStat::default()
                            },
                        );
                }
            });
        }
    });
    results.into_inner().unwrap_or_else(PoisonError::into_inner)
}

fn normalize_graph_max_count(value: Option<f64>) -> u64 {
    match value {
        Some(v) if v.is_finite() => {
            let floored = v.floor().clamp(1.0, MAX_GRAPH_MAX_COUNT_F64);
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let n = floored as u64;
            n
        }
        _ => DEFAULT_GRAPH_MAX_COUNT,
    }
}

fn normalize_graph_skip(value: Option<f64>) -> u64 {
    match value {
        Some(v) if v.is_finite() && v > 0.0 => {
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let n = v.floor() as u64;
            n
        }
        _ => 0,
    }
}

impl GitRepo {
    /// Repository reader with the default binary discovery.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Reader with a fixed binary (`None` simulates a missing git).
    #[must_use]
    pub fn with_binary(binary: Option<String>) -> Self {
        Self {
            runner: GitRunner::with_binary(binary),
            collapsed_untracked_roots: Mutex::new(HashSet::new()),
        }
    }

    pub(crate) fn git(
        &self,
        cwd: &str,
        args: &[&str],
        cancel: CancelFn<'_>,
    ) -> Result<ExecResult, GitError> {
        self.runner
            .run(cwd, args, COMMAND_TIMEOUT, OUTPUT_BYTES, cancel)
    }

    /// `resolveRepository`.
    ///
    /// # Errors
    /// `GitError::App` when `git rev-parse` fails for another reason.
    pub fn resolve_repository(
        &self,
        workspace_path: &str,
        cancel: CancelFn<'_>,
    ) -> Result<Resolution, GitError> {
        if self.runner.binary().is_none() {
            return Ok(Resolution::unavailable(workspace_path, false));
        }
        let result = self.git(
            workspace_path,
            &[
                "rev-parse",
                "--show-toplevel",
                "--show-prefix",
                "--absolute-git-dir",
                "--git-common-dir",
            ],
            cancel,
        )?;
        if result.exit_code != Some(0) {
            if is_missing_working_directory(&result) || is_not_repository(&result) {
                return Ok(Resolution::unavailable(workspace_path, true));
            }
            result.ensure_ok("git rev-parse")?;
        }
        let lines = normalized_lines(&result.stdout);
        let repo_root = js_trim(lines.first().copied().unwrap_or_default());
        if repo_root.is_empty() {
            return Err(GitError::App(
                "Failed to resolve Git repository root".to_owned(),
            ));
        }
        let line = |i: usize| lines.get(i).copied().unwrap_or_default();
        Ok(Resolution {
            workspace_path: workspace_path.to_owned(),
            repo_root: repo_root.to_owned(),
            workspace_in_repo_path: normalize_workspace_in_repo_path(line(1)),
            auto_refresh_watch_paths: build_watch_paths(
                workspace_path,
                js_trim(line(2)),
                js_trim(line(3)),
            ),
            is_git_available: true,
            is_repository: true,
        })
    }

    /// `getWorkspaceRepositoryInfo`.
    ///
    /// # Errors
    /// Propagates [`GitRepo::resolve_repository`] errors.
    pub fn workspace_repository_info(
        &self,
        workspace_path: &str,
        cancel: CancelFn<'_>,
    ) -> Result<WorkspaceRepositoryInfo, GitError> {
        let resolution = self.resolve_repository(workspace_path, cancel)?;
        let info = |kind| WorkspaceRepositoryInfo {
            workspace_path: workspace_path.to_owned(),
            kind,
            is_git_available: resolution.is_git_available,
        };
        if !resolution.is_usable() {
            return Ok(info(WorkspaceRepositoryKind::NotRepository));
        }
        let git_entry = node_resolve(&[&resolution.repo_root, ".git"]);
        if let Ok(meta) = std::fs::metadata(&git_entry) {
            if meta.is_file() {
                if let Ok(content) = std::fs::read(&git_entry) {
                    let content = String::from_utf8_lossy(&content);
                    let first = js_trim(
                        normalized_lines(&content)
                            .first()
                            .copied()
                            .unwrap_or_default(),
                    );
                    if let Some(raw) = first.strip_prefix("gitdir:") {
                        let raw = js_trim(raw);
                        let resolved = if raw.is_empty() {
                            String::new()
                        } else if node_is_absolute(raw) {
                            raw.to_owned()
                        } else {
                            node_resolve(&[&resolution.repo_root, raw])
                        };
                        // linked worktree 的 `.git` 文件指向 `<main>/.git/worktrees/<name>`。
                        if normalize_git_path(&resolved).contains("/.git/worktrees/") {
                            return Ok(info(WorkspaceRepositoryKind::LinkedWorktree));
                        }
                    }
                }
            }
        }
        Ok(info(WorkspaceRepositoryKind::MainTree))
    }

    fn run_status(
        &self,
        resolution: &Resolution,
        cancel: CancelFn<'_>,
    ) -> Result<ExecResult, GitError> {
        let collapsed = self
            .collapsed_untracked_roots
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .contains(&resolution.repo_root);
        let run = |mode: &str| {
            let flag = format!("--untracked-files={mode}");
            self.git(
                &resolution.repo_root,
                &["status", "--porcelain=v2", "--branch", &flag, "-z"],
                cancel,
            )
        };
        let result = run(if collapsed { "normal" } else { "all" })?;
        if collapsed || !result.output_truncated {
            return Ok(result);
        }
        // 大仓库逐文件未跟踪状态超过输出上限：按 repoRoot 记住折叠模式（与 TS 一致）。
        self.collapsed_untracked_roots
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(resolution.repo_root.clone());
        tracing::warn!(
            repo_root = %resolution.repo_root,
            "git status detailed output exceeded limit; collapsing untracked directories"
        );
        run("normal")
    }

    /// `getStatus` (status and both numstats run concurrently).
    ///
    /// # Errors
    /// `GitError::App` with the TS `ensureGitCommandSucceeded` messages.
    pub fn status(
        &self,
        workspace_path: &str,
        cancel: CancelFn<'_>,
    ) -> Result<StatusSnapshot, GitError> {
        let resolution = self.resolve_repository(workspace_path, cancel)?;
        self.status_for(resolution, cancel)
    }

    /// `getStatus` on an already resolved repository (lets one op share a
    /// single `rev-parse`, like the TS in-flight resolution reuse).
    ///
    /// # Errors
    /// See [`GitRepo::status`].
    pub fn status_for(
        &self,
        resolution: Resolution,
        cancel: CancelFn<'_>,
    ) -> Result<StatusSnapshot, GitError> {
        if !resolution.is_usable() {
            return Ok(StatusSnapshot {
                summary: resolution.empty_summary(),
                resolution,
                entries: Vec::new(),
                staged_stats: StatMap::default(),
                unstaged_stats: StatMap::default(),
                untracked_stats: HashMap::new(),
            });
        }
        let root = resolution.repo_root.as_str();
        let (status, staged, unstaged) = thread::scope(|scope| {
            let staged = scope.spawn(|| {
                self.git(
                    root,
                    &[
                        "diff",
                        "--cached",
                        "--numstat",
                        "-z",
                        "--find-renames",
                        "--",
                    ],
                    cancel,
                )
            });
            let unstaged = scope.spawn(|| {
                self.git(
                    root,
                    &["diff", "--numstat", "-z", "--find-renames", "--"],
                    cancel,
                )
            });
            let status = self.run_status(&resolution, cancel);
            (status, join(staged), join(unstaged))
        });
        let (status, staged, unstaged) = (status?, staged?, unstaged?);
        status.ensure_ok("git status")?;
        staged.ensure_ok("git diff --cached --numstat")?;
        unstaged.ensure_ok("git diff --numstat")?;
        let parsed = parse_status_porcelain(&status.stdout);
        let untracked_stats = build_untracked_stats(root, &parsed.entries);
        let summary = RepositorySummary {
            workspace_path: resolution.workspace_path.clone(),
            repo_root: resolution.repo_root.clone(),
            workspace_in_repo_path: resolution.workspace_in_repo_path.clone(),
            auto_refresh_watch_paths: resolution.auto_refresh_watch_paths.clone(),
            branch_name: parsed.branch_name,
            tracking_branch_name: parsed.tracking_branch_name,
            head_ref_type: parsed.head_ref_type,
            ahead: parsed.ahead,
            behind: parsed.behind,
            is_dirty: !parsed.entries.is_empty(),
            is_git_available: true,
            is_repository: true,
        };
        Ok(StatusSnapshot {
            resolution,
            summary,
            entries: parsed.entries,
            staged_stats: parse_numstat(&staged.stdout),
            unstaged_stats: parse_numstat(&unstaged.stdout),
            untracked_stats,
        })
    }

    /// `normalizeInputPath`.
    ///
    /// # Errors
    /// `GitError::App("Path is outside repository scope: <path>")`.
    pub fn normalize_input_path(resolution: &Resolution, path: &str) -> Result<String, GitError> {
        let raw = resolve_input_path(&resolution.workspace_path, path);
        let absolute = harness_fs::realpath(&raw).unwrap_or(raw);
        let relative = normalize_git_path(&node_relative(&resolution.repo_root, &absolute));
        if relative.is_empty() || relative == "." || relative == ".." || relative.starts_with("../")
        {
            return Err(GitError::App(format!(
                "Path is outside repository scope: {path}"
            )));
        }
        Ok(relative)
    }

    /// `getIgnoredPaths`.
    ///
    /// # Errors
    /// `GitError::App` when `git check-ignore` fails (exit other than 0/1).
    pub fn ignored_paths(
        &self,
        workspace_path: &str,
        paths: &[String],
        cancel: CancelFn<'_>,
    ) -> Result<Vec<String>, GitError> {
        if paths.is_empty() {
            return Ok(Vec::new());
        }
        let resolution = self.resolve_repository(workspace_path, cancel)?;
        if !resolution.is_usable() {
            return Ok(Vec::new());
        }
        let pairs: Vec<(String, String)> = paths
            .iter()
            .filter_map(|path| {
                let relative = Self::normalize_input_path(&resolution, path).ok()?;
                Some((
                    resolve_input_path(&resolution.workspace_path, path),
                    relative,
                ))
            })
            .collect();
        if pairs.is_empty() {
            return Ok(Vec::new());
        }
        let mut args: Vec<&str> = vec!["check-ignore", "--"];
        args.extend(pairs.iter().map(|(_, relative)| relative.as_str()));
        let result = self.git(&resolution.repo_root, &args, cancel)?;
        if result.exit_code == Some(1) {
            return Ok(Vec::new());
        }
        result.ensure_ok("git check-ignore")?;
        let ignored: HashSet<String> = result
            .stdout
            .split('\n')
            .map(|line| line.strip_suffix('\r').unwrap_or(line))
            .filter(|line| !line.is_empty())
            .map(normalize_git_path)
            .collect();
        Ok(pairs
            .into_iter()
            .filter(|(_, relative)| ignored.contains(relative))
            .map(|(absolute, _)| absolute)
            .collect())
    }

    /// `listLocalBranches`.
    ///
    /// # Errors
    /// Status or `git for-each-ref` failures.
    pub fn local_branches(
        &self,
        workspace_path: &str,
        cancel: CancelFn<'_>,
    ) -> Result<LocalBranchList, GitError> {
        let resolution = self.resolve_repository(workspace_path, cancel)?;
        if !resolution.is_usable() {
            let status = self.status_for(resolution, cancel)?;
            return Ok(LocalBranchList {
                head_ref_type: status.summary.head_ref_type,
                current_branch_name: status.summary.branch_name,
                branches: Vec::new(),
            });
        }
        // 性能：`for-each-ref` 只依赖 repoRoot，与 status 的三条命令互不依赖，并发执行；
        // 错误优先级保持 TS 顺序（先 status，再 for-each-ref），输出不变。
        let root = resolution.repo_root.clone();
        let (status, result) = thread::scope(|scope| {
            let refs = scope.spawn(|| {
                self.git(
                    &root,
                    &[
                        "for-each-ref",
                        "refs/heads",
                        "--format=%(refname:short)%00%(upstream:short)%00%(objectname)%00%(committerdate:unix)",
                    ],
                    cancel,
                )
            });
            let status = self.status_for(resolution, cancel);
            (status, join(refs))
        });
        let status = status?;
        let head_ref_type = status.summary.head_ref_type;
        let current_branch_name = status.summary.branch_name.clone();
        let result = result?;
        result.ensure_ok("git for-each-ref refs/heads")?;
        let current = if head_ref_type == HeadRefType::Branch {
            current_branch_name.as_deref()
        } else {
            None
        };
        let branches = parse_branch_refs(&result.stdout, current);
        Ok(LocalBranchList {
            head_ref_type,
            current_branch_name,
            branches,
        })
    }

    /// `getCommitGraph`.
    ///
    /// # Errors
    /// `git log` failures other than "no commits yet".
    pub fn commit_graph(
        &self,
        workspace_path: &str,
        max_count: Option<f64>,
        skip: Option<f64>,
        cancel: CancelFn<'_>,
    ) -> Result<CommitGraph, GitError> {
        let empty = CommitGraph {
            commits: Vec::new(),
            has_more: false,
        };
        let resolution = self.resolve_repository(workspace_path, cancel)?;
        if !resolution.is_usable() {
            return Ok(empty);
        }
        let max_count = normalize_graph_max_count(max_count);
        let skip_arg = format!("--skip={}", normalize_graph_skip(skip));
        let max_arg = format!("--max-count={}", max_count + 1);
        let result = self.git(
            &resolution.repo_root,
            &[
                "log",
                "HEAD",
                "--branches",
                "--tags",
                "--remotes",
                "--date-order",
                "--topo-order",
                &skip_arg,
                &max_arg,
                "--format=%H%x00%P%x00%an%x00%at%x00%s%x00%D%x1e",
            ],
            cancel,
        )?;
        if result.exit_code != Some(0) {
            let stderr = result.stderr.to_lowercase();
            if stderr.contains("does not have any commits yet")
                || stderr.contains("your current branch")
                || stderr.contains("bad default revision")
                || stderr.contains("ambiguous argument 'head'")
            {
                return Ok(empty);
            }
            result.ensure_ok("git log visible refs")?;
        }
        let mut commits = parse_graph_records(&result.stdout);
        let limit = usize::try_from(max_count).unwrap_or(usize::MAX);
        let has_more = commits.len() > limit;
        commits.truncate(limit);
        Ok(CommitGraph { commits, has_more })
    }

    /// `getBranchComparison` on an existing status snapshot (the TS in-flight
    /// reuse makes `refresh` share one snapshot; callers pass it explicitly).
    ///
    /// # Errors
    /// `git diff --numstat upstream...HEAD` failures.
    pub fn branch_comparison(
        &self,
        status: &StatusSnapshot,
        cancel: CancelFn<'_>,
    ) -> Result<BranchComparisonSnapshot, GitError> {
        let tracking = status.summary.tracking_branch_name.clone();
        let head_ref = Some(
            status
                .summary
                .branch_name
                .clone()
                .unwrap_or_else(|| "HEAD".to_owned()),
        );
        let Some(tracking_name) = tracking.clone().filter(|_| status.resolution.is_usable()) else {
            return Ok(BranchComparisonSnapshot {
                resolution: status.resolution.clone(),
                base_ref: tracking,
                head_ref,
                comparison_label: None,
                changes: Vec::new(),
            });
        };
        let range = format!("{tracking_name}...HEAD");
        let result = self.git(
            &status.resolution.repo_root,
            &["diff", "--numstat", "-z", "--find-renames", &range, "--"],
            cancel,
        )?;
        result.ensure_ok("git diff --numstat upstream...HEAD")?;
        let changes = parse_numstat(&result.stdout)
            .iter()
            .map(|(path, stat)| BranchChange {
                path: path.clone(),
                original_path: stat.original_path.clone(),
                kind: kind_from_numstat(stat),
                added: stat.added,
                removed: stat.removed,
            })
            .collect();
        let label = match &status.summary.branch_name {
            Some(branch) => format!("{branch} -> {tracking_name}"),
            None => format!("HEAD -> {tracking_name}"),
        };
        Ok(BranchComparisonSnapshot {
            resolution: status.resolution.clone(),
            base_ref: Some(tracking_name),
            head_ref,
            comparison_label: Some(label),
            changes,
        })
    }

    fn config_value(
        &self,
        root: &str,
        key: &str,
        cancel: CancelFn<'_>,
    ) -> Result<ConfigValue, GitError> {
        let result = self.git(
            root,
            &["config", "--show-scope", "--show-origin", "--get", key],
            cancel,
        )?;
        if result.exit_code == Some(1) {
            return Ok(ConfigValue::default());
        }
        result.ensure_ok("git config")?;
        Ok(parse_config_value(&result.stdout))
    }

    /// `getIdentity`.
    ///
    /// # Errors
    /// `git config` failures other than "unset" (exit 1).
    pub fn identity(
        &self,
        workspace_path: &str,
        cancel: CancelFn<'_>,
    ) -> Result<Identity, GitError> {
        let resolution = self.resolve_repository(workspace_path, cancel)?;
        self.identity_for(&resolution, cancel)
    }

    /// `getIdentity` on an already resolved repository.
    ///
    /// # Errors
    /// See [`GitRepo::identity`].
    pub fn identity_for(
        &self,
        resolution: &Resolution,
        cancel: CancelFn<'_>,
    ) -> Result<Identity, GitError> {
        if !resolution.is_usable() {
            return Ok(Identity::default());
        }
        let root = resolution.repo_root.as_str();
        let (name, email) = thread::scope(|scope| {
            let email = scope.spawn(|| self.config_value(root, "user.email", cancel));
            let name = self.config_value(root, "user.name", cancel);
            (name, join(email))
        });
        let (name, email) = (name?, email?);
        Ok(Identity {
            user_name: name.value,
            user_email: email.value,
            name_source: name.source,
            email_source: email.source,
            scope_label: name.scope.or(email.scope),
        })
    }
}

/// Joins a scoped worker, mapping a panic to an internal app error.
pub(crate) fn join<T>(
    handle: thread::ScopedJoinHandle<'_, Result<T, GitError>>,
) -> Result<T, GitError> {
    handle
        .join()
        .unwrap_or_else(|_| Err(GitError::App("git worker panicked".to_owned())))
}
