//! Error type.

use std::path::PathBuf;

use crate::types::{AgentKind, Concept, Scope};

/// Agent configuration failures.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// The CLI has no such concept or scope.
    #[error("{agent:?} does not support {concept:?} ({scope:?}): {reason}")]
    Unsupported {
        agent: AgentKind,
        concept: Concept,
        scope: Scope,
        reason: String,
    },
    /// The file changed after the plan was computed; nothing was written.
    #[error("{} changed since the preview; reload and try again", path.display())]
    Conflict { path: PathBuf },
    /// The file exists but cannot be parsed, or the input is invalid.
    #[error("{message}")]
    Invalid { message: String },
    /// No entry with that name/id.
    #[error("not found: {0}")]
    NotFound(String),
    /// A path outside the allowed config files, or a credentials file.
    #[error("refused: {0}")]
    Forbidden(String),
    /// No workspace given for a workspace-scope call.
    #[error("no workspace selected")]
    NoWorkspace,
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

impl ConfigError {
    pub(crate) fn invalid(message: impl Into<String>) -> Self {
        Self::Invalid {
            message: message.into(),
        }
    }

    pub(crate) fn unsupported(
        agent: AgentKind,
        concept: Concept,
        scope: Scope,
        reason: impl Into<String>,
    ) -> Self {
        Self::Unsupported {
            agent,
            concept,
            scope,
            reason: reason.into(),
        }
    }
}

pub type Result<T> = std::result::Result<T, ConfigError>;
