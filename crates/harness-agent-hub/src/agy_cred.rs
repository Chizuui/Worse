//! Credential store seam for Antigravity (agy) accounts. The hub never touches the OS store directly: the app
//! injects an implementation (Windows Credential Manager); the default reports the feature as unavailable.

use std::sync::Arc;

/// Generic credential agy keeps its login in.
pub const AGY_LIVE_TARGET: &str = "gemini:antigravity";
/// Prefix of the Harness-owned copies (`harness:agy:<account-id>`).
pub const AGY_SLOT_PREFIX: &str = "harness:agy:";

/// Target of an account's saved login.
#[must_use]
pub fn slot_target(id: &str) -> String {
    format!("{AGY_SLOT_PREFIX}{id}")
}

/// One credential attribute (keyword, flags, value).
#[derive(Clone, PartialEq, Eq)]
pub struct CredAttribute {
    pub keyword: String,
    pub flags: u32,
    pub value: Vec<u8>,
}

/// A generic credential copied field by field. `Debug` redacts the secret and attribute values; the buffers
/// are zeroed on drop.
#[derive(Clone, PartialEq, Eq, Default)]
pub struct CredBlob {
    pub persist: u32,
    pub user_name: Option<String>,
    pub comment: Option<String>,
    pub target_alias: Option<String>,
    pub secret: Vec<u8>,
    pub attributes: Vec<CredAttribute>,
}

impl std::fmt::Debug for CredBlob {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CredBlob")
            .field("persist", &self.persist)
            .field("secret", &"<redacted>")
            .field("attributes", &self.attributes.len())
            .finish_non_exhaustive()
    }
}

impl std::fmt::Debug for CredAttribute {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CredAttribute")
            .field("keyword", &self.keyword)
            .field("value", &"<redacted>")
            .finish_non_exhaustive()
    }
}

/// Overwrites a buffer with zeros.
pub fn zero(buf: &mut [u8]) {
    buf.fill(0);
    std::hint::black_box(buf);
}

impl Drop for CredBlob {
    fn drop(&mut self) {
        zero(&mut self.secret);
        for attribute in &mut self.attributes {
            zero(&mut attribute.value);
        }
    }
}

/// Generic-credential store. Errors are plain messages without secret content.
pub trait AgyCredStore: Send + Sync {
    /// Whether the store works on this platform.
    fn available(&self) -> bool;
    /// The credential at `target`, `None` when absent.
    ///
    /// # Errors
    ///
    /// Store failure other than "not found".
    fn read(&self, target: &str) -> Result<Option<CredBlob>, String>;
    /// Creates or replaces the credential at `target`.
    ///
    /// # Errors
    ///
    /// Store failure.
    fn write(&self, target: &str, blob: &CredBlob) -> Result<(), String>;
    /// Deletes the credential at `target`; absent is not an error.
    ///
    /// # Errors
    ///
    /// Store failure other than "not found".
    fn delete(&self, target: &str) -> Result<(), String>;
    /// Whether a credential exists at `target`.
    ///
    /// # Errors
    ///
    /// Store failure.
    fn exists(&self, target: &str) -> Result<bool, String> {
        self.read(target).map(|blob| blob.is_some())
    }
}

/// Store used where no OS implementation is injected.
pub struct UnavailableCredStore;

const UNAVAILABLE: &str = "Antigravity accounts need Windows Credential Manager (Windows only)";

impl AgyCredStore for UnavailableCredStore {
    fn available(&self) -> bool {
        false
    }
    fn read(&self, _target: &str) -> Result<Option<CredBlob>, String> {
        Err(UNAVAILABLE.into())
    }
    fn write(&self, _target: &str, _blob: &CredBlob) -> Result<(), String> {
        Err(UNAVAILABLE.into())
    }
    fn delete(&self, _target: &str) -> Result<(), String> {
        Err(UNAVAILABLE.into())
    }
}

/// Shared [`AgyCredStore`] (`HubConfig` stays `Debug`/`Clone`).
#[derive(Clone)]
pub struct SharedAgyCredStore(pub Arc<dyn AgyCredStore>);

impl Default for SharedAgyCredStore {
    fn default() -> Self {
        Self(Arc::new(UnavailableCredStore))
    }
}

impl std::fmt::Debug for SharedAgyCredStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SharedAgyCredStore")
    }
}

#[cfg(test)]
pub(crate) mod fake {
    use std::collections::HashMap;
    use std::sync::Mutex;

    use super::{AgyCredStore, CredBlob};

    #[derive(Default)]
    pub(crate) struct MemStore(pub Mutex<HashMap<String, CredBlob>>);

    impl MemStore {
        pub(crate) fn secret(&self, target: &str) -> Option<Vec<u8>> {
            self.0.lock().ok()?.get(target).map(|b| b.secret.clone())
        }
    }

    impl AgyCredStore for MemStore {
        fn available(&self) -> bool {
            true
        }
        fn read(&self, target: &str) -> Result<Option<CredBlob>, String> {
            Ok(self
                .0
                .lock()
                .map_err(|e| e.to_string())?
                .get(target)
                .cloned())
        }
        fn write(&self, target: &str, blob: &CredBlob) -> Result<(), String> {
            self.0
                .lock()
                .map_err(|e| e.to_string())?
                .insert(target.to_owned(), blob.clone());
            Ok(())
        }
        fn delete(&self, target: &str) -> Result<(), String> {
            self.0.lock().map_err(|e| e.to_string())?.remove(target);
            Ok(())
        }
    }
}
