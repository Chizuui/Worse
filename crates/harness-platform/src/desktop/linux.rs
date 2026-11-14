//! Linux path reveal through the default file manager.

use crate::{PathRevealer, PlatformError};
use std::path::Path;
use std::process::Command;

/// Reveals paths by opening the containing directory with `xdg-open`.
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
        let directory = if path.is_dir() {
            path
        } else {
            path.parent()
                .filter(|parent| !parent.as_os_str().is_empty())
                .ok_or_else(|| {
                    PlatformError::Io(format!("path has no parent directory: {}", path.display()))
                })?
        };

        tracing::debug!(directory = %directory.display(), "opening directory with xdg-open");

        let status = Command::new("xdg-open")
            .arg(directory)
            .status()
            .map_err(|error| PlatformError::Io(format!("failed to start xdg-open: {error}")))?;

        if status.success() {
            Ok(())
        } else {
            Err(PlatformError::Io(format!("xdg-open exited with {status}")))
        }
    }
}
