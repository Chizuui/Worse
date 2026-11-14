//! Data dir resolution and relocation (spec §9.1.2, decision 17 = A).
//!
//! Base dir = `HARNESS_HOME` or `~/.harness`. The data dir is the base dir unless `<base>/relocated.json` points to a
//! directory holding the `.harness-data` marker. Relocation copies, verifies byte for byte, then switches the pointer
//! atomically; the old files are deleted only after the switch, from a manifest that survives interruption.
//! It must run while the hub is stopped: the UI schedules it ([`schedule`]) and [`apply_pending`] runs at the next
//! startup before the hub opens the store.

use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};
use tokio::io::AsyncReadExt;

use crate::error::SettingsError;
use crate::fsx;

/// Pointer file in the base dir.
pub const POINTER_FILE: &str = "relocated.json";
/// Pending relocation request in the base dir.
pub const PENDING_FILE: &str = "relocate.pending.json";
/// Cleanup manifest in the base dir (old files still to delete after a switch).
pub const CLEANUP_FILE: &str = "relocated.cleanup.json";
/// Marker of a complete, verified data dir.
pub const DATA_MARKER: &str = ".harness-data";
/// Marker of an unfinished copy.
pub const COPY_MARKER: &str = ".harness-relocating";

/// Files of the base dir that belong to the relocation machinery, never copied.
const CONTROL_FILES: &[&str] = &[
    POINTER_FILE,
    PENDING_FILE,
    CLEANUP_FILE,
    COPY_MARKER,
    DATA_MARKER,
];

/// Result of [`resolve_data_dir`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DataDirInfo {
    pub base: PathBuf,
    pub data_dir: PathBuf,
    /// True when `data_dir` comes from the pointer.
    pub relocated: bool,
}

