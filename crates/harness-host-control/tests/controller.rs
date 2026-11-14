//! End-to-end controller tests against a fake Node sidecar.
//!
//! The fixture speaks only the NDJSON bridge protocol. Tests are skipped
//! (with a message) when `node` is not on `PATH`.

#![cfg(not(any(target_os = "android", target_os = "ios")))]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use harness_host_control::{HostConfig, HostControlError, HostController, HostSink};
use harness_host_protocol::{HostEventKind, HostState};
use serde_json::{json, Value};

#[derive(Default)]
struct RecordingSink {
    states: Mutex<Vec<HostState>>,
    startup: Mutex<Vec<Value>>,
    events: Mutex<Vec<Value>>,
}

impl HostSink for RecordingSink {
    fn on_state(&self, state: &HostState) {
        self.states.lock().unwrap().push(state.clone());
    }
    fn on_startup_state(&self, state: &Value) {
        self.startup.lock().unwrap().push(state.clone());
    }
    fn on_host_event(&self, _kind: &HostEventKind, message: &Value) {
        self.events.lock().unwrap().push(message.clone());
    }
}

impl RecordingSink {
    fn event_with(&self, key: &str) -> Vec<Value> {
        let events = self.events.lock().unwrap();
        events
            .iter()
            .filter(|e| e.get(key).is_some())
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
        init_message: json!({ "type": "init-local", "databaseStartupId": "startup-test" }),
        env: vec![("FAKE_MODE".to_owned(), mode.to_owned())],
        shutdown_grace: Duration::from_secs(2),
        attachment_ttl: Duration::from_secs(30),
    }
}

async fn eventually(mut check: impl FnMut() -> bool) {
    for _ in 0..100 {
        if check() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("condition not met in time");
}

#[test]
fn controller_is_send_and_sync() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<HostController>();
}

#[tokio::test]
async fn ready_attach_fallback_and_shutdown() {
    if !node_available() {
        eprintln!("skipping: node is not on PATH");
        return;
    }
    let sink = Arc::new(RecordingSink::default());
    let controller = HostController::start(config("ok"), sink.clone())
        .await
        .unwrap();
    assert!(matches!(
        controller.state(),
        HostState::Ready { ws_port: 45123, .. }
    ));

    let first = controller.open_attachment().await.unwrap();
    assert_eq!(first.url, "ws://127.0.0.1:45123/v1/attach");
    assert_eq!(first.token.len(), 64);
    assert_eq!(first.database_startup_id, "startup-test");

    // Startup cache: foreign id rejected, newest sequence kept.
    eventually(|| sink.startup.lock().unwrap().len() == 2).await;
    let latest = controller.latest_startup_state().unwrap();
    assert_eq!(latest["phase"], "ready");
    assert_eq!(latest["sequence"], 2);

    // The browser request received the exact contract fallback.
    eventually(|| !sink.event_with("echo").is_empty()).await;
    let echoed = sink.event_with("echo")[0]["echo"].clone();
    assert_eq!(
        echoed,
        json!({
            "type": "browser-execute-result",
            "requestId": "browser-1",
            "result": {
                "ok": false,
                "error": {
                    "code": "backend_unavailable",
                    "message": "browser executor is not available in Harness Phase 2",
                },
                "elapsedMs": 0,
            },
        })
    );

    // After attach.active the next grant re-attaches instead of re-initializing.
    let second = controller.open_attachment().await.unwrap();
    assert_ne!(second.attachment_id, first.attachment_id);
    assert_ne!(second.token, first.token);
    assert_eq!(second.startup_state.unwrap()["phase"], "ready");
    eventually(|| sink.event_with("echoHostMessageType").len() == 2).await;
    let kinds: Vec<Value> = sink
        .event_with("echoHostMessageType")
        .iter()
        .map(|e| e["echoHostMessageType"].clone())
        .collect();
    assert_eq!(
        kinds,
        vec![json!("init-local"), json!("attach-service-port")]
    );

    let snapshot = controller
        .database_startup_control(json!({ "action": "snapshot" }))
        .await
        .unwrap();
    assert_eq!(snapshot.unwrap()["sequence"], 2);
    assert!(controller
        .database_startup_control(json!({ "action": "bogus" }))
        .await
        .is_err());

    controller.shutdown().await.unwrap();
    controller.shutdown().await.unwrap();
    assert_eq!(controller.state(), HostState::Stopped);
    assert_eq!(
        controller.open_attachment().await.unwrap_err(),
        HostControlError::NotRunning
    );
}

#[tokio::test]
async fn fatal_before_ready_is_a_spawn_failure() {
    if !node_available() {
        eprintln!("skipping: node is not on PATH");
        return;
    }
    let sink = Arc::new(RecordingSink::default());
    let error = HostController::start(config("fatal"), sink.clone())
        .await
        .err()
        .unwrap();
    assert!(matches!(error, HostControlError::Spawn(ref m) if m.contains("host_import_failed")));
    let last = sink.states.lock().unwrap().last().cloned().unwrap();
    assert!(matches!(last, HostState::Failed { .. }));
}

#[tokio::test]
async fn rejected_attachment_and_unexpected_exit() {
    if !node_available() {
        eprintln!("skipping: node is not on PATH");
        return;
    }
    let sink = Arc::new(RecordingSink::default());
    let controller = HostController::start(config("reject"), sink.clone())
        .await
        .unwrap();
    let error = controller.open_attachment().await.unwrap_err();
    assert!(matches!(error, HostControlError::Protocol(ref m) if m.contains("attach_rejected")));
    controller.shutdown().await.unwrap();

    let sink = Arc::new(RecordingSink::default());
    let controller = HostController::start(config("exit-after-ready"), sink.clone())
        .await
        .unwrap();
    eventually(|| matches!(controller.state(), HostState::Failed { .. })).await;
    // The cache now reports transport_closed so a reload cannot reuse a stale ready.
    let failed = controller.latest_startup_state().unwrap();
    assert_eq!(failed["errorCode"], "transport_closed");
    assert_eq!(
        controller.open_attachment().await.unwrap_err(),
        HostControlError::NotRunning
    );
    controller.shutdown().await.unwrap();
}

#[tokio::test]
async fn missing_launcher_fails_without_spawning() {
    let mut config = config("ok");
    config.launcher = PathBuf::from("does/not/exist.mjs");
    let error = HostController::start(config, Arc::new(RecordingSink::default()))
        .await
        .err()
        .unwrap();
    assert!(matches!(error, HostControlError::Spawn(_)));
}
