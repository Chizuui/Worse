//! Resource-usage snapshot exchange between the shell and the Host.
//!
//! Direction matters: the shell (Electron Main in `ZCode`) sends
//! `resource-usage-snapshot-request` / `resource-usage-snapshot-cancel`, and
//! the Host replies with `resource-usage-snapshot-result`. The result is
//! therefore a correlated response to a shell-initiated request, never a Host
//! request the shell must answer. Shapes mirror
//! `hostResourceUsageSnapshotResultResponseSchema` in
//! `packages/shared/src/validation.ts`.

use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::ProtocolError;

/// `HostMessageTypes.ResourceUsageSnapshotRequest`.
pub const RESOURCE_USAGE_REQUEST_TYPE: &str = "resource-usage-snapshot-request";
/// `HostMessageTypes.ResourceUsageSnapshotCancel`.
pub const RESOURCE_USAGE_CANCEL_TYPE: &str = "resource-usage-snapshot-cancel";
/// `HostResponseTypes.ResourceUsageSnapshotResult`.
pub const RESOURCE_USAGE_RESULT_TYPE: &str = "resource-usage-snapshot-result";

/// How long one snapshot waits for the Host (`HOST_SNAPSHOT_TIMEOUT_MS` in
/// `packages/desktop/src/main/resourceManagerHostSampling.ts`).
pub const HOST_RESOURCE_USAGE_TIMEOUT: Duration = Duration::from_millis(900);

/// Maximum number of process rows the Host may report.
const MAX_PROCESSES: usize = 10_000;

/// Attribution bucket of one process row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ResourceUsageCategory {
    /// Host, agent runtime, and their MCP children.
    Base,
    /// A process owned by a built-in plugin.
    BuiltinPlugin,
    /// A process owned by a community plugin.
    CommunityPlugin,
}

/// One process row of a Host snapshot.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ResourceUsageProcess {
    /// Operating-system pid.
    pub pid: u32,
    /// Process display name.
    pub name: String,
    /// Attribution bucket.
    pub category: ResourceUsageCategory,
    /// Stable grouping key.
    pub group_key: String,
    /// Display label of the group.
    pub group_label: String,
    /// CPU usage since the previous sample, in percent.
    pub cpu_percent: f64,
    /// Resident memory, in bytes.
    pub memory_bytes: f64,
}

/// A `resource-usage-snapshot-result` payload without its envelope.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ResourceUsageSnapshot {
    /// Milliseconds since the Unix epoch; `0` when no sample was taken.
    pub sampled_at: u64,
    /// Process rows; empty when sampling failed or was superseded.
    pub processes: Vec<ResourceUsageProcess>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ResultWire {
    #[serde(rename = "type")]
    _kind: String,
    request_id: String,
    sampled_at: u64,
    processes: Vec<ResourceUsageProcess>,
}

/// Builds the shell -> Host request for one snapshot.
#[must_use]
pub fn resource_usage_request(request_id: &str) -> Value {
    json!({ "type": RESOURCE_USAGE_REQUEST_TYPE, "requestId": request_id })
}

/// Builds the shell -> Host cancellation of an in-flight snapshot.
#[must_use]
pub fn resource_usage_cancel(request_id: &str) -> Value {
    json!({ "type": RESOURCE_USAGE_CANCEL_TYPE, "requestId": request_id })
}

/// Parses a `resource-usage-snapshot-result` message.
///
/// Returns the correlation id and the snapshot.
///
/// # Errors
///
/// Returns [`ProtocolError::InvalidJson`] when the type, the correlation id,
/// or any row does not match the Host schema.
pub fn parse_resource_usage_result(
    message: &Value,
) -> Result<(String, ResourceUsageSnapshot), ProtocolError> {
    if message.get("type").and_then(Value::as_str) != Some(RESOURCE_USAGE_RESULT_TYPE) {
        return Err(ProtocolError::InvalidJson(
            "not a resource-usage-snapshot-result".to_owned(),
        ));
    }
    let wire = ResultWire::deserialize(message)
        .map_err(|error| ProtocolError::InvalidJson(error.to_string()))?;
    let invalid_row = wire.processes.iter().any(|row| {
        row.pid == 0
            || row.name.is_empty()
            || row.group_key.is_empty()
            || row.group_label.is_empty()
            || !(row.cpu_percent.is_finite() && row.cpu_percent >= 0.0)
            || !(row.memory_bytes.is_finite() && row.memory_bytes >= 0.0)
    });
    if wire.request_id.is_empty() || wire.processes.len() > MAX_PROCESSES || invalid_row {
        return Err(ProtocolError::InvalidJson(
            "resource-usage-snapshot-result violates the Host schema".to_owned(),
        ));
    }
    Ok((
        wire.request_id,
        ResourceUsageSnapshot {
            sampled_at: wire.sampled_at,
            processes: wire.processes,
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row() -> Value {
        json!({
            "pid": 42, "name": "node", "category": "base", "groupKey": "host",
            "groupLabel": "Host", "cpuPercent": 1.5, "memoryBytes": 1_048_576
        })
    }

    #[test]
    fn requests_use_the_host_message_types() {
        assert_eq!(
            resource_usage_request("r1"),
            json!({ "type": "resource-usage-snapshot-request", "requestId": "r1" })
        );
        assert_eq!(
            resource_usage_cancel("r1"),
            json!({ "type": "resource-usage-snapshot-cancel", "requestId": "r1" })
        );
    }

    #[test]
    fn parses_a_valid_result() {
        let message = json!({
            "type": "resource-usage-snapshot-result", "requestId": "r1",
            "sampledAt": 1_700_000_000_000_u64, "processes": [row()]
        });
        let (id, snapshot) = parse_resource_usage_result(&message).unwrap();
        assert_eq!(id, "r1");
        assert_eq!(snapshot.sampled_at, 1_700_000_000_000);
        assert_eq!(snapshot.processes.len(), 1);
        assert_eq!(snapshot.processes[0].category, ResourceUsageCategory::Base);
        assert!((snapshot.processes[0].memory_bytes - 1_048_576.0).abs() < f64::EPSILON);
    }

    #[test]
    fn accepts_the_empty_fallback_shape() {
        let message = json!({
            "type": "resource-usage-snapshot-result", "requestId": "r2",
            "sampledAt": 0, "processes": []
        });
        let (_, snapshot) = parse_resource_usage_result(&message).unwrap();
        assert_eq!(snapshot, ResourceUsageSnapshot::default());
    }

    #[test]
    fn rejects_schema_violations() {
        let mut negative = row();
        negative["cpuPercent"] = json!(-1);
        let mut extra = row();
        extra["extra"] = json!(true);
        for processes in [json!([negative]), json!([extra])] {
            let message = json!({
                "type": "resource-usage-snapshot-result", "requestId": "r3",
                "sampledAt": 1, "processes": processes
            });
            assert!(parse_resource_usage_result(&message).is_err());
        }
        let wrong_type =
            json!({ "type": "log", "requestId": "r", "sampledAt": 1, "processes": [] });
        assert!(parse_resource_usage_result(&wrong_type).is_err());
        let empty_id = json!({
            "type": "resource-usage-snapshot-result", "requestId": "",
            "sampledAt": 1, "processes": []
        });
        assert!(parse_resource_usage_result(&empty_id).is_err());
    }
}
