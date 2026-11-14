//! Search errors.

use harness_fs::FsError;

/// Failures of index builds and searches.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SearchError {
    /// The operation was cancelled through its token.
    #[error("search cancelled")]
    Cancelled,
    /// A file-system failure surfaced to the caller (Node-style).
    #[error(transparent)]
    Fs(#[from] FsError),
    /// The query could not be compiled.
    #[error("invalid search pattern: {0}")]
    InvalidPattern(String),
}
