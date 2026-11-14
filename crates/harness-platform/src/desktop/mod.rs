//! Desktop-only native path adapters.
//!
//! URL opening is owned by `tauri-plugin-opener` in the shell; this module only
//! supplies a tested [`PathRevealer`](crate::PathRevealer) per desktop OS.

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "windows")]
mod windows;

#[cfg(target_os = "linux")]
pub use linux::SystemPathRevealer;
#[cfg(target_os = "macos")]
pub use macos::SystemPathRevealer;
#[cfg(target_os = "windows")]
pub use windows::SystemPathRevealer;
