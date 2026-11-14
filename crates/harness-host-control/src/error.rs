//! Errors raised while supervising the Host sidecar.

/// Failures produced by [`crate::HostController`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum HostControlError {
    /// The Host sidecar could not be started.
    #[error("failed to spawn the Host sidecar: {0}")]
    Spawn(String),
    /// The sidecar or the Host violated the bridge protocol.
    #[error("Host protocol error: {0}")]
    Protocol(String),
    /// The Host did not answer within the configured timeout.
    #[error("Host timed out: {0}")]
    Timeout(String),
    /// The Host is not running, or it is already shutting down.
    #[error("the Host is not running")]
    NotRunning,
    /// A Host instance is already supervised.
    #[error("the Host is already running")]
    AlreadyRunning,
    /// An operating system pipe operation failed.
    #[error("Host I/O error: {0}")]
    Io(String),
    /// The shutdown barrier could not stop the Host cleanly.
    #[error("Host shutdown failed: {0}")]
    Shutdown(String),
}

impl HostControlError {
    /// Stable machine-readable code for the Tauri command layer.
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::Spawn(_) => "hostSpawnFailed",
            Self::Protocol(_) => "hostProtocolError",
            Self::Timeout(_) => "hostTimeout",
            Self::NotRunning => "hostNotStarted",
            Self::AlreadyRunning => "hostAlreadyRunning",
            Self::Io(_) => "hostIoError",
            Self::Shutdown(_) => "hostShutdownError",
        }
    }
}