/// What a relocation did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RelocationReport {
    pub from: PathBuf,
    pub to: PathBuf,
    pub files_copied: usize,
    pub bytes_copied: u64,
    /// Credential-like files left in the old dir (never opened, see `fsx::is_forbidden`).
    pub skipped: Vec<PathBuf>,
    /// False when deleting old files failed; it resumes on the next [`resolve_data_dir`].
    pub cleanup_complete: bool,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Pointer {
    data_dir: PathBuf,
    #[serde(default)]
    moved_at: u64,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Pending {
    target: PathBuf,
    #[serde(default)]
    requested_at: u64,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Cleanup {
    from: PathBuf,
    files: Vec<PathBuf>,
    remove_dir: bool,
}

/// Test/gate hook: stop a relocation after a step, as if the process died there.
#[doc(hidden)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailPoint {
    AfterCopy,
    AfterVerify,
    AfterSwitch,
}

/// Default base dir: `HARNESS_HOME`, else `<home>/.harness`.
#[must_use]
pub fn default_base_dir() -> Option<PathBuf> {
    if let Some(home) = std::env::var_os("HARNESS_HOME").filter(|v| !v.is_empty()) {
        return Some(PathBuf::from(home));
    }
    let keys: &[&str] = if cfg!(windows) {
        &["USERPROFILE", "HOME"]
    } else {
        &["HOME"]
    };
    keys.iter()
        .filter_map(std::env::var_os)
        .find(|v| !v.is_empty())
        .map(|h| PathBuf::from(h).join(".harness"))
}

/// Resolves the data dir for `base` and resumes an interrupted cleanup. Never fails: a broken pointer falls back to
/// the base dir (warning).
pub async fn resolve_data_dir(base: &Path) -> DataDirInfo {
    let mut info = DataDirInfo {
        base: base.to_path_buf(),
        data_dir: base.to_path_buf(),
        relocated: false,
    };
    match read_json::<Pointer>(&base.join(POINTER_FILE)).await {
        Ok(Some(pointer)) => {
            if fsx::exists(&pointer.data_dir.join(DATA_MARKER)).await {
                info.data_dir = pointer.data_dir;
                info.relocated = true;
            } else {
                tracing::warn!(target = %pointer.data_dir.display(), "relocated data dir lost its marker; using base dir");
            }
        }
        Ok(None) => {}
        Err(e) => tracing::warn!(error = %e, "relocated.json unreadable; using base dir"),
    }
    if let Err(e) = resume_cleanup(base, &info.data_dir).await {
        tracing::warn!(error = %e, "old data dir cleanup failed; will retry");
    }
    info
}

/// Validates `target` and records it as the pending relocation (applied by [`apply_pending`] at next start).
///
/// # Errors
///
/// Invalid target (relative, nested, non-empty) or I/O failure.
pub async fn schedule(base: &Path, target: &Path) -> Result<(), SettingsError> {
    let current = resolve_data_dir(base).await.data_dir;
    validate(base, &current, target).await?;
    let pending = Pending {
        target: absolute(target)?,
        requested_at: now_ms(),
    };
    write_json(&base.join(PENDING_FILE), &pending).await
}

/// The pending relocation target, if any.
pub async fn pending(base: &Path) -> Option<PathBuf> {
    read_json::<Pending>(&base.join(PENDING_FILE))
        .await
        .ok()
        .flatten()
        .map(|p| p.target)
}

/// Drops a pending relocation.
///
/// # Errors
///
/// I/O failure.
pub async fn cancel_pending(base: &Path) -> Result<(), SettingsError> {
    Ok(fsx::remove_file_opt(&base.join(PENDING_FILE)).await?)
}

/// Runs the pending relocation, if any. Call at startup before anything opens files in the data dir. The request is
/// consumed whether it succeeds or fails (a failing move is not retried on every start); the old dir stays in use
/// on failure.
///
/// # Errors
///
/// The relocation failed (nothing switched).
pub async fn apply_pending(base: &Path) -> Result<Option<RelocationReport>, SettingsError> {
    let Some(target) = pending(base).await else {
        return Ok(None);
    };
    let current = resolve_data_dir(base).await.data_dir;
    let result = relocate(base, &current, &target).await;
    cancel_pending(base).await?;
    result.map(Some)
}

/// Moves the data dir from `current` to `target` (copy → verify → marker → switch pointer → delete old files).
/// The hub must be stopped.
///
/// # Errors
///
/// Invalid target, copy or verification failure. In every error case the pointer is unchanged.
pub async fn relocate(
    base: &Path,
    current: &Path,
    target: &Path,
) -> Result<RelocationReport, SettingsError> {
    relocate_with_fail_point(base, current, target, None).await
}

/// [`relocate`] with an injected interruption (tests and the phase 9 gate).
#[doc(hidden)]
pub async fn relocate_with_fail_point(
    base: &Path,
    current: &Path,
    target: &Path,
    fail_at: Option<FailPoint>,
) -> Result<RelocationReport, SettingsError> {
    let target = absolute(target)?;
    let current = absolute(current)?;
    let base = absolute(base)?;
    validate(&base, &current, &target).await?;
    let target_is_base = same_path(&target, &base);

    // 1. Unfinished-copy marker (a retry after an interruption clears our own partial copy first).
    if !target_is_base {
        if fsx::exists(&target.join(COPY_MARKER)).await {
            clear_dir(&target).await?;
        }
        tokio::fs::create_dir_all(&target).await?;
        fsx::write_atomic(&target.join(COPY_MARKER), now_ms().to_string().as_bytes()).await?;
    }

    // 2. Copy.
    let (files, skipped) = list_files(&current).await?;
    let mut bytes_copied = 0;
    for rel in &files {
        let to = target.join(rel);
        if let Some(parent) = to.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        bytes_copied += tokio::fs::copy(current.join(rel), &to).await?;
    }
    interrupt(fail_at, FailPoint::AfterCopy)?;

    // 3. Verify byte for byte.
    for rel in &files {
        if !same_bytes(&current.join(rel), &target.join(rel)).await? {
            return Err(relocation_error(format!(
                "verification failed for {}",
                rel.display()
            )));
        }
    }
    interrupt(fail_at, FailPoint::AfterVerify)?;

    // 4. Marker of a complete data dir.
    if !target_is_base {
        fsx::write_atomic(&target.join(DATA_MARKER), now_ms().to_string().as_bytes()).await?;
        fsx::remove_file_opt(&target.join(COPY_MARKER)).await?;
    }

    // 5. Switch (the only step that changes which dir is in use).
    if target_is_base {
        fsx::remove_file_opt(&base.join(POINTER_FILE)).await?;
    } else {
        let pointer = Pointer {
            data_dir: target.clone(),
            moved_at: now_ms(),
        };
        write_json(&base.join(POINTER_FILE), &pointer).await?;
    }
    tracing::info!(from = %current.display(), to = %target.display(), files = files.len(), "data dir relocated");

    // 6. Cleanup of the old copy, resumable via the manifest.
    let mut old_files = files;
    let current_is_base = same_path(&current, &base);
    if !current_is_base {
        old_files.push(PathBuf::from(DATA_MARKER));
    }
    let cleanup = Cleanup {
        from: current.clone(),
        files: old_files,
        remove_dir: !current_is_base,
    };
    write_json(&base.join(CLEANUP_FILE), &cleanup).await?;
    interrupt(fail_at, FailPoint::AfterSwitch)?;
    let cleanup_complete = match resume_cleanup(&base, &target).await {
        Ok(()) => true,
        Err(e) => {
            tracing::warn!(error = %e, "old data dir cleanup failed; will retry");
            false
        }
    };
    Ok(RelocationReport {
        from: current,
        to: target,
        files_copied: cleanup.files.len() - usize::from(!current_is_base),
        bytes_copied,
        skipped,
        cleanup_complete,
    })
}

fn interrupt(fail_at: Option<FailPoint>, here: FailPoint) -> Result<(), SettingsError> {
    if fail_at == Some(here) {
        return Err(relocation_error(format!(
            "interrupted at {here:?} (test hook)"
        )));
    }
    Ok(())
}

async fn validate(base: &Path, current: &Path, target: &Path) -> Result<(), SettingsError> {
    if !target.is_absolute() {
        return Err(relocation_error("target must be an absolute path"));
    }
    let target = absolute(target)?;
    let current = absolute(current)?;
    if same_path(&target, &current) {
        return Err(relocation_error("target is the current data dir"));
    }
    if is_within(&target, &current) || is_within(&current, &target) {
        return Err(relocation_error(
            "target and current data dir must not contain each other",
        ));
    }
    if same_path(&target, base) {
        // Moving back to the base dir: it may only hold the relocation control files.
        let foreign = dir_entries(&target)
            .await?
            .into_iter()
            .filter(|name| !CONTROL_FILES.contains(&name.as_str()))
            .count();
        if foreign > 0 {
            return Err(relocation_error("the base dir is not empty"));
        }
        return Ok(());
    }
    let entries = dir_entries(&target).await?;
    if !entries.is_empty() && !entries.iter().any(|n| n == COPY_MARKER) {
        return Err(relocation_error("target directory is not empty"));
    }
    Ok(())
}

/// Deletes old files listed in the cleanup manifest (unless they are the data dir in use).
async fn resume_cleanup(base: &Path, in_use: &Path) -> Result<(), SettingsError> {
    let manifest = base.join(CLEANUP_FILE);
    let Some(cleanup) = read_json::<Cleanup>(&manifest).await? else {
        return Ok(());
    };
    if same_path(&cleanup.from, in_use) {
        // 指针切换未生效（例如目标标记丢失而回退到旧目录）时，旧目录仍在使用，绝不能按清单删除。
        tracing::warn!("cleanup manifest names the data dir in use; dropped");
        return Ok(fsx::remove_file_opt(&manifest).await?);
    }
    for rel in &cleanup.files {
        if !is_plain_relative(rel) {
            continue;
        }
        let path = cleanup.from.join(rel);
        match tokio::fs::remove_file(&path).await {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e.into()),
            _ => {}
        }
    }
    remove_empty_dirs(&cleanup.from, cleanup.remove_dir).await;
    Ok(fsx::remove_file_opt(&manifest).await?)
}

