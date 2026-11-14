//! Pure, I/O-free protocol types for the Phase 2 Host bridge.
//!
//! The crate defines the NDJSON contract between the Rust shell and the Node
//! sidecar, the `HostResponseTypes` catalogue, the Phase 2 handling class of
//! every Host message, and the Host process state. It performs no I/O and has
//! no internal dependencies, so it compiles for every target, including mobile.

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

mod error;
mod events;
mod handling;
mod resource_usage;
mod sidecar;
mod state;

use std::time::Duration;

pub use error::ProtocolError;
pub use events::{HostEventKind, HOST_EVENT_KINDS};
pub use handling::{classify, AnswerKind, HostHandling, ResponseKind, WEB_REMOTE_MESSAGE_PREFIX};
pub use resource_usage::{
    parse_resource_usage_result, resource_usage_cancel, resource_usage_request,
    ResourceUsageCategory, ResourceUsageProcess, ResourceUsageSnapshot,
    HOST_RESOURCE_USAGE_TIMEOUT, RESOURCE_USAGE_CANCEL_TYPE, RESOURCE_USAGE_REQUEST_TYPE,
    RESOURCE_USAGE_RESULT_TYPE,
};
pub use sidecar::{
    AttachActiveLine, AttachClosedLine, AttachFailedLine, AttachMessageKind, AttachOpenLine,
    AttachOpenedLine, CloseReason, FatalLine, HostMessageLine, LogLevel, LogLine, PingLine,
    PongLine, ReadyLine, ShellLine, SidecarErrorCode, SidecarLine, SIDECAR_ERROR_CODES,
};
pub use state::HostState;

/// Version carried by every NDJSON line in both directions.
pub const HOST_BRIDGE_PROTOCOL_VERSION: u16 = 1;

/// Loopback WebSocket path used for renderer attachments.
pub const HOST_WS_PATH: &str = "/v1/attach";

/// Maximum time the sidecar waits for the WebSocket hello frame.
pub const HOST_HELLO_TIMEOUT: Duration = Duration::from_secs(5);

/// Maximum time the shell waits for `attach.opened` after `attach.open`.
pub const HOST_ATTACH_TIMEOUT: Duration = Duration::from_secs(10);

/// Default lifetime of a pending attachment token.
pub const DEFAULT_ATTACHMENT_TTL: Duration = Duration::from_secs(30);

/// Maximum size of a single NDJSON line, in bytes.
pub const MAX_LINE_BYTES: usize = 16 * 1024 * 1024;
