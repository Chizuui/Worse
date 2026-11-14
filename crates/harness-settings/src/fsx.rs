//! File helpers shared by every Harness config writer: atomic writes and the credentials deny list.
//!
//! Rule (spec phase-9-parity.md §1.3): Harness never opens credential files. Every read and write of config files
//! goes through [`read_to_string_opt`] / [`write_atomic`], which refuse the names in [`is_forbidden`].

use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use tokio::io::AsyncWriteExt;

/// File names Harness must never open (credentials of agent CLIs and generic secret files).
const FORBIDDEN_NAMES: &[&str] = &[
    ".credentials.json",
    "credentials.json",
    "auth.json",
    "oauth_creds.json",
    "google_accounts.json",
];

/// True for credential files (by name): `.credentials.json`, `credentials.json`, `auth.json`,
/// `oauth_creds.json`, `google_accounts.json`, `*.key`, `*.pem`, `.env` and `.env.*`.
#[must_use]
pub fn is_forbidden(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    let lower = name.to_ascii_lowercase();
    FORBIDDEN_NAMES.contains(&lower.as_str())
        || path
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("key") || e.eq_ignore_ascii_case("pem"))
        || lower == ".env"
        || lower.starts_with(".env.")
}

fn forbidden(path: &Path) -> io::Error {
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        format!(
            "{} is a credentials file; Harness never opens it",
            path.display()
        ),
    )
}

/// Reads a UTF-8 text file; `Ok(None)` when it does not exist.
///
/// # Errors
///
/// I/O errors, invalid UTF-8, or `PermissionDenied` for a forbidden file.
pub async fn read_to_string_opt(path: &Path) -> io::Result<Option<String>> {
    if is_forbidden(path) {
        return Err(forbidden(path));
    }
    match tokio::fs::read(path).await {
        Ok(bytes) => String::from_utf8(bytes)
            .map(Some)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// True when `path` exists (metadata only; the file is never opened, so this is allowed for credential files and is
/// what the auth presence probe uses).
pub async fn exists(path: &Path) -> bool {
    tokio::fs::try_exists(path).await.unwrap_or(false)
}

static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Temp file next to `path` (same directory, so the final rename stays on one volume).
fn temp_path(path: &Path) -> PathBuf {
    let name = path
        .file_name()
        .map_or_else(|| "file".into(), |n| n.to_string_lossy().into_owned());
    let n = TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    path.with_file_name(format!(".{name}.harness-tmp-{}-{n}", std::process::id()))
}

/// True for temp files created by [`write_atomic`] (skipped by copies and listings).
#[must_use]
pub fn is_temp_file(path: &Path) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.contains(".harness-tmp-"))
}

/// Writes `contents` atomically: temp file in the same directory → flush + fsync → rename over `path`.
/// Parent directories are created. Readers see either the old or the new content, never a partial file.
///
/// # Errors
///
/// I/O errors (the temp file is removed), or `PermissionDenied` for a forbidden file.
pub async fn write_atomic(path: &Path, contents: &[u8]) -> io::Result<()> {
    if is_forbidden(path) {
        return Err(forbidden(path));
    }
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        tokio::fs::create_dir_all(parent).await?;
    }
    let tmp = temp_path(path);
    let result = async {
        let mut file = tokio::fs::File::create(&tmp).await?;
        file.write_all(contents).await?;
        file.flush().await?;
        file.sync_all().await?;
        drop(file);
        tokio::fs::rename(&tmp, path).await
    }
    .await;
    if result.is_err() {
        let _ = tokio::fs::remove_file(&tmp).await;
    }
    result
}

/// Removes a file; a missing file is not an error.
///
/// # Errors
///
/// I/O errors, or `PermissionDenied` for a forbidden file.
pub async fn remove_file_opt(path: &Path) -> io::Result<()> {
    if is_forbidden(path) {
        return Err(forbidden(path));
    }
    match tokio::fs::remove_file(path).await {
        Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credential_names_are_forbidden() {
        for name in [
            ".credentials.json",
            "Auth.json",
            "oauth_creds.json",
            "id.key",
            ".env",
            ".env.local",
            "credentials.json",
        ] {
            assert!(is_forbidden(Path::new("x").join(name).as_path()), "{name}");
        }
        for name in [
            "settings.json",
            ".claude.json",
            "config.toml",
            "mcp.json",
            "SKILL.md",
        ] {
            assert!(!is_forbidden(Path::new(name)), "{name}");
        }
    }

    #[tokio::test]
    async fn forbidden_files_are_never_read_or_written() -> io::Result<()> {
        let dir = tempfile::tempdir()?;
        let creds = dir.path().join(".credentials.json");
        std::fs::write(&creds, "SECRET")?;
        assert!(read_to_string_opt(&creds).await.is_err());
        assert!(write_atomic(&creds, b"x").await.is_err());
        assert_eq!(std::fs::read_to_string(&creds)?, "SECRET");
        assert!(exists(&creds).await);
        Ok(())
    }

    #[tokio::test]
    async fn atomic_write_replaces_and_leaves_no_temp() -> io::Result<()> {
        let dir = tempfile::tempdir()?;
        let file = dir.path().join("sub").join("a.json");
        write_atomic(&file, b"one").await?;
        write_atomic(&file, b"two").await?;
        assert_eq!(read_to_string_opt(&file).await?.as_deref(), Some("two"));
        let leftovers = std::fs::read_dir(dir.path().join("sub"))?
            .filter_map(Result::ok)
            .filter(|e| is_temp_file(&e.path()))
            .count();
        assert_eq!(leftovers, 0);
        Ok(())
    }
}