/// Removes empty sub-directories (deepest first) and, when `include_root`, the root if it ends up empty.
async fn remove_empty_dirs(root: &Path, include_root: bool) {
    let mut dirs = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(mut rd) = tokio::fs::read_dir(&dir).await else {
            continue;
        };
        while let Ok(Some(entry)) = rd.next_entry().await {
            if entry.file_type().await.is_ok_and(|t| t.is_dir()) {
                stack.push(entry.path());
                dirs.push(entry.path());
            }
        }
    }
    dirs.sort_by_key(|d| std::cmp::Reverse(d.components().count()));
    for dir in dirs {
        let _ = tokio::fs::remove_dir(&dir).await;
    }
    if include_root {
        let _ = tokio::fs::remove_dir(root).await;
    }
}

/// Relative paths of all files to copy, and credential-like files that are skipped.
async fn list_files(root: &Path) -> Result<(Vec<PathBuf>, Vec<PathBuf>), SettingsError> {
    let mut files = Vec::new();
    let mut skipped = Vec::new();
    let mut stack = vec![PathBuf::new()];
    while let Some(rel_dir) = stack.pop() {
        let mut rd = tokio::fs::read_dir(root.join(&rel_dir)).await?;
        while let Some(entry) = rd.next_entry().await? {
            let name = entry.file_name().to_string_lossy().into_owned();
            let rel = rel_dir.join(&name);
            let at_root = rel_dir.as_os_str().is_empty();
            if at_root && CONTROL_FILES.contains(&name.as_str()) {
                continue;
            }
            let file_type = entry.file_type().await?;
            if file_type.is_dir() {
                if at_root && name == "tmp" {
                    continue;
                }
                stack.push(rel);
            } else if rel
                .extension()
                .is_some_and(|e| e.eq_ignore_ascii_case("lock"))
                || fsx::is_temp_file(&rel)
            {
                // 锁文件和临时文件不属于数据：既不复制也不删除。
            } else if fsx::is_forbidden(&rel) {
                skipped.push(rel);
            } else {
                files.push(rel);
            }
        }
    }
    files.sort();
    Ok((files, skipped))
}

