//! Errors raised while decoding or encoding bridge lines.

use thiserror::Error;

/// Failure to interpret an NDJSON line or to encode one.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ProtocolError {
    /// The line is not valid JSON, or does not match the expected shape.
    #[error("invalid bridge line: {0}")]
    InvalidJson(String),
    /// The line declares a bridge version this crate does not speak.
    #[error("unsupported bridge version: {0}")]
    UnsupportedVersion(u16),
    /// The line declares a kind that is not part of the protocol.
    #[error("unknown bridge line kind: {0}")]
    UnknownKind(String),
    /// The line exceeds [`crate::MAX_LINE_BYTES`].
    #[error("bridge line exceeds {0} bytes")]
    LineTooLarge(usize),
    /// A required envelope field is absent.
    #[error("missing bridge line field: {0}")]
    MissingField(&'static str),
}
