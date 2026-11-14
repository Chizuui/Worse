//! Phase 6 auto-updater (spec `Worse/docs/specs/harness/phase-6-updater.md`).
//!
//! Replaces `ZCode`'s `electron-updater` flow with `tauri-plugin-updater`. The
//! shell is the single owner of the update state; the renderer only receives
//! `ZCode`'s `UpdateStatePayload` / `UpdateCheckResultPayload` and calls
//! `updater_call`. Install always drains the Host first.

#![allow(clippy::needless_pass_by_value)]

#[cfg(any(target_os = "android", target_os = "ios"))]
use harness_protocol::CommandError;

#[cfg(not(any(target_os = "android", target_os = "ios")))]
mod model;
#[cfg(not(any(target_os = "android", target_os = "ios")))]
mod runtime;
#[cfg(not(any(target_os = "android", target_os = "ios")))]
mod store;
// Windows only: the plugin's leftover installer directories in %TEMP% (spec section 4.1).
#[cfg(windows)]
mod temp_sweep;

#[cfg(windows)]
pub(crate) use temp_sweep::sweep_leftover_installers;

#[cfg(not(any(target_os = "android", target_os = "ios")))]
// 通配导入：tauri::command 生成的隐藏宏（__cmd__*）也要一并导出，generate_handler! 才能找到。
pub(crate) use runtime::*;

/// Mobile shells are remote clients distributed through app stores: no updater.
#[cfg(any(target_os = "android", target_os = "ios"))]
#[tauri::command]
#[allow(clippy::unused_async)]
pub(crate) async fn updater_call(method: String) -> Result<(), CommandError> {
    Err(crate::error::unsupported(format!(
        "updater `{method}` is not available on a remote-control client"
    )))
}
