pub type Rect = (i32, i32, i32, i32);

/// Restored size used when neither our saved rect nor the OS normal rect is usable.
#[cfg(windows)]
const FALLBACK_SIZE: (i32, i32) = (1280, 800);
/// A normal rect smaller than this is treated as garbage (minimized / never shown).
#[cfg(windows)]
const MIN_SIZE: (i32, i32) = (240, 160);

#[cfg(windows)]
fn hwnd_of(window: &slint::Window) -> Option<*mut core::ffi::c_void> {
    use slint::winit_030::winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use slint::winit_030::WinitWindowAccessor;
    window
        .with_winit_window(|w| {
            let handle = w.window_handle().ok()?;
            match handle.as_raw() {
                RawWindowHandle::Win32(raw) => Some(raw.hwnd.get() as *mut core::ffi::c_void),
                _ => None,
            }
        })
        .flatten()
}

#[cfg(windows)]
#[allow(unsafe_code)]
fn place(hwnd: *mut core::ffi::c_void, r: Rect) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{SetWindowPos, SWP_NOACTIVATE, SWP_NOZORDER};
    unsafe {
        SetWindowPos(
            hwnd,
            core::ptr::null_mut(),
            r.0,
            r.1,
            r.2 - r.0,
            r.3 - r.1,
            SWP_NOZORDER | SWP_NOACTIVATE,
        );
    }
}

/// Work area (screen minus taskbar / autohidden appbars) of the monitor nearest the window, in
/// physical pixels.
#[cfg(windows)]
#[allow(unsafe_code)]
fn work_rect(hwnd: *mut core::ffi::c_void) -> Option<windows_sys::Win32::Foundation::RECT> {
    use windows_sys::Win32::Foundation::RECT;
    use windows_sys::Win32::Graphics::Gdi::{
        GetMonitorInfoW, MonitorFromWindow, MONITORINFO, MONITOR_DEFAULTTONEAREST,
    };
    use windows_sys::Win32::UI::Shell::{
        SHAppBarMessage, ABE_BOTTOM, ABE_LEFT, ABE_RIGHT, ABE_TOP, ABM_GETAUTOHIDEBAREX, APPBARDATA,
    };
    let empty = RECT {
        left: 0,
        top: 0,
        right: 0,
        bottom: 0,
    };
    let mut info = MONITORINFO {
        cbSize: u32::try_from(core::mem::size_of::<MONITORINFO>()).unwrap_or(40),
        rcMonitor: empty,
        rcWork: empty,
        dwFlags: 0,
    };
    unsafe {
        let monitor = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
        if monitor.is_null() || GetMonitorInfoW(monitor, &mut info) == 0 {
            return None;
        }
        let mut work = info.rcWork;
        for edge in [ABE_LEFT, ABE_TOP, ABE_RIGHT, ABE_BOTTOM] {
            let mut data = APPBARDATA {
                cbSize: u32::try_from(core::mem::size_of::<APPBARDATA>()).unwrap_or(48),
                hWnd: core::ptr::null_mut(),
                uCallbackMessage: 0,
                uEdge: edge,
                rc: info.rcMonitor,
                lParam: 0,
            };
            if SHAppBarMessage(ABM_GETAUTOHIDEBAREX, &mut data) != 0 {
                match edge {
                    ABE_LEFT => work.left += 1,
                    ABE_TOP => work.top += 1,
                    ABE_RIGHT => work.right -= 1,
                    _ => work.bottom -= 1,
                }
            }
        }
        Some(work)
    }
}

/// The rect Windows keeps as the window's normal (pre-maximize) geometry.
#[cfg(windows)]
#[allow(unsafe_code)]
fn normal_rect(hwnd: *mut core::ffi::c_void) -> Option<Rect> {
    use windows_sys::Win32::Foundation::POINT;
    use windows_sys::Win32::UI::WindowsAndMessaging::{GetWindowPlacement, WINDOWPLACEMENT};
    let mut placement = WINDOWPLACEMENT {
        length: u32::try_from(core::mem::size_of::<WINDOWPLACEMENT>()).unwrap_or(44),
        flags: 0,
        showCmd: 0,
        ptMinPosition: POINT { x: 0, y: 0 },
        ptMaxPosition: POINT { x: 0, y: 0 },
        rcNormalPosition: windows_sys::Win32::Foundation::RECT {
            left: 0,
            top: 0,
            right: 0,
            bottom: 0,
        },
    };
    if unsafe { GetWindowPlacement(hwnd, &mut placement) } == 0 {
        return None;
    }
    let r = placement.rcNormalPosition;
    Some((r.left, r.top, r.right, r.bottom))
}

