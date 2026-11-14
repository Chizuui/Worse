//! Capability flags that gate platform commands for the active surface.

use serde::{Deserialize, Serialize};
use ts_rs::TS;

/// Feature switches the frontend must honour before calling a command.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export, export_to = "../../../apps/harness-app/src/generated/")]
#[allow(clippy::struct_excessive_bools)] // one flag per independent capability
pub struct Capabilities {
    /// Whether external URLs can be opened in the system browser.
    pub open_external: bool,
    /// Whether a path can be revealed in the system file manager.
    pub reveal_path: bool,
    /// Whether the native directory picker is available.
    pub select_directory: bool,
    /// Whether the native file picker is available.
    pub select_file: bool,
    /// Whether this surface may spawn a local host and agent.
    pub local_host: bool,
    /// Whether native notifications are available.
    pub notifications: bool,
}
