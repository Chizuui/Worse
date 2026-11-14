//! Application identity types shared with the frontend.

use harness_core::Surface;
use serde::{Deserialize, Serialize};
use ts_rs::TS;

/// Build and runtime identity for the running Harness shell.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export, export_to = "../../../apps/harness-app/src/generated/")]
pub struct AppInfo {
    /// Product name shown to the user.
    pub name: String,
    /// Shell version taken from the package manifest.
    pub version: String,
    /// Protocol version the shell implements.
    pub protocol_version: u32,
    /// Surface the shell is running on.
    pub surface: SurfaceDto,
    /// Operating system family reported by the shell.
    pub os: String,
}

/// Frontend-facing representation of [`Surface`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export, export_to = "../../../apps/harness-app/src/generated/")]
pub enum SurfaceDto {
    /// Desktop shell that may own a local host.
    Desktop,
    /// Mobile shell that is only a remote-control client.
    Mobile,
}

impl From<Surface> for SurfaceDto {
    fn from(surface: Surface) -> Self {
        match surface {
            Surface::Desktop => Self::Desktop,
            Surface::Mobile => Self::Mobile,
        }
    }
}
