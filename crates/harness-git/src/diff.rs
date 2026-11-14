//! `getDiff` and its content helpers (`gitCliRepo.ts`, `gitCliHelpers.ts`).

use std::fs::File;
use std::io::Read;
use std::thread;

use crate::error::GitError;
use crate::repo::{join, GitRepo, Resolution};
use crate::runner::{CancelFn, ExecResult, DIFF_BYTES, DIFF_TIMEOUT};
use crate::types::{DiffAvailability, DiffResult};
use crate::util::{js_trim, normalize_git_path};

const NULL_DEVICE: &str = if cfg!(windows) { "NUL" } else { "/dev/null" };
const BINARY_SUMMARY: &str = "Binary diff is not previewable.";
const TRUNCATED_SUMMARY: &str = "Git diff output exceeded the preview limit.";

/// `GitDiffQuery` fields used by the read path.
#[derive(Debug, Clone, Default)]
pub struct DiffQuery {
    /// Workspace path.
    pub workspace_path: String,
    /// File path (absolute or workspace-relative with `/`).
    pub path: String,
    /// Explicit staged flag.
    pub staged: Option<bool>,
    /// `GitChangeSourceId`.
    pub source_id: Option<String>,
}

fn panicked() -> GitError {
    GitError::App("git worker panicked".to_owned())
}

/// Runs `a` on a scoped worker and `b` on the current thread.
fn both<'scope, A, B>(
    scope: &'scope thread::Scope<'scope, '_>,
    a: impl FnOnce() -> Result<A, GitError> + Send + 'scope,
    b: impl FnOnce() -> Result<B, GitError>,
) -> (Result<A, GitError>, Result<B, GitError>)
where
    A: Send + 'scope,
{
    let a = scope.spawn(a);
    let b = b();
    (join(a), b)
}

fn is_binary_diff(stdout: &str) -> bool {
    stdout.contains("GIT binary patch") || stdout.contains("Binary files ")
}

/// `toDiffResult`.
fn to_diff_result(
    path: &str,
    result: &ExecResult,
    allowed: &[i32],
    empty_summary: &str,
) -> DiffResult {
    if result.timed_out {
        return DiffResult::unavailable(path, "Git diff command timed out.");
    }
    if result.output_truncated {
        return DiffResult::without_patch(path, DiffAvailability::Truncated, TRUNCATED_SUMMARY);
    }
    if !result.exit_code.is_some_and(|code| allowed.contains(&code)) {
        return DiffResult::unavailable(path, result.message());
    }
    if js_trim(&result.stdout).is_empty() {
        return DiffResult::unavailable(path, empty_summary);
    }
    if is_binary_diff(&result.stdout) {
        return DiffResult::without_patch(path, DiffAvailability::Binary, BINARY_SUMMARY);
    }
    DiffResult {
        path: path.to_owned(),
        availability: DiffAvailability::Patch,
        patch: Some(result.stdout.clone()),
        before_content: None,
        after_content: None,
        summary: None,
    }
}

/// `withDiffContents`: full contents only when both sides were readable.
fn with_contents(mut diff: DiffResult, contents: Option<(String, String)>) -> DiffResult {
    if diff.availability != DiffAvailability::Patch {
        return diff;
    }
    if let Some((before, after)) = contents {
        diff.before_content = Some(before);
        diff.after_content = Some(after);
    }
    diff
}

/// `readWorkingTreePreviewContent`.
fn read_working_tree(absolute_path: &str) -> Option<String> {
    let meta = std::fs::metadata(absolute_path).ok()?;
    if !meta.is_file() || meta.len() > DIFF_BYTES as u64 {
        return None;
    }
    let bytes = std::fs::read(absolute_path).ok()?;
    let content = String::from_utf8_lossy(&bytes).into_owned();
    (!content.contains('\0')).then_some(content)
}

