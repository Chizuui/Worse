//! Errors of the git service.

/// A git service failure.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum GitError {
    /// TS `throw new Error(message)` equivalent (same message text).
    #[error("{0}")]
    App(String),
    /// The request was cancelled; the running git child was killed.
    #[error("cancelled")]
    Cancelled,
}
