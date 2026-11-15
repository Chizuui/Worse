//! Harness native UI (Slint), Phase 8 round 1. Spec: `docs/specs/harness/phase-8-ui.md`.
//!
//! Main thread: Slint event loop. One `harness-core` thread: tokio current-thread runtime with the core,
//! the event pump and the file lister.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod agy_cred;
mod agy_usage;
mod bridge;
mod chat;
mod computer_mcp;
mod core;
mod files;
mod gitui;
mod highlight;
mod import;
mod inbox;
mod live_update;
mod markdown;
mod orchestra;
mod panels;
mod plan_usage;
mod remote;
mod settings;
mod setup;
mod workarea;

#[allow(
    clippy::all,
    clippy::pedantic,
    clippy::unwrap_used,
    clippy::expect_used
)]
mod ui {
    slint::include_modules!();
}

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;

use harness_settings::{relocation, SettingsStore};
use slint::ComponentHandle;
use tokio::sync::mpsc;

/// `HARNESS_HOME`, else `~/.harness`.
fn harness_home() -> PathBuf {
    if let Some(home) = std::env::var_os("HARNESS_HOME").filter(|v| !v.is_empty()) {
        return PathBuf::from(home);
    }
    let user_home = std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" })
        .map_or_else(|| PathBuf::from("."), PathBuf::from);
    user_home.join(".harness")
}

fn workspace(last: Option<&str>) -> PathBuf {
    if let Some(dir) = std::env::var_os("HARNESS_WORKSPACE").filter(|v| !v.is_empty()) {
        return PathBuf::from(dir);
    }
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    // The exe that owns the shipped files: the source exe when this is a live-update shadow copy.
    let exe_dir = live_update::launch_exe().parent().map(Path::to_path_buf);
    let same = |a: &Path, b: &Path| match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    };
    let root = cwd.parent().is_none();
    if root || exe_dir.is_some_and(|dir| same(&dir, &cwd)) {
        if let Some(last) = last.map(PathBuf::from).filter(|p| p.is_dir()) {
            return last;
        }
    }
    if root {
        if let Some(home) = std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" })
            .map(PathBuf::from)
            .filter(|p| p.is_dir())
        {
            return home;
        }
    }
    cwd
}

fn prepare_process() {
    #[cfg(target_os = "macos")]
    fix_macos_path();
    select_renderer();
}

#[cfg(target_os = "macos")]
fn login_shell_path() -> Option<String> {
    use std::io::Read;
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};
    const MARK: &str = "__HARNESS_PATH__";
    let shell = std::env::var_os("SHELL")
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "/bin/zsh".into());
    let mut child = Command::new(shell)
        .args([
            "-l",
            "-i",
            "-c",
            &format!("printf '{MARK}%s{MARK}' \"$PATH\""),
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(50)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
    let mut out = String::new();
    child.stdout.take()?.read_to_string(&mut out).ok()?;
    out.split(MARK).nth(1).map(str::to_owned)
}

#[cfg(target_os = "macos")]
fn fix_macos_path() {
    let mut dirs: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).collect())
        .unwrap_or_default();
    let mut extra: Vec<PathBuf> = login_shell_path()
        .map(|p| std::env::split_paths(&p).collect())
        .unwrap_or_default();
    extra.push(PathBuf::from("/opt/homebrew/bin"));
    extra.push(PathBuf::from("/usr/local/bin"));
    if let Some(home) = std::env::var_os("HOME").filter(|h| !h.is_empty()) {
        extra.push(PathBuf::from(home).join(".local").join("bin"));
    }
    for dir in extra {
        if !dir.as_os_str().is_empty() && !dirs.contains(&dir) {
            dirs.push(dir);
        }
    }
    if let Ok(joined) = std::env::join_paths(&dirs) {
        std::env::set_var("PATH", joined);
    }
}

static RELAUNCH: std::sync::Mutex<Option<PathBuf>> = std::sync::Mutex::new(None);

fn install_restart_for_update(window: &ui::AppWindow, workspace: &Path) {
    let dir = workspace.to_path_buf();
    window.on_restart_for_update(move || {
        if let Ok(mut slot) = RELAUNCH.lock() {
            *slot = Some(dir.clone());
        }
        let _ = slint::quit_event_loop();
    });
}

