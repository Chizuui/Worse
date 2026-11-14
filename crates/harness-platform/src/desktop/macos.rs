//! macOS path reveal through Finder.

use crate::{PathRevealer, PlatformError};
use std::path::Path;
use std::process::Command;

/// Reveals paths in Finder.
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
        tracing::debug!(path = %path.display(), "revealing path in Finder");

        let status = Command::new("open")
            .arg("-R")
            .arg(path)
            .status()
            .map_err(|error| PlatformError::Io(format!("failed to start open: {error}")))?;

        if status.success() {
            Ok(())
        } else {
            Err(PlatformError::Io(format!("open -R exited with {status}")))
        }
    }
}
