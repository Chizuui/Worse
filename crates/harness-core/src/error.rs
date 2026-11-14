//! Errors raised by the core domain crate.

/// Errors produced while building core domain values.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CoreError {
    /// The supplied workspace identity and path were both empty.
    #[error("workspace identity and path are both empty")]
    EmptyWorkspaceKey,
    /// An identifier was empty or otherwise unusable.
    #[error("invalid identifier: {0}")]
    InvalidId(String),
}