fn relaunch() {
    let Some(dir) = RELAUNCH.lock().ok().and_then(|mut slot| slot.take()) else {
        return;
    };
    // A shadow copy restarts the newest build (its source exe), not itself.
    let exe = live_update::launch_exe();
    if let Err(error) = std::process::Command::new(exe)
        .current_dir(&dir)
        .env("HARNESS_WORKSPACE", &dir)
        .spawn()
    {
        eprintln!(
            "harness-slint: cannot restart in {}: {error}",
            dir.display()
        );
    }
}

/// Renderer choice (phase-10 Midnight Glass UI §2): Skia by default, then femtovg, then software.
/// `HARNESS_RENDERER=femtovg|software` puts that renderer first, `SLINT_BACKEND` overrides all.
fn select_renderer() {
    if std::env::var_os("SLINT_BACKEND").is_some_and(|v| !v.is_empty()) {
        return;
    }
    let order: &[&str] = match std::env::var("HARNESS_RENDERER").as_deref() {
        Ok("software") => &["software", "skia", "femtovg"],
        Ok("femtovg") => &["femtovg", "skia", "software"],
        _ => &["skia", "femtovg", "software"],
    };
    for &renderer in order {
        match slint::BackendSelector::new()
            .backend_name("winit".into())
            .renderer_name(renderer.into())
            .select()
        {
            Ok(()) => return,
            Err(error) => eprintln!("harness-slint: {renderer} renderer unavailable: {error}"),
        }
    }
}

/// Gates only (automation builds): `HARNESS_UI_WINDOW_SIZE=<w>x<h>` opens the window at that logical size, so the
/// narrow settings layout can be driven without a resize tool.
#[cfg(feature = "automation")]
fn test_window_size(window: &ui::AppWindow) {
    let Some(value) = std::env::var("HARNESS_UI_WINDOW_SIZE").ok() else {
        return;
    };
    let Some((w, h)) = value.split_once('x') else {
        return;
    };
    if let (Ok(w), Ok(h)) = (w.trim().parse::<f32>(), h.trim().parse::<f32>()) {
        window.window().set_size(slint::LogicalSize::new(w, h));
    }
}

/// Startup (spec §6.3): applies a relocation scheduled by the previous run, resolves the data directory and opens
/// the one `SettingsStore` of the process, before the first frame (so it already has the stored theme and language).
fn boot(base: &Path) -> Option<(PathBuf, Arc<SettingsStore>, PathBuf)> {
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("harness-slint: cannot start boot runtime: {error}");
            return None;
        }
    };
    runtime.block_on(async {
        if let Err(error) = relocation::apply_pending(base).await {
            eprintln!("harness-slint: data directory move failed, keeping the old one: {error}");
        }
        let data_dir = relocation::resolve_data_dir(base).await.data_dir;
        let store = match SettingsStore::open(&data_dir).await {
            Ok(store) => store,
            Err(error) => {
                eprintln!("harness-slint: settings store failed: {error}");
                return None;
            }
        };
        let last = store.snapshot().general.last_workspace.clone();
        let workspace = workspace(last.as_deref());
        let resolved = workspace.to_string_lossy().into_owned();
        if last.as_deref() != Some(resolved.as_str()) {
            if let Err(error) = store
                .patch(|s| s.general.last_workspace = Some(resolved))
                .await
            {
                eprintln!("harness-slint: cannot store the workspace: {error}");
            }
        }
        Some((data_dir, Arc::new(store), workspace))
    })
}

/// Helper modes of the single executable (phase-9-parity.md §9.9). Checked before any Slint, settings or tokio
/// setup: a helper process only speaks MCP / ACP on its piped stdio. Under the Windows GUI subsystem the parent's
/// pipes still arrive through `STARTF_USESTDHANDLES`, so std/tokio stdio work unchanged.
fn helper_mode() -> Option<ExitCode> {
    use harness_agent_hub::helpers;
    let mut args = std::env::args().skip(1);
    let mode = args.next()?;
    match mode.as_str() {
        helpers::FLAG_MEMORY_MCP => Some(helpers::run_memory_mcp(args)),
        helpers::FLAG_DELEGATE => Some(helpers::run_delegate_mcp()),
        helpers::FLAG_API_AGENT => Some(harness_api_agent::run()),
        helpers::FLAG_COMPUTER_MCP => Some(computer_mcp::run()),
        _ => None,
    }
}

