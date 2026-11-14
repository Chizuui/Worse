//! Git tree snapshots of a workspace around a turn (spec phase-8-core.md §15.2).
//!
//! A snapshot stages the whole worktree into a *temporary copy* of the repository index and writes it as a
//! tree object (`git add -A` + `git write-tree` under `GIT_INDEX_FILE`). The user's index, worktree, HEAD and
//! refs are never modified; only blob/tree objects are added, as `git stash create` does.

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use harness_core_api::diff::{parse_unified, FileDiff};
use tokio::io::AsyncReadExt;
use tokio::process::Command;

use crate::util::new_id;

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Upper bound of one git invocation (a stuck git must not hold the session actor forever).
const GIT_LIMIT: Duration = Duration::from_secs(120);
/// Turn diffs larger than this are cut (`truncated`).
pub(crate) const DIFF_CAP: usize = 2 * 1024 * 1024;

struct Output {
    status_ok: bool,
    stdout: Vec<u8>,
    truncated: bool,
    stderr: String,
}

async fn git(
    cwd: &Path,
    args: &[&str],
    index_file: Option<&Path>,
    cap: usize,
) -> Result<Output, String> {
    let mut command = Command::new("git");
    command
        .args(["-c", "core.quotepath=false", "-c", "core.autocrlf=false"])
        .args(args)
        .current_dir(cwd)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    if let Some(index) = index_file {
        command.env("GIT_INDEX_FILE", index);
    }
    #[cfg(windows)]
    command.creation_flags(CREATE_NO_WINDOW);
    let mut child = command.spawn().map_err(|e| format!("git: {e}"))?;
    let mut stdout = child.stdout.take().ok_or("git: no stdout")?;
    let mut stderr = child.stderr.take().ok_or("git: no stderr")?;
    let run = async {
        let mut out = Vec::new();
        let mut truncated = false;
        let mut buffer = vec![0u8; 64 * 1024];
        let read_out = async {
            loop {
                let n = stdout.read(&mut buffer).await.map_err(|e| e.to_string())?;
                if n == 0 {
                    break;
                }
                let room = cap.saturating_sub(out.len());
                out.extend_from_slice(&buffer[..n.min(room)]);
                if n > room {
                    truncated = true;
                }
            }
            Ok::<(), String>(())
        };
        let mut err = Vec::new();
        let read_err = stderr.read_to_end(&mut err);
        let (out_result, err_result) = tokio::join!(read_out, read_err);
        out_result?;
        err_result.map_err(|e| e.to_string())?;
        let status = child.wait().await.map_err(|e| e.to_string())?;
        Ok::<Output, String>(Output {
            status_ok: status.success(),
            stdout: out,
            truncated,
            stderr: String::from_utf8_lossy(&err).trim().to_owned(),
        })
    };
    tokio::time::timeout(GIT_LIMIT, run)
        .await
        .map_err(|_| format!("git {} did not finish in time", args.join(" ")))?
}

