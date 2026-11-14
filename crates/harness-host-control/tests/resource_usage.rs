//! Resource-usage snapshots against the fake Node sidecar.
//!
//! Tests are skipped (with a message) when `node` is not on `PATH`.

#![cfg(not(any(target_os = "android", target_os = "ios")))]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use harness_host_control::{HostConfig, HostControlError, HostController, HostSink};
use harness_host_protocol::{HostEventKind, HostState};
use serde_json::{json, Value};

#[derive(Default)]
struct EchoSink {
    echoes: Mutex<Vec<Value>>,
}

impl HostSink for EchoSink {
    fn on_state(&self, _state: &HostState) {}
    fn on_startup_state(&self, _state: &Value) {}
    fn on_host_event(&self, _kind: &HostEventKind, message: &Value) {
        if let Some(echo) = message.get("echo") {
            self.echoes.lock().unwrap().push(echo.clone());
        }
    }
}

impl EchoSink {
    fn echoed(&self, kind: &str) -> Vec<Value> {
        let echoes = self.echoes.lock().unwrap();
        echoes
            .iter()
            .filter(|echo| echo["type"] == kind)
            .cloned()
            .collect()
    }
}

fn node_available() -> bool {
    std::process::Command::new("node")
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success())
}

fn config(mode: &str) -> HostConfig {
    let fixtures = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let launcher = fixtures.join("fake-sidecar.mjs");
    HostConfig {
        node_program: PathBuf::from("node"),
        host_entry: launcher.clone(),
        launcher,
        cwd: fixtures,
        init_message: json!({ "type": "init-local", "databaseStartupId": "startup-usage" }),
        env: vec![("FAKE_MODE".to_owned(), mode.to_owned())],
        shutdown_grace: Duration::from_secs(2),
        attachment_ttl: Duration::from_secs(30),
    }
}

#[tokio::test]
async fn snapshot_resolves_the_matching_result_and_ignores_stale_ids() {
    if !node_available() {
        eprintln!("skipping: node is not on PATH");
        return;
    }
    let controller = HostController::start(config("ok"), Arc::new(EchoSink::default()))
        .await
        .unwrap();
    let snapshot = controller
        .resource_usage_snapshot(Duration::from_secs(5))
        .await
        .unwrap();
    assert_eq!(snapshot.sampled_at, 1_700_000_000_000);
    assert_eq!(snapshot.processes.len(), 1);
    assert_eq!(snapshot.processes[0].group_key, "host");
    // The slot was released, so a second snapshot issues a new request.
    let again = controller
        .resource_usage_snapshot(Duration::from_secs(5))
        .await
        .unwrap();
    assert_eq!(again.processes.len(), 1);
    controller.shutdown().await.unwrap();
    assert_eq!(
        controller
            .resource_usage_snapshot(Duration::from_secs(1))
            .await
            .unwrap_err(),
        HostControlError::NotRunning
    );
}

#[tokio::test]
async fn timeout_keeps_one_request_in_flight_and_cancel_reaches_the_host() {
    if !node_available() {
        eprintln!("skipping: node is not on PATH");
        return;
    }
    let sink = Arc::new(EchoSink::default());
    let controller = HostController::start(config("usage-silent"), sink.clone())
        .await
        .unwrap();
    for _ in 0..2 {
        let snapshot = controller
            .resource_usage_snapshot(Duration::from_millis(200))
            .await
            .unwrap();
        assert_eq!(snapshot.processes.len(), 0);
    }

    controller.cancel_resource_usage().await.unwrap();
    for _ in 0..100 {
        if !sink.echoed("resource-usage-snapshot-cancel").is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let requests = sink.echoed("resource-usage-snapshot-request");
    let cancels = sink.echoed("resource-usage-snapshot-cancel");
    assert_eq!(
        requests.len(),
        1,
        "a pending request must not be duplicated"
    );
    assert_eq!(cancels.len(), 1);
    assert_eq!(cancels[0]["requestId"], requests[0]["requestId"]);
    controller.shutdown().await.unwrap();
}
