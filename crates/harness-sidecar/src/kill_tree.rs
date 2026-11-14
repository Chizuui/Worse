//! Forcible termination of a sidecar process and its descendants.

use std::process::Stdio;
use std::time::Duration;

use tokio::process::Child;
use tokio::time::timeout;

use crate::error::SidecarError;

/// How long a killed process may take to be reaped before reporting an error.
const REAP_TIMEOUT: Duration = Duration::from_secs(5);

/// Kills the process tree rooted at `child` and reaps the direct child.
///
/// Tradeoff: Windows shells out to `taskkill /T /F /PID`, which walks the
/// parent/child links of the live process table. Unix sends `SIGKILL` to the
/// process group that the supervisor created with `process_group(0)`, so it
/// relies on the child not having detached into a different group or session.
/// Descendants that deliberately daemonize escape both strategies, which is
/// acceptable because every sidecar is expected to stay in its group.
pub(crate) async fn kill_tree(child: &mut Child) -> Result<(), SidecarError> {
    kill_process_tree(child.id()).await;
    reap(child).await
}

/// `CREATE_NO_WINDOW`: start a console program without allocating a console window.
#[cfg(windows)]
pub(crate) const CREATE_NO_WINDOW: u32 = 0x0800_0000;

#[cfg(windows)]
async fn kill_process_tree(pid: Option<u32>) {
    let Some(pid) = pid else {
        return;
    };
    let result = tokio::process::Command::new("taskkill")
        .args(["/T", "/F", "/PID", &pid.to_string()])
        // 同上：GUI 进程调用 taskkill 时避免闪出控制台窗口。
        .creation_flags(CREATE_NO_WINDOW)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await;
    match result {
        Ok(status) if status.success() => {}
        Ok(status) => tracing::warn!(%pid, code = status.code(), "taskkill did not report success"),
        Err(error) => tracing::warn!(%pid, %error, "taskkill could not be executed"),
    }
}

#[cfg(unix)]
async fn kill_process_tree(pid: Option<u32>) {
    let Some(pid) = pid else {
        return;
    };
    let group = format!("-{pid}");
    let result = tokio::process::Command::new("kill")
        .args(["-s", "KILL", "--", &group])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await;
    match result {
        Ok(status) if status.success() => {}
        Ok(status) => tracing::warn!(%pid, code = status.code(), "kill did not report success"),
        Err(error) => tracing::warn!(%pid, %error, "kill could not be executed"),
    }
}

async fn reap(child: &mut Child) -> Result<(), SidecarError> {
    match timeout(REAP_TIMEOUT, child.wait()).await {
        Ok(Ok(_status)) => return Ok(()),
        Ok(Err(error)) => return Err(SidecarError::Io(error.to_string())),
        Err(_elapsed) => {
            tracing::warn!(
                "process tree did not exit after the kill request; signalling the child"
            );
        }
    }
    child
        .start_kill()
        .map_err(|error| SidecarError::Io(error.to_string()))?;
    match timeout(REAP_TIMEOUT, child.wait()).await {
        Ok(Ok(_status)) => Ok(()),
        Ok(Err(error)) => Err(SidecarError::Io(error.to_string())),
        Err(_elapsed) => Err(SidecarError::Io(
            "sidecar did not exit after kill".to_owned(),
        )),
    }
}
