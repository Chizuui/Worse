#![forbid(unsafe_code)]

mod commands;
mod error;
#[cfg(not(any(target_os = "android", target_os = "ios")))]
mod host;
// Phase 7 (parity B): kill switch that routes the user back to ZCode, spec phase-7-parity.md.
#[cfg(not(any(target_os = "android", target_os = "ios")))]
mod kill_switch;
mod plugin_sandbox;
mod remote_control;
mod state;
// Phase 6 (updater B): signed auto-update, spec phase-6-updater.md.
mod updater;

use tauri::Manager;

/// Release log file size before rotation.
#[cfg(not(debug_assertions))]
const RELEASE_LOG_FILE_BYTES: u128 = 5 * 1024 * 1024;
/// Rotated release log files kept next to the current one.
#[cfg(not(debug_assertions))]
const RELEASE_LOG_FILES_KEPT: usize = 3;

fn log_plugin<R: tauri::Runtime>() -> tauri::plugin::TauriPlugin<R> {
    let builder =
        tauri_plugin_log::Builder::new()
            .clear_targets()
            .target(tauri_plugin_log::Target::new(
                tauri_plugin_log::TargetKind::Stdout,
            ));

    // 修复原因（Phase 3c 打包验证）：插件默认单文件 40 KB、只保留一份、级别 Trace，Host 的逐条
    // RPC 日志在启动后约一秒内就把布局解析、资源检查、Rust 服务启用等启动日志轮转掉，打包版
    // 无法事后排查。改为 Debug 级别、单文件 5 MiB、保留 3 份历史。
    #[cfg(not(debug_assertions))]
    let builder = builder
        .level(log::LevelFilter::Debug)
        .max_file_size(RELEASE_LOG_FILE_BYTES)
        .rotation_strategy(tauri_plugin_log::RotationStrategy::KeepSome(
            RELEASE_LOG_FILES_KEPT,
        ))
        .target(tauri_plugin_log::Target::new(
            tauri_plugin_log::TargetKind::LogDir { file_name: None },
        ));

    builder.build()
}

/// Routes window close through the Host shutdown barrier on desktop.
///
/// The first close request is deferred until the Host is disposed and the
/// sidecar has exited; the barrier runs off the main thread.
#[cfg(not(any(target_os = "android", target_os = "ios")))]
fn on_window_event<R: tauri::Runtime>(window: &tauri::Window<R>, event: &tauri::WindowEvent) {
    let tauri::WindowEvent::CloseRequested { api, .. } = event else {
        return;
    };
    let app = window.app_handle().clone();
    let Some(state) = app.try_state::<state::AppState>() else {
        return;
    };
    api.prevent_close();
    if state.begin_exit() {
        // A barrier is already running; it ends the process with `app.exit`.
        return;
    }
    tauri::async_runtime::spawn(async move {
        if let Some(state) = app.try_state::<state::AppState>() {
            state.host().shutdown_for_exit().await;
        }
        // Phase 6 (updater B): macOS/Linux install a verified update after the Host drained.
        updater::install_on_quit(&app).await;
        // Phase 6 fix round: the installer that relaunched this version has long exited by now.
        #[cfg(windows)]
        updater::sweep_leftover_installers(&app, "exit").await;
        app.exit(0);
    });
}

/// Mobile shells own no Host, so closing needs no barrier.
#[cfg(any(target_os = "android", target_os = "ios"))]
fn on_window_event<R: tauri::Runtime>(_window: &tauri::Window<R>, _event: &tauri::WindowEvent) {}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // Phase 7：总开关打开时 Harness 不启动（不建窗口、不起 Host、不写共享数据），直接把用户交回 ZCode。
    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    if let Some(switch) = kill_switch::engaged() {
        switch.route_to_zcode();
        return;
    }

    let builder = tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(log_plugin())
        .setup(|app| {
            app.manage(state::AppState::new(app.handle()));
            #[cfg(not(any(target_os = "android", target_os = "ios")))]
            app.manage(remote_control::WebRemoteCalls::default());
            // Phase 5 (plugins): in-flight hsandbox fetch / dispose calls to the host-node sandbox owner.
            #[cfg(not(any(target_os = "android", target_os = "ios")))]
            app.manage(plugin_sandbox::PluginSandboxCalls::default());
            // Phase 6 (updater B): owner of the update state; off unless a feed and key exist.
            #[cfg(not(any(target_os = "android", target_os = "ios")))]
            {
                let updater = updater::UpdaterRuntime::new(app.handle());
                app.manage(std::sync::Arc::clone(&updater));
                updater.start();
            }
            // Phase 6 fix round: remove %TEMP% installers left by earlier updates (off the main thread).
            #[cfg(windows)]
            {
                let handle = app.handle().clone();
                tauri::async_runtime::spawn(async move {
                    updater::sweep_leftover_installers(&handle, "startup").await;
                });
            }
            Ok(())
        })
        .on_window_event(on_window_event);

    // Phase 5 (plugins): plugin UI documents are served from the host-node sandbox owner.
    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    let builder = builder.register_asynchronous_uri_scheme_protocol(
        plugin_sandbox::SCHEME,
        plugin_sandbox::handle_scheme,
    );

    // Phase 6 (updater B): network + signature verification for the shell-owned updater.
    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    let builder = builder.plugin(tauri_plugin_updater::Builder::new().build());

    let builder = builder.invoke_handler(tauri::generate_handler![
        commands::app::get_app_info,
        commands::app::get_capabilities,
        commands::host::host_database_startup_control,
        commands::host::host_open_attachment,
        commands::host::host_status,
        commands::platform::open_external,
        commands::platform::reveal_path,
        commands::platform::select_directory,
        commands::platform::select_file,
        remote_control::remote_control_call,
        plugin_sandbox::plugin_sandbox_call,
        updater::updater_call,
    ]);

    if let Err(error) = builder.run(tauri::generate_context!()) {
        log::error!("harness application terminated: {error}");
    }
}
