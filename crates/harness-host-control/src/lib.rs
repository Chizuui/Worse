//! Desktop supervision of the Node Host sidecar.
//!
//! The crate owns process lifecycle, NDJSON routing, attachment grants, and
//! the shell-side cache of `database-startup-state`. It deliberately stores no
//! conversation, task, queue, or interaction state: the Node Host remains the
//! owner of everything that crosses the service boundary.
//!
//! Mobile surfaces never run a local Host, so the whole crate compiles away
//! outside desktop targets.

#![cfg(not(any(target_os = "android", target_os = "ios")))]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

mod attach;
mod config;
mod control;
mod controller;
mod dispatcher;
mod error;
mod fallback;
mod handshake;
mod routing;
mod shared;
mod sink;
mod startup;
mod usage;

pub use config::HostConfig;
pub use control::StartupControl;
pub use controller::{AttachmentGrant, HostController};
pub use error::HostControlError;
pub use sink::HostSink;