/// `buildUntrackedTextDiffResult`; `None` when the file cannot be read.
fn untracked_text_diff(absolute_path: &str, repo_relative_path: &str) -> Option<DiffResult> {
    // 流式读取：先完整扫描 NUL（与 TS 一样二进制优先于超限判断），超限后不再保留内容，避免大文件占用内存。
    let mut file = File::open(absolute_path).ok()?;
    let mut content: Vec<u8> = Vec::new();
    let mut total: usize = 0;
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf).ok()?;
        if n == 0 {
            break;
        }
        if buf[..n].contains(&0) {
            return Some(DiffResult::without_patch(
                absolute_path,
                DiffAvailability::Binary,
                BINARY_SUMMARY,
            ));
        }
        total += n;
        if total <= DIFF_BYTES {
            content.extend_from_slice(&buf[..n]);
        } else {
            content = Vec::new();
        }
    }
    if total > DIFF_BYTES {
        return Some(DiffResult::without_patch(
            absolute_path,
            DiffAvailability::Truncated,
            TRUNCATED_SUMMARY,
        ));
    }
    let text = String::from_utf8_lossy(&content).into_owned();
    let normalized = text.replace("\r\n", "\n");
    let mut patch_lines: Vec<String> = vec![
        "--- /dev/null".to_owned(),
        format!("+++ b/{}", normalize_git_path(repo_relative_path)),
    ];
    if !normalized.is_empty() {
        let has_trailing_newline = normalized.ends_with('\n');
        let mut lines: Vec<&str> = normalized.split('\n').collect();
        if has_trailing_newline {
            lines.pop();
        }
        if !lines.is_empty() {
            patch_lines.push(format!("@@ -0,0 +1,{} @@", lines.len()));
            patch_lines.extend(lines.iter().map(|line| format!("+{line}")));
            if !has_trailing_newline {
                patch_lines.push("\\ No newline at end of file".to_owned());
            }
        }
    }
    Some(DiffResult {
        path: absolute_path.to_owned(),
        availability: DiffAvailability::Patch,
        patch: Some(format!("{}\n", patch_lines.join("\n"))),
        before_content: Some(String::new()),
        after_content: Some(text),
        summary: None,
    })
}

impl GitRepo {
    fn diff_git(
        &self,
        cwd: &str,
        args: &[&str],
        cancel: CancelFn<'_>,
    ) -> Result<ExecResult, GitError> {
        self.runner.run(cwd, args, DIFF_TIMEOUT, DIFF_BYTES, cancel)
    }

    /// `readGitBlobPreviewContent` (`git show <ref>:<path>`).
    fn blob(
        &self,
        root: &str,
        reference: &str,
        path: &str,
        cancel: CancelFn<'_>,
    ) -> Result<Option<String>, GitError> {
        let spec = format!("{reference}:{path}");
        let result = self.diff_git(root, &["show", &spec], cancel)?;
        if result.timed_out
            || result.output_truncated
            || result.exit_code != Some(0)
            || result.stdout.contains('\0')
        {
            return Ok(None);
        }
        Ok(Some(result.stdout))
    }

    fn pair(before: Option<String>, after: Option<String>) -> Option<(String, String)> {
        Some((before?, after?))
    }

