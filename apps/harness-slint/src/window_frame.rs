//! Window corners and frame on Windows 11.
//!
//! With the custom title bar (Settings › Appearance › "Use Windows title bar" off) winit makes the window
//! frameless, and a frameless window on Windows 11 has square corners and no shadow. Two winit settings ask
//! the desktop compositor (DWM) for what a normal window gets: rounded corners (`DWMWCP_ROUND`, drawn and
//! anti-aliased by DWM, squared again by DWM while the window is maximized or snapped) and the drop shadow
//! (winit keeps a 1 px non-client strip so DWM treats the window as framed). The frame follows the app's
//! light / dark choice, so the thin border DWM draws around a rounded window is dark in the dark theme.
//!
//! Windows 10 has no corner preference: the window stays square there. Other platforms are not touched.

use slint::winit_030::winit::window::WindowAttributes;

use crate::ui::AppWindow;

/// Applied to the main window's attributes before winit creates it (see `select_renderer`).
#[must_use]
pub fn attributes(attributes: WindowAttributes) -> WindowAttributes {
    #[cfg(windows)]
    {
        use slint::winit_030::winit::platform::windows::{
            CornerPreference, WindowAttributesExtWindows,
        };
        attributes
            .with_corner_preference(CornerPreference::Round)
            .with_undecorated_shadow(true)
    }
    #[cfg(not(windows))]
    {
        attributes
    }
}

#[cfg(windows)]
thread_local! {
    /// The app's theme choice for the frame: `None` follows the system, `Some(true)` dark, `Some(false)` light.
    static WANTED: std::cell::Cell<Option<bool>> = const { std::cell::Cell::new(None) };
    /// Applies `WANTED` once the winit window exists (it is created after the event loop starts), then stops.
    static RETRY: slint::Timer = slint::Timer::default();
}

/// Makes the DWM frame follow the app theme. `pref` is `Theme.pref`: 0 system, 1 light, 2 dark.
pub fn follow_theme(window: &AppWindow, pref: i32) {
    #[cfg(windows)]
    {
        use slint::ComponentHandle;
        WANTED.with(|cell| {
            cell.set(match pref {
                1 => Some(false),
                2 => Some(true),
                _ => None,
            });
        });
        if apply(window.window()) {
            return;
        }
        // Settings arrive before the window is shown: try again every 50 ms until it exists.
        let weak = window.as_weak();
        RETRY.with(|timer| {
            timer.start(
                slint::TimerMode::Repeated,
                std::time::Duration::from_millis(50),
                move || {
                    let done = weak.upgrade().is_none_or(|w| apply(w.window()));
                    if done {
                        RETRY.with(slint::Timer::stop);
                    }
                },
            );
        });
    }
    #[cfg(not(windows))]
    {
        let _ = (window, pref);
    }
}

/// Sets the frame theme; false while the winit window does not exist yet.
#[cfg(windows)]
fn apply(window: &slint::Window) -> bool {
    use slint::winit_030::winit::window::Theme;
    use slint::winit_030::WinitWindowAccessor;
    let wanted = WANTED.with(std::cell::Cell::get);
    window
        .with_winit_window(|w| {
            // `set_theme` only restyles the frame. winit's record of the system theme, which Slint reads for
            // "System", is left alone.
            w.set_theme(wanted.map(|dark| if dark { Theme::Dark } else { Theme::Light }));
        })
        .is_some()
}