async fn same_bytes(a: &Path, b: &Path) -> Result<bool, SettingsError> {
    let (mut fa, mut fb) = (
        tokio::fs::File::open(a).await?,
        tokio::fs::File::open(b).await?,
    );
    if fa.metadata().await?.len() != fb.metadata().await?.len() {
        return Ok(false);
    }
    let mut ba = vec![0u8; 64 * 1024];
    let mut bb = vec![0u8; 64 * 1024];
    loop {
        let na = read_full(&mut fa, &mut ba).await?;
        let nb = read_full(&mut fb, &mut bb).await?;
        if na != nb || ba[..na] != bb[..nb] {
            return Ok(false);
        }
        if na == 0 {
            return Ok(true);
        }
    }
}

async fn read_full(file: &mut tokio::fs::File, buf: &mut [u8]) -> std::io::Result<usize> {
    let mut filled = 0;
    while filled < buf.len() {
        let n = file.read(&mut buf[filled..]).await?;
        if n == 0 {
            break;
        }
        filled += n;
    }
    Ok(filled)
}

async fn clear_dir(dir: &Path) -> Result<(), SettingsError> {
    let mut rd = tokio::fs::read_dir(dir).await?;
    while let Some(entry) = rd.next_entry().await? {
        if entry.file_type().await?.is_dir() {
            tokio::fs::remove_dir_all(entry.path()).await?;
        } else {
            tokio::fs::remove_file(entry.path()).await?;
        }
    }
    Ok(())
}

