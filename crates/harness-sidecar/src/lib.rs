//! Supervision of child sidecar processes for the Harness desktop shell.

//!

//! The crate owns process lifecycle only: it starts a child, exposes its

//! NDJSON stdin/stdout pair, and guarantees that a shutdown closes stdin,

//! waits for the grace period, and then kills the remaining process tree.

//! Mobile surfaces never run a local host, so the whole crate compiles away

//! outside desktop targets.

#![cfg(not(any(target_os = "android", target_os = "ios")))]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

mod error;
mod handle;
mod kill_tree;
mod spec;
mod supervisor;
mod tokio_supervisor;

#[cfg(test)]
mod tests;

pub use error::SidecarError;
pub use handle::{SidecarHandle, SidecarIo, SidecarStdin, SidecarStdout};
pub use spec::{SidecarSpec, DEFAULT_SHUTDOWN_GRACE};
pub use supervisor::{Supervisor, SupervisorFuture};
pub use tokio_supervisor::TokioSupervisor;
