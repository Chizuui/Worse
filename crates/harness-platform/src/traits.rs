//! Adapter traits implemented by the Tauri composition root.

use crate::{PlatformError, ValidatedUrl};
use std::path::Path;

/// Opens validated external URLs with the operating system default handler.
///
/// Implementations may block on the operating system; callers must run them
/// outside the webview event loop.
pub trait UrlOpener: Send + Sync {
    /// Opens `url`.
    ///
    /// # Errors
    ///
    /// Returns [`PlatformError`] when the surface cannot open URLs or the
    /// operating system rejects the request.
    fn open(&self, url: &ValidatedUrl) -> Result<(), PlatformError>;
}

/// Reveals a filesystem path in the operating system file manager.
///
/// Implementations may block on the operating system; callers must run them
/// outside the webview event loop.
pub trait PathRevealer: Send + Sync {
    /// Shows `path` in the file manager.
    ///
    /// # Errors
    ///
    /// Returns [`PlatformError::Unsupported`] on mobile surfaces and
    /// [`PlatformError::Io`] when the file manager cannot be started.
    fn reveal(&self, path: &Path) -> Result<(), PlatformError>;
}
