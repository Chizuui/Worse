//! Application metadata and capability commands.

#![allow(clippy::needless_pass_by_value)]

use crate::state::AppState;
use harness_protocol::{AppInfo, Capabilities, PROTOCOL_VERSION};
use tauri::State;

/// Returns shell identity and protocol metadata.
#[tauri::command]
#[must_use]
pub(crate) fn get_app_info(state: State<'_, AppState>) -> AppInfo {
    AppInfo {
        name: "Harness".to_owned(),
        version: env!("CARGO_PKG_VERSION").to_owned(),
        protocol_version: PROTOCOL_VERSION,
        surface: state.surface().into(),
        os: std::env::consts::OS.to_owned(),
    }
}

/// Returns capability flags for the active surface.
#[tauri::command]
#[must_use]
pub(crate) fn get_capabilities(state: State<'_, AppState>) -> Capabilities {
    harness_platform::capabilities_for(state.surface())
}
