//! Command error construction helpers.

use harness_protocol::{CommandError, ErrorCode};

/// Builds an internal command error.
pub(crate) fn internal(message: impl Into<String>) -> CommandError {
    CommandError {
        code: ErrorCode::Internal,
        message: message.into(),
    }
}

/// Builds an unsupported-operation command error.
pub(crate) fn unsupported(message: impl Into<String>) -> CommandError {
    CommandError {
        code: ErrorCode::Unsupported,
        message: message.into(),
    }
}

/// Builds an invalid-input command error.
pub(crate) fn invalid_input(message: impl Into<String>) -> CommandError {
    CommandError {
        code: ErrorCode::InvalidInput,
        message: message.into(),
    }
}

/// Builds a Host lifecycle command error with an explicit category.
#[cfg(not(any(target_os = "android", target_os = "ios")))]
pub(crate) fn host(code: ErrorCode, message: impl Into<String>) -> CommandError {
    CommandError {
        code,
        message: message.into(),
    }
}

/// Returns the camelCase wire name of an [`ErrorCode`].
///
/// Serialising the enum keeps this in lockstep with the generated TypeScript
/// union instead of duplicating the names in a second match.
#[cfg(not(any(target_os = "android", target_os = "ios")))]
pub(crate) fn wire_name(code: ErrorCode) -> String {
    serde_json::to_value(code).map_or_else(
        |_error| "internal".to_owned(),
        |value| {
            value
                .as_str()
                .map_or_else(|| "internal".to_owned(), ToOwned::to_owned)
        },
    )
}
