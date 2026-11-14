//! Plain and atomic writes.
//!
//! [`atomic_write`] mirrors `writeFileAtomically` in the TS
//! `packages/services/src/fs/atomicFileUtils.ts`: a temp file
//! `.<basename>.<uuid>.tmp` is created exclusively next to the target,
//! written, fsynced, optionally chmod-ed, then renamed over the target.
//! `std::fs::rename` maps to `MoveFileExW(MOVEFILE_REPLACE_EXISTING)` on
//! Windows. Transient Windows rename failures (`EPERM`/`EBUSY`/`EACCES`,
//! typically antivirus or an indexer holding the target) are retried after
//! 10, 25, 50 and 100 ms. The temp file is removed on every failure path.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::error::FsError;

/// Rename retry delays on Windows.
const RENAME_RETRY_DELAYS_MS: [u64; 4] = [10, 25, 50, 100];

/// Writes `data` to `path`, optionally creating parent directories.
///
/// # Errors
/// Any OS error.
pub fn write_file(path: &str, data: &[u8], create_dirs: bool) -> Result<(), FsError> {
    let p = Path::new(path);
    if create_dirs {
        ensure_parent(p)?;
    }
    fs::write(p, data).map_err(|e| FsError::from_io(&e, "open", p))
}

fn ensure_parent(p: &Path) -> Result<(), FsError> {
    if let Some(parent) = p.parent().filter(|d| !d.as_os_str().is_empty()) {
        fs::create_dir_all(parent).map_err(|e| FsError::from_io(&e, "mkdir", parent))?;
    }
    Ok(())
}

fn temp_path_for(target: &Path) -> PathBuf {
    let name = target
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    target.with_file_name(format!(".{name}.{}.tmp", uuid::Uuid::new_v4()))
}

/// Atomically replaces `path` with `data`. Readers observe either the old or
/// the new content, never a partial file. The parent directory must exist.
/// `mode` (Unix permission bits, masked with `0o777`) is ignored on Windows.
///
/// # Errors
/// Any OS error (`syscall:"open"` when the parent is missing).
pub fn atomic_write(path: &str, data: &[u8], mode: Option<u32>) -> Result<(), FsError> {
    let target = Path::new(path);
    let temp = temp_path_for(target);
    let result = write_temp_and_rename(target, &temp, data, mode);
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

fn write_temp_and_rename(
    target: &Path,
    temp: &Path,
    data: &[u8],
    mode: Option<u32>,
) -> Result<(), FsError> {
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(temp)
        .map_err(|e| FsError::from_io(&e, "open", temp))?;
    file.write_all(data)
        .map_err(|e| FsError::from_io(&e, "write", temp))?;
    file.sync_all()
        .map_err(|e| FsError::from_io(&e, "fsync", temp))?;
    drop(file);
    apply_mode(temp, mode)?;
    rename_with_retry(temp, target)
}

#[cfg(unix)]
fn apply_mode(temp: &Path, mode: Option<u32>) -> Result<(), FsError> {
    use std::os::unix::fs::PermissionsExt;
    if let Some(mode) = mode {
        fs::set_permissions(temp, fs::Permissions::from_mode(mode & 0o777))
            .map_err(|e| FsError::from_io(&e, "chmod", temp))?;
    }
    Ok(())
}

#[cfg(not(unix))]
#[allow(clippy::unnecessary_wraps)]
fn apply_mode(_temp: &Path, _mode: Option<u32>) -> Result<(), FsError> {
    Ok(())
}

fn rename_with_retry(temp: &Path, target: &Path) -> Result<(), FsError> {
    let mut attempt = 0;
    loop {
        match fs::rename(temp, target) {
            Ok(()) => return Ok(()),
            Err(e) => {
                let delay = RENAME_RETRY_DELAYS_MS.get(attempt).copied();
                match delay {
                    Some(ms) if cfg!(windows) && is_transient(&e) => {
                        attempt += 1;
                        tracing::debug!(target = %target.display(), attempt, "atomic rename retry");
                        std::thread::sleep(Duration::from_millis(ms));
                    }
                    _ => return Err(FsError::from_io(&e, "rename", target)),
                }
            }
        }
    }
}

fn is_transient(error: &std::io::Error) -> bool {
    // Windows：ERROR_ACCESS_DENIED(5) / ERROR_SHARING_VIOLATION(32) / ERROR_LOCK_VIOLATION(33)
    // 常由杀毒或索引服务短暂占用目标文件引起（Node 中表现为 EPERM/EBUSY/EACCES），可重试。
    matches!(error.raw_os_error(), Some(5 | 32 | 33))
}