fn install_crash_log(base: &std::path::Path) {
    let dir = base.join("logs");
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        use std::io::Write;
        let when = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or_default();
        let thread = std::thread::current()
            .name()
            .unwrap_or("unnamed")
            .to_owned();
        let trace = std::backtrace::Backtrace::force_capture();
        let _ = std::fs::create_dir_all(&dir);
        if let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join("crash.log"))
        {
            let _ = writeln!(
                file,
                "=== panic at unix {when} on thread {thread}\n{info}\n{trace}\n"
            );
        }
        previous(info);
    }));
}

fn install_window_chrome(window: &ui::AppWindow) {
    use slint::winit_030::winit::window::ResizeDirection;
    use slint::winit_030::WinitWindowAccessor;
    let weak = window.as_weak();
    let last_down = std::cell::Cell::new(None::<std::time::Instant>);
    let saved = std::rc::Rc::new(std::cell::Cell::new(None::<workarea::Rect>));
    let toggle = {
        let saved = std::rc::Rc::clone(&saved);
        move |window: &ui::AppWindow| {
            let handle = window.window();
            if let Some(rect) = saved.take() {
                workarea::restore(handle, rect);
                window.set_win_maximized(false);
            } else if handle.is_maximized() {
                handle.set_maximized(false);
                window.set_win_maximized(false);
            } else if let Some(rect) = workarea::maximize(handle) {
                saved.set(Some(rect));
                window.set_win_maximized(true);
            } else {
                handle.set_maximized(true);
                window.set_win_maximized(true);
            }
        }
    };
    window.on_window_action(move |action| {
        let Some(window) = weak.upgrade() else {
            return;
        };
        let handle = window.window();
        match action.as_str() {
            "sync" => window.set_win_maximized(saved.get().is_some() || handle.is_maximized()),
            "minimize" => handle.set_minimized(true),
            "maximize" => toggle(&window),
            "close" => handle.dispatch_event(slint::platform::WindowEvent::CloseRequested),
            "drag" => {
                let now = std::time::Instant::now();
                let double = last_down.get().is_some_and(|at| {
                    now.duration_since(at) < std::time::Duration::from_millis(400)
                });
                if double {
                    last_down.set(None);
                    toggle(&window);
                } else {
                    last_down.set(Some(now));
                    // A press on a maximized window only arms the drag: un-maximizing under the
                    // pointer happens on the first real move ("drag-restore"), like the native
                    // title bar. Dragging a maximized window directly does nothing on Windows —
                    // that was the "locked" title bar.
                    if saved.get().is_none() && !handle.is_maximized() {
                        handle.with_winit_window(|w| {
                            let _ = w.drag_window();
                        });
                    }
                }
            }
            rest if rest.starts_with("drag-restore:") => {
                // "drag-restore:<x per-mille of the title bar width>:<pointer y in logical px>"
                let mut fields = rest["drag-restore:".len()..].split(':');
                let fraction = fields
                    .next()
                    .and_then(|v| v.parse::<f64>().ok())
                    .map_or(0.5, |per_mille| (per_mille / 1000.0).clamp(0.0, 1.0));
                let top = fields
                    .next()
                    .and_then(|v| v.parse::<f64>().ok())
                    .unwrap_or(0.0);
                let top = (top * f64::from(handle.scale_factor())).round() as i32;
                let rect = saved.take();
                if !workarea::place_dragged(handle, rect, fraction, top) {
                    // No Win32 placement (or no cursor): fall back to the saved rect.
                    if let Some(rect) = rect {
                        workarea::restore(handle, rect);
                    }
                }
                // winit caches the maximized flag: drop it so the restored geometry sticks.
                if handle.is_maximized() {
                    handle.set_maximized(false);
                }
                window.set_win_maximized(false);
                handle.with_winit_window(|w| {
                    let _ = w.drag_window();
                });
            }
            other => {
                let direction = match other.strip_prefix("resize-") {
                    Some("n") => ResizeDirection::North,
                    Some("s") => ResizeDirection::South,
                    Some("e") => ResizeDirection::East,
                    Some("w") => ResizeDirection::West,
                    Some("ne") => ResizeDirection::NorthEast,
                    Some("nw") => ResizeDirection::NorthWest,
                    Some("se") => ResizeDirection::SouthEast,
                    Some("sw") => ResizeDirection::SouthWest,
                    _ => return,
                };
                handle.with_winit_window(|w| {
                    let _ = w.drag_resize_window(direction);
                });
            }
        }
    });
}

