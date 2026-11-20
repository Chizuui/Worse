use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use slint::{ComponentHandle, SharedString, Timer, TimerMode};

use crate::ui::{AppWindow, PerformanceUi};

const TICK: Duration = Duration::from_millis(200);
const LIMIT_REACHED: &str = "Recording finished";

#[derive(Default)]
struct State {
    window: Option<slint::Weak<AppWindow>>,
    directory: PathBuf,
    duration: u64,
    timer: Timer,
    frame_start: Cell<Option<Instant>>,
    frame_span: RefCell<Option<harness_perf::Span>>,
}

thread_local! {
    static STATE: RefCell<State> = RefCell::new(State {
        duration: 20,
        ..State::default()
    });
}

fn with_ui(apply: impl FnOnce(PerformanceUi<'_>)) {
    let window = STATE.with(|cell| cell.borrow().window.as_ref().and_then(slint::Weak::upgrade));
    if let Some(window) = window {
        apply(window.global::<PerformanceUi>());
    }
}

pub(crate) fn install(window: &AppWindow, data_dir: &Path) {
    STATE.with(|cell| {
        let mut state = cell.borrow_mut();
        state.window = Some(window.as_weak());
        state.directory = data_dir.join("traces");
    });

    let ui = window.global::<PerformanceUi>();
    ui.on_toggle(toggle);
    ui.on_set_duration(|value| {
        let seconds = value.parse::<u64>().unwrap_or(20).clamp(10, 60);
        STATE.with(|cell| cell.borrow_mut().duration = seconds);
        with_ui(|ui| ui.set_duration(SharedString::from(seconds.to_string())));
    });
    ui.on_reveal(reveal);
    ui.on_open_perfetto(open_perfetto);
    with_ui(|ui| ui.set_duration(SharedString::from("20")));
    install_frame_timing(window);
}

fn install_frame_timing(window: &AppWindow) {
    if !crate::gpu_rendering() {
        frame_timing_unavailable();
        return;
    }
    let result = window.window().set_rendering_notifier(|state, _api| {
        let now = Instant::now();
        match state {
            slint::RenderingState::BeforeRendering => {
                STATE.with(|cell| {
                    let cell = cell.borrow_mut();
                    cell.frame_start.set(Some(now));
                    cell.frame_span
                        .borrow_mut()
                        .replace(harness_perf::span("frame"));
                });
            }
            slint::RenderingState::AfterRendering => {
                let started = STATE.with(|cell| cell.borrow_mut().frame_start.take());
                let span = STATE.with(|cell| cell.borrow_mut().frame_span.borrow_mut().take());
                if let Some(span) = span {
                    span.end();
                }
                if let Some(started) = started {
                    let millis = now.saturating_duration_since(started).as_secs_f64() * 1000.0;
                    harness_perf::counter_float(harness_perf::Counter::FrameMilliseconds, millis);
                }
            }
            _ => {}
        }
    });
    if result.is_err() {
        frame_timing_unavailable();
    }
}

fn frame_timing_unavailable() {
    with_ui(|ui| {
        ui.set_status(SharedString::from(
            "Frame timing is not available with this renderer; other tracks are still recorded.",
        ));
    });
}

fn toggle() {
    if harness_perf::is_recording() {
        finish(false);
        return;
    }
    match harness_perf::start() {
        Ok(()) => {
            with_ui(|ui| {
                ui.set_recording(true);
                ui.set_slices(0);
                ui.set_elapsed(SharedString::from("0.0 s"));
                ui.set_truncated(false);
                ui.set_status(SharedString::from("Recording…"));
                ui.set_status_error(false);
            });
            start_timer();
        }
        Err(error) => with_ui(|ui| {
            ui.set_recording(false);
            ui.set_status(SharedString::from(error.to_string()));
            ui.set_status_error(true);
        }),
    }
}

fn start_timer() {
    let started = Instant::now();
    let weak = STATE.with(|cell| cell.borrow().window.clone());
    STATE.with(|cell| {
        let state = cell.borrow_mut();
        let limit = Duration::from_secs(state.duration.max(5));
        state.timer.start(TimerMode::Repeated, TICK, move || {
            let elapsed = started.elapsed();
            let slices = harness_perf::slice_count();
            with_ui(|ui| {
                ui.set_elapsed(SharedString::from(format!(
                    "{:.1} s",
                    elapsed.as_secs_f64()
                )));
                ui.set_slices(i32::try_from(slices).unwrap_or(i32::MAX));
            });
            if elapsed >= limit && weak.is_some() {
                finish(true);
            }
        });
    });
}

fn finish(reached_limit: bool) {
    STATE.with(|cell| cell.borrow().timer.stop());
    let directory = STATE.with(|cell| cell.borrow().directory.clone());
    let result = harness_perf::stop(&directory);
    with_ui(|ui| {
        ui.set_recording(false);
        ui.set_slices(0);
        ui.set_elapsed(SharedString::default());
        match result {
            Ok(written) => {
                ui.set_last_path(SharedString::from(written.path.display().to_string()));
                ui.set_truncated(written.truncated);
                ui.set_status(SharedString::from(if reached_limit {
                    format!(
                        "{LIMIT_REACHED}: {} slices in {:.1} s.",
                        written.slices,
                        written.duration.as_secs_f64()
                    )
                } else {
                    format!(
                        "Saved {} slices in {:.1} s.",
                        written.slices,
                        written.duration.as_secs_f64()
                    )
                }));
                ui.set_status_error(false);
            }
            Err(error) => {
                ui.set_status(SharedString::from(error.to_string()));
                ui.set_status_error(true);
            }
        }
    });
}

fn reveal() {
    use harness_platform::PathRevealer;
    let path = with_ui_path();
    let Some(path) = path else {
        return;
    };
    let _ = harness_platform::desktop::SystemPathRevealer::new().reveal(&path);
}

fn open_perfetto() {
    open_url("https://ui.perfetto.dev");
}

/// Opens an `https://` link in the default browser.
fn open_url(url: &str) {
    if !url.starts_with("https://") {
        return;
    }
    let program = if cfg!(windows) {
        "explorer.exe"
    } else if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    let _ = std::process::Command::new(program).arg(url).spawn();
}

fn with_ui_path() -> Option<PathBuf> {
    let window = STATE.with(|cell| cell.borrow().window.as_ref().and_then(slint::Weak::upgrade))?;
    let path = window.global::<PerformanceUi>().get_last_path();
    if path.is_empty() {
        return None;
    }
    Some(PathBuf::from(path.as_str()))
}