async fn dir_entries(dir: &Path) -> Result<Vec<String>, SettingsError> {
    let mut names = Vec::new();
    match tokio::fs::read_dir(dir).await {
        Ok(mut rd) => {
            while let Some(entry) = rd.next_entry().await? {
                names.push(entry.file_name().to_string_lossy().into_owned());
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    Ok(names)
}

fn absolute(path: &Path) -> Result<PathBuf, SettingsError> {
    let abs = std::path::absolute(path)?;
    // Lexical normalisation (no symlink resolution: the target may not exist yet).
    let mut out = PathBuf::new();
    for component in abs.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    Ok(out)
}

fn path_key(path: &Path) -> String {
    let text = path.to_string_lossy().replace('\\', "/");
    let text = text.trim_end_matches('/').to_owned();
    if cfg!(windows) {
        text.to_lowercase()
    } else {
        text
    }
}

fn same_path(a: &Path, b: &Path) -> bool {
    path_key(a) == path_key(b)
}

/// True when `inner` is strictly inside `outer`.
fn is_within(inner: &Path, outer: &Path) -> bool {
    let (inner, outer) = (path_key(inner), path_key(outer));
    inner.len() > outer.len() && inner.starts_with(&outer) && inner[outer.len()..].starts_with('/')
}

fn is_plain_relative(path: &Path) -> bool {
    path.components().all(|c| matches!(c, Component::Normal(_)))
}

fn relocation_error(message: impl Into<String>) -> SettingsError {
    SettingsError::Relocation(message.into())
}

async fn read_json<T: serde::de::DeserializeOwned>(
    path: &Path,
) -> Result<Option<T>, SettingsError> {
    let Some(text) = fsx::read_to_string_opt(path).await? else {
        return Ok(None);
    };
    serde_json::from_str(&text)
        .map(Some)
        .map_err(|e| SettingsError::Invalid {
            path: path.display().to_string(),
            message: e.to_string(),
        })
}

async fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<(), SettingsError> {
    let text = serde_json::to_string_pretty(value).map_err(|e| SettingsError::Invalid {
        path: path.display().to_string(),
        message: e.to_string(),
    })?;
    Ok(fsx::write_atomic(path, text.as_bytes()).await?)
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

#[cfg(test)]
mod tests {
    use super::*;

    type R = Result<(), Box<dyn std::error::Error>>;

    fn seed(dir: &Path) -> std::io::Result<()> {
        std::fs::create_dir_all(dir.join("memory").join("p1"))?;
        std::fs::write(dir.join("harness.sqlite"), vec![7u8; 200_000])?;
        std::fs::write(dir.join("settings.json"), "{}")?;
        std::fs::write(dir.join("memory").join("p1").join("MEMORY.md"), "note")?;
        std::fs::write(dir.join("x.lock"), "")?;
        Ok(())
    }

    #[tokio::test]
    async fn relocates_and_moves_back() -> R {
        let root = tempfile::tempdir()?;
        let base = root.path().join("base");
        let target = root.path().join("elsewhere").join("data");
        seed(&base)?;
        schedule(&base, &target).await?;
        assert_eq!(pending(&base).await, Some(absolute(&target)?));
        let report = apply_pending(&base).await?.ok_or("no report")?;
        assert_eq!(report.files_copied, 3);
        assert!(report.cleanup_complete);
        assert!(pending(&base).await.is_none());
        let info = resolve_data_dir(&base).await;
        assert!(info.relocated);
        assert_eq!(info.data_dir, absolute(&target)?);
        assert_eq!(
            std::fs::read_to_string(target.join("memory/p1/MEMORY.md"))?,
            "note"
        );
        assert!(!base.join("harness.sqlite").exists());
        assert!(!base.join("memory").exists());
        assert!(
            base.join("x.lock").exists(),
            "lock files are neither copied nor deleted"
        );

        // Move back to the base dir: pointer removed, old dir removed.
        std::fs::remove_file(base.join("x.lock"))?;
        let report = relocate(&base, &info.data_dir, &base).await?;
        assert!(report.cleanup_complete);
        let info = resolve_data_dir(&base).await;
        assert!(!info.relocated);
        assert!(base.join("harness.sqlite").exists());
        assert!(!target.exists());
        Ok(())
    }

    #[tokio::test]
    async fn interruption_before_switch_keeps_old_dir_and_retry_succeeds() -> R {
        let root = tempfile::tempdir()?;
        let base = root.path().join("base");
        let target = root.path().join("new");
        seed(&base)?;
        for point in [FailPoint::AfterCopy, FailPoint::AfterVerify] {
            assert!(relocate_with_fail_point(&base, &base, &target, Some(point))
                .await
                .is_err());
            let info = resolve_data_dir(&base).await;
            assert!(!info.relocated);
            assert!(base.join("harness.sqlite").exists());
            assert!(target.join(COPY_MARKER).exists());
        }
        relocate(&base, &base, &target).await?;
        assert!(resolve_data_dir(&base).await.relocated);
        assert!(!target.join(COPY_MARKER).exists());
        Ok(())
    }

    #[tokio::test]
    async fn interruption_after_switch_resumes_cleanup() -> R {
        let root = tempfile::tempdir()?;
        let base = root.path().join("base");
        let target = root.path().join("new");
        seed(&base)?;
        assert!(
            relocate_with_fail_point(&base, &base, &target, Some(FailPoint::AfterSwitch))
                .await
                .is_err()
        );
        assert!(base.join("harness.sqlite").exists());
        let info = resolve_data_dir(&base).await;
        assert!(info.relocated);
        assert!(!base.join("harness.sqlite").exists());
        assert!(!base.join(CLEANUP_FILE).exists());
        Ok(())
    }

    #[tokio::test]
    async fn rejects_bad_targets_and_lost_marker() -> R {
        let root = tempfile::tempdir()?;
        let base = root.path().join("base");
        seed(&base)?;
        assert!(schedule(&base, Path::new("relative")).await.is_err());
        assert!(schedule(&base, &base.join("inner")).await.is_err());
        let busy = root.path().join("busy");
        std::fs::create_dir_all(&busy)?;
        std::fs::write(busy.join("f"), "x")?;
        assert!(schedule(&base, &busy).await.is_err());
        std::fs::write(
            base.join(POINTER_FILE),
            serde_json::to_string(&serde_json::json!({"dataDir": root.path().join("gone")}))?,
        )?;
        assert!(!resolve_data_dir(&base).await.relocated);
        Ok(())
    }
}