    /// `getDiff`.
    ///
    /// # Errors
    /// `Path is outside repository scope: <path>` and resolution failures.
    pub fn diff(&self, query: &DiffQuery, cancel: CancelFn<'_>) -> Result<DiffResult, GitError> {
        let resolution = self.resolve_repository(&query.workspace_path, cancel)?;
        let absolute = crate::repo::resolve_input_path(&query.workspace_path, &query.path);
        if !resolution.is_git_available {
            return Ok(DiffResult::unavailable(
                &absolute,
                "Git binary is not available in the current environment.",
            ));
        }
        if !resolution.is_repository {
            return Ok(DiffResult::unavailable(
                &absolute,
                "Workspace is not inside a Git repository.",
            ));
        }
        let relative = Self::normalize_input_path(&resolution, &query.path)?;
        let root = resolution.repo_root.as_str();
        if query.source_id.as_deref() == Some("branch") {
            return self.branch_diff(&resolution, &absolute, &relative, cancel);
        }
        let staged = query
            .staged
            .unwrap_or(query.source_id.as_deref() == Some("staged"));
        let args: &[&str] = if staged {
            &[
                "diff",
                "--cached",
                "--no-ext-diff",
                "--no-color",
                "--binary",
                "--",
                &relative,
            ]
        } else {
            &[
                "diff",
                "--no-ext-diff",
                "--no-color",
                "--binary",
                "--",
                &relative,
            ]
        };
        // 性能：TS 依次执行 diff 与 `git show` 读取全文；这些命令互不依赖（只读，且 blob 失败只会
        // 返回 None），这里并发执行。全文只在 patch 可用时被采用（with_contents），输出与 TS 一致。
        let (result, contents) = thread::scope(|scope| {
            let contents = scope.spawn(|| -> Result<_, GitError> {
                Ok(if staged {
                    let (before, after) = both(
                        scope,
                        || self.blob(root, "HEAD", &relative, cancel),
                        || self.blob(root, "", &relative, cancel),
                    );
                    Self::pair(before?, after?)
                } else {
                    Self::pair(
                        self.blob(root, "", &relative, cancel)?,
                        read_working_tree(&absolute),
                    )
                })
            });
            let result = self.diff_git(root, args, cancel);
            (result, join(contents))
        });
        let parsed = to_diff_result(
            &absolute,
            &result?,
            &[0],
            "No Git diff is available for this file.",
        );
        if parsed.availability != DiffAvailability::Unavailable || staged {
            return Ok(with_contents(parsed, contents?));
        }
        if std::fs::metadata(&absolute).is_err() {
            return Ok(parsed);
        }
        if let Some(untracked) = untracked_text_diff(&absolute, &relative) {
            return Ok(untracked);
        }
        let result = self.diff_git(
            root,
            &[
                "diff",
                "--no-index",
                "--no-ext-diff",
                "--no-color",
                "--binary",
                NULL_DEVICE,
                &absolute,
            ],
            cancel,
        )?;
        Ok(to_diff_result(
            &absolute,
            &result,
            &[0, 1],
            "No previewable diff is available for this file.",
        ))
    }

    fn branch_diff(
        &self,
        resolution: &Resolution,
        absolute: &str,
        relative: &str,
        cancel: CancelFn<'_>,
    ) -> Result<DiffResult, GitError> {
        // 复用 diff 入口已完成的仓库解析，不再为 status 重跑一次 `git rev-parse`。
        let status = self.status_for(resolution.clone(), cancel)?;
        let Some(tracking) = status.summary.tracking_branch_name else {
            return Ok(DiffResult::unavailable(
                absolute,
                "Current branch does not have an upstream branch.",
            ));
        };
        let root = resolution.repo_root.as_str();
        let range = format!("{tracking}...HEAD");
        // 性能：range diff、merge-base 与 HEAD 侧 blob 互不依赖，并发执行；只有基线 blob 依赖
        // merge-base 结果。各命令的参数与 TS 完全一致。
        let (result, (merge_base, after)) = thread::scope(|scope| {
            let rest = scope.spawn(|| {
                both(
                    scope,
                    || self.git(root, &["merge-base", &tracking, "HEAD"], cancel),
                    || self.blob(root, "HEAD", relative, cancel),
                )
            });
            let result = self.diff_git(
                root,
                &[
                    "diff",
                    "--no-ext-diff",
                    "--no-color",
                    "--binary",
                    "--find-renames",
                    &range,
                    "--",
                    relative,
                ],
                cancel,
            );
            let rest = rest
                .join()
                .unwrap_or_else(|_| (Err(panicked()), Err(panicked())));
            (result, rest)
        });
        let parsed = to_diff_result(
            absolute,
            &result?,
            &[0],
            "No branch comparison diff is available for this file.",
        );
        let merge_base = merge_base?;
        let merge_base = if merge_base.exit_code == Some(0) {
            js_trim(&merge_base.stdout).to_owned()
        } else {
            String::new()
        };
        let base_ref = if merge_base.is_empty() {
            tracking.as_str()
        } else {
            merge_base.as_str()
        };
        let before = self.blob(root, base_ref, relative, cancel)?;
        let after = after?;
        Ok(with_contents(parsed, Self::pair(before, after)))
    }
}
