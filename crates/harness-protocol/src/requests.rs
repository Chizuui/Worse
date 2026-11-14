//! Request and response payloads for platform commands.

use serde::{Deserialize, Serialize};
use ts_rs::TS;

/// Payload for the `open_external` command.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export, export_to = "../../../apps/harness-app/src/generated/")]
pub struct OpenExternalRequest {
    /// URL to open; the shell validates the scheme before use.
    pub url: String,
}

/// Payload for the `reveal_path` command.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export, export_to = "../../../apps/harness-app/src/generated/")]
pub struct RevealPathRequest {
    /// Filesystem path to show in the platform file manager.
    pub path: String,
}

/// Result of the `select_directory` command; `None` when the user cancels.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export, export_to = "../../../apps/harness-app/src/generated/")]
pub struct SelectDirectoryResponse {
    /// Chosen directory, or `None` when the picker was cancelled.
    pub path: Option<String>,
}

/// Result of the `select_file` command; `None` when the user cancels.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export, export_to = "../../../apps/harness-app/src/generated/")]
pub struct SelectFileResponse {
    /// Chosen file, or `None` when the picker was cancelled.
    pub path: Option<String>,
}
