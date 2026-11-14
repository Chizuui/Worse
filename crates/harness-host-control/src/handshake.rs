//! Spawning the sidecar and waiting for its `ready` line.

use std::path::Path;
use std::sync::Arc;

use harness_core::SidecarId;
use harness_host_protocol::{
    ReadyLine, SidecarLine, HOST_ATTACH_TIMEOUT, HOST_BRIDGE_PROTOCOL_VERSION,
};
use harness_sidecar::{SidecarIo, SidecarSpec, Supervisor, TokioSupervisor};
use tokio::time::{timeout_at, Instant};

use crate::config::HostConfig;
use crate::dispatcher;
use crate::error::HostControlError;
use crate::shared::HostShared;

/// Builds the argv-only spawn specification (never a shell string).
pub(crate) fn sidecar_spec(id: &SidecarId, config: &HostConfig) -> SidecarSpec {
    let mut spec = SidecarSpec::new(id.clone(), config.node_program.clone())
        .with_args([
            "--no-warnings".to_owned(),
            config.launcher.to_string_lossy().into_owned(),
        ])
        .with_cwd(config.cwd.clone())
        .with_env(
            "HARNESS_HOST_ENTRY",
            config.host_entry.to_string_lossy().into_owned(),
        )
        .with_shutdown_grace(config.shutdown_grace);
    for (key, value) in &config.env {
        spec = spec.with_env(key.clone(), value.clone());
    }
    spec
}

/// Fails early with a readable error when a launch file is missing.
pub(crate) async fn ensure_file(path: &Path, what: &str) -> Result<(), HostControlError> {
    match tokio::fs::metadata(path).await {
        Ok(metadata) if metadata.is_file() => Ok(()),
        Ok(_) => Err(HostControlError::Spawn(format!(
            "{what} is not a file: {}",
            path.display()
        ))),
        Err(error) => Err(HostControlError::Spawn(format!(
            "{what} is missing at {}: {error}",
            path.display()
        ))),
    }
}

/// Spawns the sidecar and returns its I/O pair and pid.
pub(crate) async fn spawn(
    supervisor: &TokioSupervisor,
    spec: SidecarSpec,
) -> Result<(SidecarIo, Option<u32>), HostControlError> {
    let mut handle = supervisor
        .spawn(spec)
        .await
        .map_err(|error| HostControlError::Spawn(error.to_string()))?;
    let pid = handle.pid();
    let io = handle
        .take_io()
        .ok_or_else(|| HostControlError::Spawn("sidecar I/O pair unavailable".to_owned()))?;
    Ok((io, pid))
}

/// Reads lines until `ready`, routing every earlier line normally.
///
/// Lines before `ready` (logs, early Host messages) go through the regular
/// dispatcher so nothing is lost while the Host imports.
pub(crate) async fn wait_ready(
    shared: &Arc<HostShared>,
    io: &mut SidecarIo,
) -> Result<ReadyLine, HostControlError> {
    let deadline = Instant::now() + HOST_ATTACH_TIMEOUT;
    loop {
        let next = timeout_at(deadline, io.recv_line())
            .await
            .map_err(|_elapsed| {
                HostControlError::Timeout("sidecar did not report ready".to_owned())
            })?
            .map_err(|error| HostControlError::Io(error.to_string()))?;
        let Some(raw) = next else {
            return Err(HostControlError::Spawn(
                "sidecar exited before reporting ready".to_owned(),
            ));
        };
        match SidecarLine::parse(&raw) {
            Ok(SidecarLine::Ready(ready)) => return validate(ready),
            Ok(SidecarLine::Fatal(fatal)) => {
                return Err(HostControlError::Spawn(format!(
                    "sidecar failed to start ({}): {}",
                    fatal.code.as_str(),
                    fatal.message
                )));
            }
            Ok(_) | Err(_) => dispatcher::dispatch_line(shared, &raw),
        }
    }
}

fn validate(ready: ReadyLine) -> Result<ReadyLine, HostControlError> {
    if ready.protocol != HOST_BRIDGE_PROTOCOL_VERSION {
        return Err(HostControlError::Protocol(format!(
            "sidecar speaks bridge protocol {}, expected {HOST_BRIDGE_PROTOCOL_VERSION}",
            ready.protocol
        )));
    }
    if ready.ws_port == 0 {
        return Err(HostControlError::Protocol(
            "sidecar reported WebSocket port 0".to_owned(),
        ));
    }
    Ok(ready)
}
