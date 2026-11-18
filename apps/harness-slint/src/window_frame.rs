//! The window frame on Windows 11: corners, shadow, and the colours of the title bar and border.
//!
//! - With the custom title bar (Settings › Appearance › "Use Windows title bar" off) winit makes the window
//!   frameless, and a frameless window on Windows 11 has square corners and no shadow. Two winit settings ask
//!   the desktop compositor (DWM) for what a normal window gets: rounded corners (`DWMWCP_ROUND`, drawn and
//!   anti-aliased by DWM, squared again while the window is maximized or snapped) and the drop shadow.
//! - With the Windows title bar, the bar and the 1 px border are drawn by Windows in its own colours (the
//!   accent colour, or grey), which sits badly on the app's canvas. DWM takes a caption colour, a caption
//!   text colour and a border colour per window (`DWMWA_CAPTION_COLOR` …, Windows 11 build 22000+): they are
//!   set to the theme's canvas, text and hairline, and set again whenever the app switches between light and
//!   dark. The bar keeps its native buttons, snapping and resizing.
//!
//! Windows 10 ignores both (square corners, system colours). Other platforms are not touched.

use slint::winit_030::winit::window::WindowAttributes;

use crate::ui::AppWindow;

/// Canvas, text and hairline of the dark and the light theme (`ui/theme/tokens.slint`), as RGB. The hairline
/// is the theme's translucent border flattened onto the canvas.
#[cfg(any(windows, test))]
const DARK: Frame = Frame {
    canvas: (0x08, 0x09, 0x0a),
    text: (0xed, 0xee, 0xf0),
    border: (0x26, 0x28, 0x2c),
};
#[cfg(any(windows, test))]
const LIGHT: Frame = Frame {
    canvas: (0xf3, 0xf2, 0xee),
    text: (0x1a, 0x1b, 0x1d),
    border: (0xd3, 0xd2, 0xcd),
};

#[cfg(any(windows, test))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Frame {
    canvas: (u8, u8, u8),
    text: (u8, u8, u8),
    border: (u8, u8, u8),
}

#[cfg(any(windows, test))]
const fn frame(dark: bool) -> Frame {
    if dark {
        DARK
    } else {
        LIGHT
    }
}

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
    /// The theme the frame should show.
    static DARK_WANTED: std::cell::Cell<bool> = const { std::cell::Cell::new(true) };
    /// Applies it once the winit window exists (it is created after the event loop starts), then stops.
    static RETRY: slint::Timer = slint::Timer::default();
}

/// Tints the frame to the theme now and whenever the app changes between light and dark (a theme picked in
/// Settings, or the system's when the app follows it).
pub fn install(window: &AppWindow) {
    #[cfg(windows)]
    {
        use slint::ComponentHandle;
        let weak = window.as_weak();
        window.on_frame_dark_changed(move |dark| {
            if let Some(window) = weak.upgrade() {
                set_dark(&window, dark);
            }
        });
        set_dark(window, window.global::<crate::ui::Theme>().get_dark());
    }
    #[cfg(not(windows))]
    {
        let _ = window;
    }
}

#[cfg(windows)]
fn set_dark(window: &AppWindow, dark: bool) {
    use slint::ComponentHandle;
    DARK_WANTED.with(|cell| cell.set(dark));
    if apply(window.window()) {
        return;
    }
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

/// Sets the frame's theme and colours; false while the winit window does not exist yet.
#[cfg(windows)]
fn apply(window: &slint::Window) -> bool {
    use slint::winit_030::winit::platform::windows::{Color, WindowExtWindows};
    use slint::winit_030::winit::window::Theme;
    use slint::winit_030::WinitWindowAccessor;
    let dark = DARK_WANTED.with(std::cell::Cell::get);
    let colours = frame(dark);
    let rgb = |(r, g, b): (u8, u8, u8)| Color::from_rgb(r, g, b);
    window
        .with_winit_window(|w| {
            // Dark or light window chrome (buttons, menus). This restyles the frame only; winit's record of
            // the system theme, which Slint reads for "System", is left alone.
            w.set_theme(Some(if dark { Theme::Dark } else { Theme::Light }));
            w.set_title_background_color(Some(rgb(colours.canvas)));
            w.set_title_text_color(rgb(colours.text));
            w.set_border_color(Some(rgb(colours.border)));
        })
        .is_some()
}

#[cfg(test)]
mod tests {
    use super::{frame, DARK, LIGHT};

    #[test]
    fn frame_colours_follow_the_theme() {
        assert_eq!(frame(true), DARK);
        assert_eq!(frame(false), LIGHT);
        // The caption is the canvas, so the bar and the app read as one surface.
        assert_eq!(DARK.canvas, (0x08, 0x09, 0x0a));
        assert_eq!(LIGHT.canvas, (0xf3, 0xf2, 0xee));
        assert_ne!(DARK.border, DARK.canvas);
    }
}
