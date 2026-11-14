//! Host attachment and process-state DTOs shared with the frontend.
//!
//! The shell owns the Host process lifecycle; these types are the only Host
//! state the webview is allowed to observe directly.

use serde::{Deserialize, Serialize};
use ts_rs::TS;

/// Grant handed to the webview so it can open one Host attachment.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export, export_to = "../../../apps/harness-app/src/generated/")]
pub struct HostAttachmentGrant {
    /// Attachment identifier the WebSocket hello must echo.
    pub attachment_id: String,
    /// Loopback WebSocket URL; never contains the token.
    pub url: String,
    /// Single-use attachment token; never logged by any layer.
    pub token: String,
    /// Identifier of the `database-startup-state` sequence this attachment belongs to.
    pub database_startup_id: String,
    /// Latest cached `database-startup-state`, when one has been observed.
    #[ts(type = "unknown | null")]
    pub startup_state: Option<serde_json::Value>,
}

/// Process state of the local Host as observed by the shell.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export, export_to = "../../../apps/harness-app/src/generated/")]
pub struct HostStatus {
    /// Coarse lifecycle state.
    pub state: HostStateDto,
    /// Stable error category when `state` is `failed`.
    pub error_code: Option<String>,
    /// Human-readable failure detail; never contains credentials.
    pub error_message: Option<String>,
}

/// Frontend-facing Host lifecycle state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export, export_to = "../../../apps/harness-app/src/generated/")]
pub enum HostStateDto {
    /// No Host process has been started.
    Stopped,
    /// The sidecar is spawning or the Host is importing.
    Starting,
    /// The Host reported `ready` and accepts attachments.
    Ready,
    /// Starting the Host failed.
    Failed,
}
