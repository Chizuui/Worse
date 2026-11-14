//! Internal error type, mapped to [`harness_core_api::ApiError`] at the API boundary.

use harness_core_api::ApiError;

/// Hub-internal error.
#[derive(Debug, thiserror::Error)]
pub enum HubError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("{0}")]
    Config(String),
    #[error("agent: {0}")]
    Agent(String),
    #[error("not found: {0}")]
    NotFound(String),
    #[error("invalid: {0}")]
    Invalid(String),
    #[error("hub is shut down")]
    ShutDown,
}

impl From<HubError> for ApiError {
    fn from(error: HubError) -> Self {
        match error {
            HubError::Io(e) => ApiError::Storage(e.to_string()),
            HubError::Sqlite(e) => ApiError::Storage(e.to_string()),
            HubError::Json(e) => ApiError::Internal(e.to_string()),
            HubError::Config(m) | HubError::Agent(m) => ApiError::Internal(m),
            HubError::NotFound(m) => ApiError::NotFound(m),
            HubError::Invalid(m) => ApiError::InvalidArgument(m),
            HubError::ShutDown => ApiError::ShutDown,
        }
    }
}
