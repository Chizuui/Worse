//! Local Host bridge commands.
//!
//! The commands only carry control traffic: attachment grants, database startup
//! control, and Host status. Conversation traffic never crosses this surface.

#![allow(clippy::needless_pass_by_value)]

use harness_protocol::{CommandError, HostAttachmentGrant, HostStatus};

#[cfg(not(any(target_os = "android", target_os = "ios")))]
use crate::state::AppState;
#[cfg(not(any(target_os = "android", target_os = "ios")))]
use serde_json::Value;
#[cfg(not(any(target_os = "android", target_os = "ios")))]
use tauri::State;

/// Reports the local Host lifecycle without spawning it.
#[cfg(not(any(target_os = "android", target_os = "ios")))]
#[tauri::command]
pub(crate) async fn host_status(state: State<'_, AppState>) -> Result<HostStatus, CommandError> {
    Ok(state.host().status())
}
/// Mobile shells never own a local Host.
#[cfg(any(target_os = "android", target_os = "ios"))]
#[tauri::command]
#[allow(clippy::unused_async)]
pub(crate) async fn host_status() -> Result<HostStatus, CommandError> {
    Ok(HostStatus {
        state: harness_protocol::HostStateDto::Stopped,
        error_code: None,
        error_message: None,
    })
}

/// Spawns the Host on first use and returns one attachment grant.
#[cfg(not(any(target_os = "android", target_os = "ios")))]
#[tauri::command]
pub(crate) async fn host_open_attachment(
    state: State<'_, AppState>,
) -> Result<HostAttachmentGrant, CommandError> {
    state.host().open_attachment().await
}

/// Mobile shells cannot open a local Host attachment.
#[cfg(any(target_os = "android", target_os = "ios"))]
#[tauri::command]
#[allow(clippy::unused_async)]
pub(crate) async fn host_open_attachment() -> Result<HostAttachmentGrant, CommandError> {
    Err(crate::error::unsupported(
        "local host is not available on mobile",
    ))
}
/// Applies one `database-startup-control` message from the webview.
#[cfg(not(any(target_os = "android", target_os = "ios")))]
#[tauri::command]
pub(crate) async fn host_database_startup_control(
    state: State<'_, AppState>,
    control: Value,
) -> Result<(), CommandError> {
    state.host().control(control).await
}

/// Mobile shells cannot control a local Host database startup.
#[cfg(any(target_os = "android", target_os = "ios"))]
#[tauri::command]
#[allow(clippy::unused_async)]
pub(crate) async fn host_database_startup_control() -> Result<(), CommandError> {
    Err(crate::error::unsupported(
        "local host is not available on mobile",
    ))
}
