use std::time::{Duration, Instant};

use harness_core::SidecarId;

use super::TokioSupervisor;
use crate::error::SidecarError;
use crate::spec::SidecarSpec;
use crate::supervisor::Supervisor;

/// A child that stays alive until it is stopped.
fn sleeper(id: &str, grace: Duration) -> SidecarSpec {
    #[cfg(windows)]
    let spec = SidecarSpec::new(id, "ping").with_args(["-n", "60", "127.0.0.1"]);
    #[cfg(unix)]
    let spec = SidecarSpec::new(id, "sleep").with_args(["60"]);
    spec.with_shutdown_grace(grace)
}

/// A child that prints one line and exits.
fn printer(id: &str) -> SidecarSpec {
    #[cfg(windows)]
    let spec = SidecarSpec::new(id, "cmd").with_args(["/C", "echo hello"]);
    #[cfg(unix)]
    let spec = SidecarSpec::new(id, "sh").with_args(["-c", "echo hello"]);
    spec
}

/// A child that exits as soon as its stdin reaches end-of-input.
fn reader(id: &str) -> SidecarSpec {
    #[cfg(windows)]
    let spec = SidecarSpec::new(id, "cmd").with_args(["/C", "more"]);
    #[cfg(unix)]
    let spec = SidecarSpec::new(id, "sh").with_args(["-c", "cat > /dev/null"]);
    spec
}

#[tokio::test]
async fn reads_stdout_lines_and_shuts_down_idempotently() {
    let supervisor = TokioSupervisor::new();
    let mut handle = supervisor.spawn(printer("printer")).await.unwrap();
    assert_eq!(handle.id().as_str(), "printer");
    assert!(handle.pid().is_some());
    assert!(handle.has_io());
    let mut io = handle.take_io().unwrap();
    assert!(!handle.has_io());
    assert_eq!(io.recv_line().await.unwrap().as_deref(), Some("hello"));
    drop(io);
    supervisor.shutdown_all().await.unwrap();
    supervisor.shutdown_all().await.unwrap();
}

#[tokio::test]
async fn force_kills_a_stubborn_child_after_the_grace_period() {
    let supervisor = TokioSupervisor::new();
    let handle = supervisor
        .spawn(sleeper("stubborn", Duration::from_millis(100)))
        .await
        .unwrap();
    assert!(handle.pid().is_some());
    let started = Instant::now();
    tokio::time::timeout(Duration::from_secs(30), supervisor.shutdown_all())
        .await
        .unwrap()
        .unwrap();
    assert!(started.elapsed() < Duration::from_secs(30));
}

#[tokio::test]
async fn rejects_a_duplicate_id_until_the_child_exits() {
    let supervisor = TokioSupervisor::new();
    let _handle = supervisor
        .spawn(sleeper("duplicate", Duration::from_secs(1)))
        .await
        .unwrap();
    let error = supervisor
        .spawn(sleeper("duplicate", Duration::from_secs(1)))
        .await
        .unwrap_err();
    assert!(matches!(error, SidecarError::AlreadyRunning(id) if id.as_str() == "duplicate"));
    supervisor.shutdown_all().await.unwrap();
}

#[tokio::test]
async fn respawns_after_the_previous_child_exited() {
    let supervisor = TokioSupervisor::new();
    let mut first = supervisor.spawn(printer("echo")).await.unwrap();
    let mut io = first.take_io().unwrap();
    assert_eq!(io.recv_line().await.unwrap().as_deref(), Some("hello"));
    drop(io);
    drop(first);
    let mut respawned = false;
    for _ in 0..20 {
        match supervisor.spawn(printer("echo")).await {
            Ok(_handle) => {
                respawned = true;
                break;
            }
            Err(SidecarError::AlreadyRunning(_)) => {
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            Err(other) => panic!("unexpected spawn error: {other}"),
        }
    }
    assert!(respawned, "the finished child was never reaped");
    supervisor.shutdown_all().await.unwrap();
}

#[tokio::test]
async fn shutdown_of_an_unknown_id_reports_not_found() {
    let supervisor = TokioSupervisor::new();
    let error = supervisor
        .shutdown(&SidecarId::from("missing"))
        .await
        .unwrap_err();
    assert!(matches!(error, SidecarError::NotFound(id) if id.as_str() == "missing"));
}

#[tokio::test]
async fn closes_stdin_so_a_reader_exits_within_the_grace_period() {
    let supervisor = TokioSupervisor::new();
    let _handle = supervisor
        .spawn(reader("reader").with_shutdown_grace(Duration::from_secs(10)))
        .await
        .unwrap();
    let started = Instant::now();
    supervisor.shutdown_all().await.unwrap();
    assert!(
        started.elapsed() < Duration::from_secs(8),
        "the child ignored end-of-input and had to be killed"
    );
}

#[tokio::test]
async fn stdin_rejects_writes_after_close() {
    let supervisor = TokioSupervisor::new();
    let mut handle = supervisor
        .spawn(sleeper("stdin", Duration::from_millis(100)))
        .await
        .unwrap();
    let io = handle.take_io().unwrap();
    assert!(io.stdin.is_open().await);
    assert!(io.stdin.close().await);
    assert!(!io.stdin.close().await);
    assert!(io.stdin.write_line("{}").await.is_err());
    supervisor.shutdown_all().await.unwrap();
}
