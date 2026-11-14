//! Errors raised while supervising sidecar processes.

use harness_core::SidecarId;

/// Failures produced by sidecar supervision.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SidecarError {
    /// The process could not be started.
    #[error("failed to spawn sidecar: {0}")]
    Spawn(String),
    /// A sidecar with the same identifier is already supervised.
    #[error("sidecar '{0}' is already running")]
    AlreadyRunning(SidecarId),
    /// No supervised sidecar matches the identifier.
    #[error("sidecar '{0}' is not supervised")]
    NotFound(SidecarId),
    /// The operating system returned an I/O error.
    #[error("sidecar I/O error: {0}")]
    Io(String),
}

impl From<std::io::Error> for SidecarError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error.to_string())
    }
}
