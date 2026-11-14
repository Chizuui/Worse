//! Command error envelope returned by every shell command.

use serde::{Deserialize, Serialize};
use ts_rs::TS;

/// Typed error returned to the frontend by a failed command.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export, export_to = "../../../apps/harness-app/src/generated/")]
pub struct CommandError {
    /// Machine-readable error category.
    pub code: ErrorCode,
    /// Human-readable detail; never contains credentials.
    pub message: String,
}

/// Machine-readable category for a [`CommandError`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export, export_to = "../../../apps/harness-app/src/generated/")]
pub enum ErrorCode {
    /// The surface does not support the command.
    Unsupported,
    /// The payload was rejected before execution.
    InvalidInput,
    /// The user or the platform denied the operation.
    Denied,
    /// The shell failed unexpectedly.
    Internal,
    /// The local Host has not been started yet.
    HostNotStarted,
    /// The local Host sidecar could not be spawned.
    HostSpawnFailed,
    /// The local Host produced an invalid or unexpected protocol message.
    HostProtocolError,
    /// The local Host did not answer before the deadline.
    HostTimeout,
}
