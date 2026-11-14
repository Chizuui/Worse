//! Platform boundary for the Harness shell.
//!
//! The crate has no dependency on Tauri: the composition root injects adapters
//! that implement the traits defined here. That keeps native behaviour testable
//! and keeps mobile builds free of desktop-only code.

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

mod capabilities;
#[cfg(not(any(target_os = "android", target_os = "ios")))]
pub mod desktop;
mod error;
mod traits;
mod url;

pub use capabilities::capabilities_for;
pub use error::PlatformError;
pub use traits::{PathRevealer, UrlOpener};
pub use url::ValidatedUrl;
