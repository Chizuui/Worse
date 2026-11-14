//! Runtime dependencies shared by the Tauri commands.
//!
//! Desktop shells additionally own the window's local Host runtime
//! ([`crate::host::HostRuntime`]). Mobile shells never spawn a Host.

#[cfg(not(any(target_os = "android", target_os = "ios")))]
use std::sync::atomic::{AtomicBool, Ordering};

use harness_core::Surface;
use harness_platform::{PathRevealer, PlatformError, UrlOpener, ValidatedUrl};
use tauri::AppHandle;
use tauri_plugin_opener::OpenerExt;

#[cfg(not(any(target_os = "android", target_os = "ios")))]
use crate::host::HostRuntime;

/// Native adapters, surface information, and the local Host runtime.
pub(crate) struct AppState {
    surface: Surface,
    url_opener: Box<dyn UrlOpener>,
    path_revealer: Box<dyn PathRevealer>,
    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    exiting: AtomicBool,
    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    host: HostRuntime,
}

impl AppState {
    /// Builds the standard adapters for the active surface.
    pub(crate) fn new(app: &AppHandle) -> Self {
        Self {
            surface: Surface::current(),
            url_opener: Box::new(TauriUrlOpener { app: app.clone() }),
            path_revealer: create_path_revealer(),
            #[cfg(not(any(target_os = "android", target_os = "ios")))]
            exiting: AtomicBool::new(false),
            #[cfg(not(any(target_os = "android", target_os = "ios")))]
            host: HostRuntime::new(app),
        }
    }

    /// Returns the active deployment surface.
    pub(crate) const fn surface(&self) -> Surface {
        self.surface
    }

    /// Returns the external URL opener.
    pub(crate) fn url_opener(&self) -> &dyn UrlOpener {
        self.url_opener.as_ref()
    }

    /// Returns the native path revealer.
    pub(crate) fn path_revealer(&self) -> &dyn PathRevealer {
        self.path_revealer.as_ref()
    }

    /// Marks the shell as exiting; returns whether it already was.
    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    pub(crate) fn begin_exit(&self) -> bool {
        self.exiting.swap(true, Ordering::AcqRel)
    }

    /// Returns the window's local Host runtime.
    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    pub(crate) fn host(&self) -> &HostRuntime {
        &self.host
    }
}

struct TauriUrlOpener {
    app: AppHandle,
}

impl UrlOpener for TauriUrlOpener {
    fn open(&self, url: &ValidatedUrl) -> Result<(), PlatformError> {
        self.app
            .opener()
            .open_url(url.as_str(), None::<&str>)
            .map_err(|error| PlatformError::Io(format!("failed to open URL: {error}")))
    }
}

#[cfg(not(any(target_os = "android", target_os = "ios")))]
fn create_path_revealer() -> Box<dyn PathRevealer> {
    Box::new(harness_platform::desktop::SystemPathRevealer::new())
}

#[cfg(any(target_os = "android", target_os = "ios"))]
fn create_path_revealer() -> Box<dyn PathRevealer> {
    Box::new(UnsupportedPathRevealer)
}

#[cfg(any(target_os = "android", target_os = "ios"))]
struct UnsupportedPathRevealer;

#[cfg(any(target_os = "android", target_os = "ios"))]
impl PathRevealer for UnsupportedPathRevealer {
    fn reveal(&self, _path: &std::path::Path) -> Result<(), PlatformError> {
        Err(PlatformError::Unsupported)
    }
}
