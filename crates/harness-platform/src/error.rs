//! Errors returned by platform adapters.

use harness_protocol::{CommandError, ErrorCode};
use thiserror::Error;

/// Failure raised while performing a platform operation.
#[derive(Debug, Error)]
pub enum PlatformError {
    /// The active surface cannot perform the operation.
    #[error("operation is not supported on this surface")]
    Unsupported,
    /// The caller supplied input the adapter refuses to act on.
    #[error("invalid input: {0}")]
    InvalidInput(String),
    /// An operating system call failed.
    #[error("platform I/O failed: {0}")]
    Io(String),
}

impl From<PlatformError> for CommandError {
    fn from(error: PlatformError) -> Self {
        let code = match &error {
            PlatformError::Unsupported => ErrorCode::Unsupported,
            PlatformError::InvalidInput(_) => ErrorCode::InvalidInput,
            PlatformError::Io(_) => ErrorCode::Internal,
        };

        Self {
            code,
            message: error.to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::PlatformError;
    use harness_protocol::{CommandError, ErrorCode};

    #[test]
    fn unsupported_maps_to_unsupported_code() {
        let error: CommandError = PlatformError::Unsupported.into();

        assert_eq!(error.code, ErrorCode::Unsupported);
        assert_eq!(error.message, "operation is not supported on this surface");
    }

    #[test]
    fn invalid_input_keeps_its_detail() {
        let error: CommandError = PlatformError::InvalidInput("bad scheme".to_owned()).into();

        assert_eq!(error.code, ErrorCode::InvalidInput);
        assert!(error.message.contains("bad scheme"));
    }

    #[test]
    fn io_failure_maps_to_internal_code() {
        let error: CommandError = PlatformError::Io("launcher missing".to_owned()).into();

        assert_eq!(error.code, ErrorCode::Internal);
        assert!(error.message.contains("launcher missing"));
    }
}
