//! Stable command names registered by the Harness shell.
//!
//! Frontends must use these constants instead of hard-coded strings so that
//! the Rust and TypeScript sides cannot drift.

/// Returns [`crate::AppInfo`] for the running shell.
pub const GET_APP_INFO: &str = "get_app_info";
/// Returns [`crate::Capabilities`] for the current surface.
pub const GET_CAPABILITIES: &str = "get_capabilities";
/// Opens an external URL after validation.
pub const OPEN_EXTERNAL: &str = "open_external";
/// Reveals a path in the platform file manager.
pub const REVEAL_PATH: &str = "reveal_path";
/// Shows the native directory picker.
pub const SELECT_DIRECTORY: &str = "select_directory";
/// Shows the native file picker.
pub const SELECT_FILE: &str = "select_file";
/// Spawns the local Host on first use and returns one attachment grant.
pub const HOST_OPEN_ATTACHMENT: &str = "host_open_attachment";
/// Forwards a database startup control message and replays the cached snapshot.
pub const HOST_DATABASE_STARTUP_CONTROL: &str = "host_database_startup_control";
/// Reports the local Host lifecycle state without spawning it.
pub const HOST_STATUS: &str = "host_status";