fn main() -> ExitCode {
    if let Some(code) = helper_mode() {
        return code;
    }
    // Live update: run from a shadow copy so the build output stays writable.
    if let Some(code) = live_update::shadow_launch() {
        return code;
    }
    // Then drop the shadow variables from the environment, so children do not inherit them.
    live_update::take_shadow_env();
    prepare_process();
    let window = match ui::AppWindow::new() {
        Ok(window) => window,
        Err(error) => {
            eprintln!("harness-slint: cannot create window: {error}");
            return ExitCode::FAILURE;
        }
    };
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    let (event_tx, event_rx) = mpsc::unbounded_channel();
    let (files_tx, files_rx) = mpsc::unbounded_channel();
    let (settings_tx, settings_rx) = mpsc::unbounded_channel();
    let base = harness_home();
    install_crash_log(&base);
    let use_fake = std::env::var("HARNESS_UI_CORE").is_ok_and(|v| v == "fake");
    let Some((data_dir, store, workspace_dir)) = boot(&base) else {
        return ExitCode::FAILURE;
    };
    let core_workspace = workspace_dir.clone();
    let initial = store.snapshot();
    // 窗口框架只读启动时的设置：winit 不能在运行中可靠地增删窗口装饰，所以 app.slint 的 no-frame
    // 绑定 native-frame，设置页提示“重启生效”。
    window.set_native_frame(initial.appearance.native_frame);
    let worker = settings::Worker {
        store: Some(Arc::clone(&store)),
        core: cmd_tx.clone(),
        base: base.clone(),
        data_dir: data_dir.clone(),
        workspace: workspace_dir.clone(),
    };
    let home = data_dir.clone();
    let core_thread = std::thread::Builder::new()
        .name("harness-core".into())
        .spawn(move || {
            let runtime = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime,
                Err(error) => {
                    eprintln!("harness-slint: cannot start core runtime: {error}");
                    return;
                }
            };
            // One SettingsStore per process (R6): the core and the settings worker share the same Arc.
            let core = async move {
                if use_fake {
                    core::fake::run(home, store, cmd_rx, event_tx).await;
                } else {
                    core::hub::run(home, core_workspace, store, cmd_rx, event_tx).await;
                }
            };
            runtime.block_on(async move {
                tokio::join!(
                    core,
                    bridge::pump(event_rx),
                    files::serve(files_rx),
                    worker.serve(settings_rx)
                );
            });
        });
    let core_thread = match core_thread {
        Ok(handle) => handle,
        Err(error) => {
            eprintln!("harness-slint: cannot spawn core thread: {error}");
            return ExitCode::FAILURE;
        }
    };
    #[cfg(feature = "automation")]
    test_window_size(&window);
    let first_run_done = initial.setup.first_run_done;
    settings::install(&window, settings_tx, initial, &data_dir);
    // §9.10：首次启动检查与 Set up agents 对话框。
    setup::install(&window, first_run_done);
    chat::install(&window, files_tx.clone(), &workspace_dir, &data_dir);
    bridge::install(&window, cmd_tx, files_tx, &workspace_dir);
    panels::install(&window);
    gitui::install(&window);
    orchestra::install(&window);
    inbox::install(&window);
    import::install(&window);
    remote::install(&window, &data_dir);
    plan_usage::install(&window);
    install_window_chrome(&window);
    install_restart_for_update(&window, &workspace_dir);
    if live_update::is_shadow() {
        live_update::cleanup_old_copies();
        live_update::watch(window.as_weak());
    }
    let result = window.run();
    panels::trace("main: event loop returned");
    remote::shutdown();
    settings::teardown();
    bridge::teardown();
    chat::teardown();
    panels::teardown();
    drop(window);
    panels::trace("main: joining core thread");
    if core_thread.join().is_err() {
        eprintln!("harness-slint: core thread panicked");
        return ExitCode::FAILURE;
    }
    relaunch();
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("harness-slint: event loop error: {error}");
            ExitCode::FAILURE
        }
    }
}
