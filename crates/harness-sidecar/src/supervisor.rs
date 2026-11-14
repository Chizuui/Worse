//! Supervisor abstraction shared by the shell and the desktop runtime.

use core::future::Future;
use core::pin::Pin;

use crate::error::SidecarError;
use crate::handle::SidecarHandle;
use crate::spec::SidecarSpec;

/// Boxed future returned by [`Supervisor`] methods.
///
/// Boxed futures keep the trait object safe, so callers may hold a
/// `Box<dyn Supervisor>` without an extra helper crate.
pub type SupervisorFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Starts and stops sidecar processes.
pub trait Supervisor: Send + Sync {
    /// Starts one sidecar process.
    ///
    /// # Errors
    ///
    /// Returns [`SidecarError::Spawn`] when the process cannot be started and
    /// [`SidecarError::AlreadyRunning`] when `spec.id` is still supervised.
    fn spawn(&self, spec: SidecarSpec)
        -> SupervisorFuture<'_, Result<SidecarHandle, SidecarError>>;

    /// Stops every supervised sidecar process.
    ///
    /// The call is idempotent: once all children are stopped, further calls
    /// resolve immediately.
    ///
    /// # Errors
    ///
    /// Returns the first [`SidecarError`] observed while stopping children.
    fn shutdown_all(&self) -> SupervisorFuture<'_, Result<(), SidecarError>>;
}