pub(crate) async fn git_text(cwd: &Path, args: &[&str]) -> Result<String, String> {
    let out = git(cwd, args, None, 64 * 1024).await?;
    if !out.status_ok {
        return Err(out.stderr);
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

/// Snapshot of `workspace` as a git tree id; `Ok(None)` when it is not inside a git work tree (or git is
/// missing). `tmp_dir` holds the temporary index copy (removed again).
pub(crate) async fn take(tmp_dir: &Path, workspace: &Path) -> Result<Option<String>, String> {
    let Ok(inside) = git_text(workspace, &["rev-parse", "--is-inside-work-tree"]).await else {
        return Ok(None);
    };
    if inside != "true" {
        return Ok(None);
    }
    let index = git_text(workspace, &["rev-parse", "--git-path", "index"]).await?;
    let index = workspace.join(index);
    tokio::fs::create_dir_all(tmp_dir)
        .await
        .map_err(|e| format!("{}: {e}", tmp_dir.display()))?;
    let temp = tmp_dir.join(new_id("index"));
    // 复制用户索引而不是直接使用：快照不得改动用户的暂存区（spec §15.2）；复制件保留 stat 缓存，add 更快。
    match tokio::fs::copy(&index, &temp).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(format!("{}: {e}", index.display())),
    }
    let result = async {
        let add = git(workspace, &["add", "-A", "--", "."], Some(&temp), 64 * 1024).await?;
        if !add.status_ok {
            return Err(format!("git add: {}", add.stderr));
        }
        let tree = git(workspace, &["write-tree"], Some(&temp), 1024).await?;
        if !tree.status_ok {
            return Err(format!("git write-tree: {}", tree.stderr));
        }
        Ok(String::from_utf8_lossy(&tree.stdout).trim().to_owned())
    }
    .await;
    let _ = tokio::fs::remove_file(&temp).await;
    result.map(Some)
}

/// Whether `tree` still exists in the repository of `workspace`.
pub(crate) async fn exists(workspace: &Path, tree: &str) -> bool {
    let spec = format!("{tree}^{{tree}}");
    git(workspace, &["cat-file", "-e", &spec], None, 1024)
        .await
        .is_ok_and(|o| o.status_ok)
}

/// Structured diff `before → after`, limited to the workspace directory. Returns `(files, truncated)`.
pub(crate) async fn diff(
    workspace: &Path,
    before: &str,
    after: &str,
) -> Result<(Vec<FileDiff>, bool), String> {
    // 树对象路径相对仓库根；`--relative` 让输出路径相对工作区目录，与 workspace 服务一致。
    let out = git(
        workspace,
        &[
            "diff",
            "--no-color",
            "--no-ext-diff",
            "-M",
            "--relative",
            before,
            after,
            "--",
            ".",
        ],
        None,
        DIFF_CAP,
    )
    .await?;
    if !out.status_ok {
        return Err(format!("git diff: {}", out.stderr));
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let mut files = parse_unified(&text);
    if out.truncated {
        // 截断处可能留下不完整的最后一个文件，去掉以免显示半个 hunk。
        files.pop();
    }
    Ok((files, out.truncated))
}

// ---- file undo / reapply (phase-9-parity.md §9.2.1 file rules) ---------------------------------------

/// Largest file restored from a snapshot blob.
const BLOB_CAP: usize = 256 * 1024 * 1024;

/// Workspace-relative paths that differ between two trees (`--no-renames`, so a rename is delete + add).
pub(crate) async fn changed_paths(
    workspace: &Path,
    from: &str,
    to: &str,
) -> Result<Vec<String>, String> {
    let out = git(
        workspace,
        &[
            "diff",
            "--name-only",
            "--no-renames",
            "--relative",
            "-z",
            from,
            to,
            "--",
            ".",
        ],
        None,
        DIFF_CAP,
    )
    .await?;
    if !out.status_ok {
        return Err(format!("git diff: {}", out.stderr));
    }
    Ok(out
        .stdout
        .split(|b| *b == 0)
        .filter(|p| !p.is_empty())
        .map(|p| String::from_utf8_lossy(p).into_owned())
        .collect())
}

/// Blob id of `path` (relative to the workspace) in `tree`; `None` when the path is not in it.
pub(crate) async fn blob_of(
    workspace: &Path,
    tree: &str,
    path: &str,
) -> Result<Option<String>, String> {
    let spec = format!("{tree}:./{path}");
    let out = git(
        workspace,
        &["rev-parse", "--verify", "--quiet", &spec],
        None,
        1024,
    )
    .await?;
    if !out.status_ok {
        return Ok(None);
    }
    let id = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    Ok((!id.is_empty()).then_some(id))
}

/// Blob id the workspace file would get (`git hash-object`, same filters as the snapshot's `git add`);
/// `None` when the file does not exist.
pub(crate) async fn hash_file(workspace: &Path, path: &str) -> Result<Option<String>, String> {
    match tokio::fs::metadata(workspace.join(path)).await {
        Ok(meta) if meta.is_file() => {}
        Ok(_) => return Ok(Some(String::new())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("{path}: {e}")),
    }
    let out = git(workspace, &["hash-object", "--", path], None, 1024).await?;
    if !out.status_ok {
        return Err(format!("git hash-object: {}", out.stderr));
    }
    Ok(Some(String::from_utf8_lossy(&out.stdout).trim().to_owned()))
}

/// Content of a blob.
pub(crate) async fn read_blob(workspace: &Path, blob: &str) -> Result<Vec<u8>, String> {
    let out = git(workspace, &["cat-file", "blob", blob], None, BLOB_CAP).await?;
    if !out.status_ok {
        return Err(format!("git cat-file: {}", out.stderr));
    }
    if out.truncated {
        return Err("file too large to restore".into());
    }
    Ok(out.stdout)
}
