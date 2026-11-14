//! Core domain types shared by every Harness crate.
//!
//! This crate has no internal dependencies and no I/O: it defines the
//! canonical workspace identity, the identifiers used across process
//! boundaries, the deployment surface, and the shared error type.

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

mod error;
mod ids;
mod surface;
mod workspace_key;

pub use error::CoreError;
pub use ids::{SidecarId, WindowId};
pub use surface::Surface;
pub use workspace_key::WorkspaceKey;
