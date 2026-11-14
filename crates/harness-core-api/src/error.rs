//! API error type.

use serde::{Deserialize, Serialize};

/// Result alias of every [`crate::HarnessCore`] call.
pub type ApiResult<T> = Result<T, ApiError>;

/// Errors returned by the hub. Serializable so a remote transport can carry them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[serde(tag = "kind", content = "message", rename_all = "camelCase")]
pub enum ApiError {
    /// Session, turn, agent or permission request does not exist.
    #[error("not found: {0}")]
    NotFound(String),
    /// The request is malformed (empty text, unknown agent id in an order, ...).
    #[error("invalid argument: {0}")]
    InvalidArgument(String),
    /// A permission request was already answered or cancelled.
    #[error("already resolved: {0}")]
    AlreadyResolved(String),
    /// The requested agent is not installed or not usable on this machine.
    #[error("agent unavailable: {0}")]
    AgentUnavailable(String),
    /// The hub is shutting down or has shut down.
    #[error("hub is shut down")]
    ShutDown,
    /// Storage or file-system failure.
    #[error("storage error: {0}")]
    Storage(String),
    /// A file changed since its plan was made; nothing was written (API 0.4.0).
    #[error("conflict: {0}")]
    Conflict(String),
    /// The session has a running, queued or paused turn or a pending permission (API 0.4.0).
    #[error("busy: {0}")]
    Busy(String),
    /// Any other internal failure.
    #[error("internal error: {0}")]
    Internal(String),
}