/// Cursor position in physical screen pixels (needed to keep the title bar under the pointer).
#[cfg(windows)]
#[allow(unsafe_code)]
fn cursor_pos() -> Option<(i32, i32)> {
    use windows_sys::Win32::Foundation::POINT;
    use windows_sys::Win32::UI::WindowsAndMessaging::GetCursorPos;
    let mut point = POINT { x: 0, y: 0 };
    unsafe { (GetCursorPos(&mut point) != 0).then_some((point.x, point.y)) }
}

#[cfg(windows)]
#[allow(unsafe_code)]
pub fn maximize(window: &slint::Window) -> Option<Rect> {
    use windows_sys::Win32::UI::WindowsAndMessaging::GetWindowRect;
    let hwnd = hwnd_of(window)?;
    let work = work_rect(hwnd)?;
    let mut current = windows_sys::Win32::Foundation::RECT {
        left: 0,
        top: 0,
        right: 0,
        bottom: 0,
    };
    if unsafe { GetWindowRect(hwnd, &mut current) } == 0 {
        return None;
    }
    place(hwnd, (work.left, work.top, work.right, work.bottom));
    Some((current.left, current.top, current.right, current.bottom))
}

#[cfg(windows)]
pub fn restore(window: &slint::Window, rect: Rect) {
    if let Some(hwnd) = hwnd_of(window) {
        place(hwnd, rect);
    }
}

/// Restored rect (size taken from `saved`, else the normal rect, else 1280x800) clamped to the
/// work area. `None` when the window has no Win32 handle.
#[cfg(windows)]
pub fn restored_rect(window: &slint::Window, saved: Option<Rect>) -> Option<Rect> {
    let hwnd = hwnd_of(window)?;
    let work = work_rect(hwnd)?;
    let (work_w, work_h) = (work.right - work.left, work.bottom - work.top);
    let preferred = saved.or_else(|| normal_rect(hwnd));
    let mut width = preferred.map_or(FALLBACK_SIZE.0, |r| r.2 - r.0);
    let mut height = preferred.map_or(FALLBACK_SIZE.1, |r| r.3 - r.1);
    if width < MIN_SIZE.0 || height < MIN_SIZE.1 {
        width = FALLBACK_SIZE.0;
        height = FALLBACK_SIZE.1;
    }
    let width = width.min(work_w).max(MIN_SIZE.0.min(work_w));
    let height = height.min(work_h).max(MIN_SIZE.1.min(work_h));
    let (left, top) = preferred.map_or((work.left, work.top), |r| (r.0, r.1));
    Some((left, top, left + width, top + height))
}

/// Un-maximize the window and keep the title bar under the cursor so the drag can continue:
/// `fraction` is the pointer x as a share of the title bar width (0..=1) and `top` the pointer y in
/// physical pixels from the window top, both captured when the drag started.
/// Returns false on platforms without Win32 (the caller then only restores the saved rect).
#[cfg(windows)]
#[allow(unsafe_code)]
pub fn place_dragged(
    window: &slint::Window,
    saved: Option<Rect>,
    fraction: f64,
    top: i32,
) -> bool {
    use windows_sys::Win32::Foundation::{POINT, RECT};
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        SetWindowPlacement, SW_SHOWNORMAL, WINDOWPLACEMENT,
    };
    let Some(hwnd) = hwnd_of(window) else {
        return false;
    };
    let Some(restored) = restored_rect(window, saved) else {
        return false;
    };
    let Some((cursor_x, cursor_y)) = cursor_pos() else {
        return false;
    };
    let width = restored.2 - restored.0;
    let height = restored.3 - restored.1;
    let fraction = fraction.clamp(0.0, 1.0);
    let left = cursor_x - (fraction * f64::from(width)).round() as i32;
    let top = cursor_y - top;
    let target = (left, top, left + width, top + height);
    let placement = WINDOWPLACEMENT {
        length: u32::try_from(core::mem::size_of::<WINDOWPLACEMENT>()).unwrap_or(44),
        flags: 0,
        showCmd: u32::try_from(SW_SHOWNORMAL).unwrap_or(1),
        ptMinPosition: POINT { x: 0, y: 0 },
        ptMaxPosition: POINT { x: 0, y: 0 },
        rcNormalPosition: RECT {
            left: target.0,
            top: target.1,
            right: target.2,
            bottom: target.3,
        },
    };
    unsafe {
        // Store the target as the normal rect and leave the maximized state: a later
        // SW_RESTORE (winit's set_maximized(false)) then lands on the same rect, not on the
        // pre-maximize position.
        SetWindowPlacement(hwnd, &placement);
        place(hwnd, target);
    }
    true
}

#[cfg(not(windows))]
pub fn maximize(_window: &slint::Window) -> Option<Rect> {
    None
}

#[cfg(not(windows))]
pub fn restore(_window: &slint::Window, _rect: Rect) {}

#[cfg(not(windows))]
pub fn place_dragged(
    _window: &slint::Window,
    _saved: Option<Rect>,
    _fraction: f64,
    _top: i32,
) -> bool {
    false
}
