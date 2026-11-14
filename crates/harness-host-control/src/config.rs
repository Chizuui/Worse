//! Configuration for one Host sidecar.

use std::path::PathBuf;
use std::time::Duration;

use serde_json::Value;

/// Everything the controller needs to spawn and attach to the Host.
#[derive(Debug, Clone)]
pub struct HostConfig {
    /// Node executable, normally `node`.
    pub node_program: PathBuf,
    /// Absolute path to the sidecar launcher (`dist/main.js`).
    pub launcher: PathBuf,
    /// Absolute path to the Host bundle (`packages/desktop/out/host/index.js`).
    pub host_entry: PathBuf,
    /// Working directory of the sidecar; the repository root in development.
    pub cwd: PathBuf,
    /// Complete `init-local` message without a port, `hostId`, or `deliveryKind`.
    pub init_message: Value,
    /// Extra environment variables passed to the sidecar.
    pub env: Vec<(String, String)>,
    /// How long a graceful shutdown may take before the tree is killed.
    pub shutdown_grace: Duration,
    /// Validity window of a freshly issued attachment token.
    pub attachment_ttl: Duration,
}
