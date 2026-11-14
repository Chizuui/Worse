//! Wire protocol version shared by every Harness frontend and the shell.

/// Current Harness IPC protocol version.
///
/// Bump this whenever the shape or meaning of a command payload changes.
pub const PROTOCOL_VERSION: u32 = 1;
