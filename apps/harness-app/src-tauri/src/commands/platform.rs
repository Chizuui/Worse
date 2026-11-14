//! Native platform commands used by the frontend.

#![allow(clippy::needless_pass_by_value)]

use std::path::Path;

use crate::{error, state::AppState};
use harness_platform::ValidatedUrl;
use harness_protocol::{
    CommandError, OpenExternalRequest, RevealPathRequest, SelectDirectoryResponse,
    SelectFileResponse,
};
use tauri::{AppHandle, State};
use tauri_plugin_dialog::{DialogExt, FilePath};

/// Opens an external URL after enforcing the shared scheme allowlist.
#[tauri::command(async)]
pub(crate) fn open_external(
    state: State<'_, AppState>,
    request: OpenExternalRequest,
) -> Result<(), CommandError> {
    let OpenExternalRequest { url } = request;
    let url = ValidatedUrl::parse(&url).map_err(CommandError::from)?;
    state.url_opener().open(&url).map_err(CommandError::from)
}

/// Reveals a path in the platform file manager.
#[tauri::command(async)]
pub(crate) fn reveal_path(
    state: State<'_, AppState>,
    request: RevealPathRequest,
) -> Result<(), CommandError> {
    if !harness_platform::capabilities_for(state.surface()).reveal_path {
        return Err(error::unsupported(
            "path reveal is unavailable on this surface",
        ));
    }

    let RevealPathRequest { path } = request;
    state
        .path_revealer()
        .reveal(Path::new(&path))
        .map_err(CommandError::from)
}

/// Opens the native directory picker without blocking the webview thread.
#[tauri::command]
pub(crate) async fn select_directory(
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<SelectDirectoryResponse, CommandError> {
    ensure_directory_selection(&state)?;
    pick_folder(app).await
}

#[cfg(desktop)]
async fn pick_folder(app: AppHandle) -> Result<SelectDirectoryResponse, CommandError> {
    let selection =
        tauri::async_runtime::spawn_blocking(move || app.dialog().file().blocking_pick_folder())
            .await
            .map_err(|join_error| {
                error::internal(format!("directory picker task failed: {join_error}"))
            })?;

    Ok(SelectDirectoryResponse {
        path: selection.map(selected_path).transpose()?,
    })
}

// 修复原因：tauri-plugin-dialog 在 Android/iOS 上没有 blocking_pick_folder，
// 移动端构建因此编译失败。移动端能力表本就不提供目录选择，这里直接返回 unsupported。
#[cfg(mobile)]
#[allow(clippy::unused_async)]
async fn pick_folder(_app: AppHandle) -> Result<SelectDirectoryResponse, CommandError> {
    Err(error::unsupported(
        "directory selection is unavailable on this surface",
    ))
}

/// Opens the native file picker without blocking the webview thread.
#[tauri::command]
pub(crate) async fn select_file(
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<SelectFileResponse, CommandError> {
    ensure_file_selection(&state)?;

    let selection =
        tauri::async_runtime::spawn_blocking(move || app.dialog().file().blocking_pick_file())
            .await
            .map_err(|join_error| {
                error::internal(format!("file picker task failed: {join_error}"))
            })?;

    Ok(SelectFileResponse {
        path: selection.map(selected_path).transpose()?,
    })
}

fn ensure_directory_selection(state: &AppState) -> Result<(), CommandError> {
    if harness_platform::capabilities_for(state.surface()).select_directory {
        Ok(())
    } else {
        Err(error::unsupported(
            "directory selection is unavailable on this surface",
        ))
    }
}

fn ensure_file_selection(state: &AppState) -> Result<(), CommandError> {
    if harness_platform::capabilities_for(state.surface()).select_file {
        Ok(())
    } else {
        Err(error::unsupported(
            "file selection is unavailable on this surface",
        ))
    }
}

fn selected_path(file: FilePath) -> Result<String, CommandError> {
    file.into_path()
        .map(|path| path.to_string_lossy().into_owned())
        .map_err(|path_error| {
            error::invalid_input(format!("selected path is not a local file: {path_error}"))
        })
}
