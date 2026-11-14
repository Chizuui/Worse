//! Windows path reveal through File Explorer.

use crate::{PathRevealer, PlatformError};
use std::path::Path;
use std::process::Command;

/// Reveals paths in the Windows File Explorer.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemPathRevealer;

impl SystemPathRevealer {
    /// Creates a revealer.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

impl PathRevealer for SystemPathRevealer {
    fn reveal(&self, path: &Path) -> Result<(), PlatformError> {
        tracing::debug!(path = %path.display(), "revealing path in File Explorer");

        // Explorer expects `/select,<path>` as a single argument.
        let selector = format!("/select,{}", path.display());
        let mut child = Command::new("explorer")
            .arg(selector)
            .spawn()
            .map_err(|error| PlatformError::Io(format!("failed to start explorer: {error}")))?;

        // Reap the child off the caller's thread. Explorer exits with code 1
        // even on success, so its status carries no signal.
        std::thread::spawn(move || {
            let _ = child.wait();
        });

        Ok(())
    }
}
