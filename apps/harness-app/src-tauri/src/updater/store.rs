//! Updater preferences on disk (`<data>/updater.json`, spec section 6).
//!
//! Async IO only; writes go to a temp file that is renamed over the target so a
//! crash never leaves a half-written file.

use std::io;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use super::model::ReleaseNotes;

const FILE_NAME: &str = "updater.json";

/// Everything the updater keeps across restarts.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub(crate) struct Persisted {
    pub(crate) auto_download_and_install_updates: bool,
    pub(crate) skipped_version: Option<String>,
    pub(crate) pending_release_notes: Option<ReleaseNotes>,
}

/// Cached view of `updater.json`; the async mutex serialises read-modify-write.
pub(crate) struct Store {
    path: PathBuf,
    cache: Mutex<Option<Persisted>>,
}

impl Store {
    pub(crate) fn new(directory: PathBuf) -> Self {
        Self {
            path: directory.join(FILE_NAME),
            cache: Mutex::new(None),
        }
    }

    /// Reads the preferences; a missing or unreadable file yields defaults.
    pub(crate) async fn load(&self) -> Persisted {
        let mut cache = self.cache.lock().await;
        if let Some(cached) = cache.as_ref() {
            return cached.clone();
        }
        let loaded = read(&self.path).await;
        *cache = Some(loaded.clone());
        loaded
    }

    /// Applies `change` and persists the result atomically.
    pub(crate) async fn update(&self, change: impl FnOnce(&mut Persisted)) -> io::Result<()> {
        let mut cache = self.cache.lock().await;
        let mut next = match cache.as_ref() {
            Some(cached) => cached.clone(),
            None => read(&self.path).await,
        };
        change(&mut next);
        write(&self.path, &next).await?;
        *cache = Some(next);
        Ok(())
    }
}

async fn read(path: &PathBuf) -> Persisted {
    match tokio::fs::read(path).await {
        Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_else(|error| {
            tracing::warn!(%error, path = %path.display(), "ignoring an unreadable updater.json");
            Persisted::default()
        }),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Persisted::default(),
        Err(error) => {
            tracing::warn!(%error, path = %path.display(), "failed to read updater.json");
            Persisted::default()
        }
    }
}

async fn write(path: &PathBuf, value: &Persisted) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let bytes = serde_json::to_vec_pretty(value).map_err(io::Error::other)?;
    let temp = path.with_extension("json.tmp");
    tokio::fs::write(&temp, bytes).await?;
    tokio::fs::rename(&temp, path).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn round_trips_and_defaults() -> io::Result<()> {
        let directory = tempfile::tempdir()?;
        let store = Store::new(directory.path().join("nested"));
        assert_eq!(store.load().await, Persisted::default());
        store
            .update(|value| {
                value.auto_download_and_install_updates = true;
                value.skipped_version = Some("0.2.0".to_owned());
            })
            .await?;
        let reopened = Store::new(directory.path().join("nested"));
        let loaded = reopened.load().await;
        assert!(loaded.auto_download_and_install_updates);
        assert_eq!(loaded.skipped_version.as_deref(), Some("0.2.0"));
        assert!(!directory.path().join("nested/updater.json.tmp").exists());
        Ok(())
    }

    #[tokio::test]
    async fn corrupt_file_yields_defaults() -> io::Result<()> {
        let directory = tempfile::tempdir()?;
        tokio::fs::write(directory.path().join(FILE_NAME), b"{not json").await?;
        let store = Store::new(directory.path().to_path_buf());
        assert_eq!(store.load().await, Persisted::default());
        Ok(())
    }
}
