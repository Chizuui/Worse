//! Tokio-backed sidecar supervisor.

use std::collections::HashMap;
use std::process::Stdio;
use std::time::Duration;

use harness_core::SidecarId;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, ChildStderr, Command};
use tokio::sync::Mutex;
use tokio::time::{timeout_at, Instant};

use crate::error::SidecarError;
use crate::handle::{SidecarHandle, SidecarStdin};
use crate::kill_tree;
use crate::spec::SidecarSpec;
use crate::supervisor::{Supervisor, SupervisorFuture};

/// One child process owned by the supervisor.
struct ManagedChild {
    id: SidecarId,
    child: Child,
    stdin: SidecarStdin,
    grace: Duration,
}

/// Supervises sidecar processes with Tokio.
///
/// Children are spawned into their own process group on Unix so the shutdown
/// barrier can kill the whole tree. On Windows the tree is killed through
/// `taskkill /T /F`. `kill_on_drop` is enabled as a last-resort safety net when
/// the supervisor is dropped without a shutdown.
pub struct TokioSupervisor {
    children: Mutex<HashMap<SidecarId, ManagedChild>>,
}

impl TokioSupervisor {
    /// Creates a supervisor that owns no children.
    #[must_use]
    pub fn new() -> Self {
        Self {
            children: Mutex::new(HashMap::new()),
        }
    }

    /// Stops one supervised sidecar process.
    ///
    /// # Errors
    ///
    /// Returns [`SidecarError::NotFound`] when `id` is not supervised and
    /// forwards any failure observed while stopping the process.
    pub async fn shutdown(&self, id: &SidecarId) -> Result<(), SidecarError> {
        let managed = self
            .children
            .lock()
            .await
            .remove(id)
            .ok_or_else(|| SidecarError::NotFound(id.clone()))?;
        Self::stop(vec![managed]).await
    }

    async fn spawn_sidecar(&self, spec: SidecarSpec) -> Result<SidecarHandle, SidecarError> {
        if spec.program.as_os_str().is_empty() {
            return Err(SidecarError::Spawn("program path is empty".to_owned()));
        }
        let mut children = self.children.lock().await;
        Self::reap_stale(&mut children, &spec.id)?;
        let mut command = Command::new(&spec.program);
        command
            .args(&spec.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        if let Some(cwd) = spec.cwd.as_deref() {
            command.current_dir(cwd);
        }
        command.envs(spec.env.iter().map(|(key, value)| (key, value)));
        #[cfg(unix)]
        command.process_group(0);
        // 修复原因：release 版 shell 是 windows_subsystem="windows" 的 GUI 进程，没有控制台；
        // 直接启动 node 这类控制台程序时 Windows 会为其新建一个可见的控制台窗口。
        // stdio 已全部重定向为管道，CREATE_NO_WINDOW 只去掉这个多余窗口，不影响通信。
        #[cfg(windows)]
        command.creation_flags(crate::kill_tree::CREATE_NO_WINDOW);
        let mut child = command
            .spawn()
            .map_err(|error| SidecarError::Spawn(format!("{}: {error}", spec.id)))?;
        let pid = child.id();
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| SidecarError::Spawn(format!("{}: stdin pipe unavailable", spec.id)))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| SidecarError::Spawn(format!("{}: stdout pipe unavailable", spec.id)))?;
        if let Some(stderr) = child.stderr.take() {
            spawn_stderr_drain(spec.id.clone(), stderr);
        }
        let stdin = SidecarStdin::new(stdin);
        children.insert(
            spec.id.clone(),
            ManagedChild {
                id: spec.id.clone(),
                child,
                stdin: stdin.clone(),
                grace: spec.shutdown_grace,
            },
        );
        Ok(SidecarHandle::new(
            spec.id,
            pid,
            stdin,
            BufReader::new(stdout).lines(),
        ))
    }

    fn reap_stale(
        children: &mut HashMap<SidecarId, ManagedChild>,
        id: &SidecarId,
    ) -> Result<(), SidecarError> {
        let Some(existing) = children.get_mut(id) else {
            return Ok(());
        };
        match existing.child.try_wait() {
            Ok(Some(_status)) => {
                children.remove(id);
                Ok(())
            }
            Ok(None) => Err(SidecarError::AlreadyRunning(id.clone())),
            Err(error) => Err(SidecarError::Io(error.to_string())),
        }
    }

    async fn stop(mut managed: Vec<ManagedChild>) -> Result<(), SidecarError> {
        if managed.is_empty() {
            return Ok(());
        }
        let longest = managed
            .iter()
            .map(|child| child.grace)
            .max()
            .unwrap_or(Duration::ZERO);
        let deadline = Instant::now() + longest;
        let mut first_error = None;
        for mut entry in managed.drain(..) {
            entry.stdin.close().await;
            let waited = timeout_at(deadline, entry.child.wait()).await;
            let outcome = match waited {
                Ok(Ok(_status)) => Ok(()),
                Ok(Err(error)) => Err(SidecarError::Io(error.to_string())),
                Err(_elapsed) => {
                    tracing::warn!(
                        sidecar = %entry.id,
                        "graceful shutdown timed out; killing the process tree"
                    );
                    kill_tree::kill_tree(&mut entry.child).await
                }
            };
            if let Err(error) = outcome {
                tracing::warn!(sidecar = %entry.id, %error, "sidecar did not shut down cleanly");
                first_error.get_or_insert(error);
            }
        }
        first_error.map_or(Ok(()), Err)
    }
}

impl Default for TokioSupervisor {
    fn default() -> Self {
        Self::new()
    }
}

impl Supervisor for TokioSupervisor {
    fn spawn(
        &self,
        spec: SidecarSpec,
    ) -> SupervisorFuture<'_, Result<SidecarHandle, SidecarError>> {
        Box::pin(self.spawn_sidecar(spec))
    }

    fn shutdown_all(&self) -> SupervisorFuture<'_, Result<(), SidecarError>> {
        Box::pin(async move {
            let managed = {
                let mut guard = self.children.lock().await;
                guard.drain().map(|(_, child)| child).collect::<Vec<_>>()
            };
            Self::stop(managed).await
        })
    }
}

fn spawn_stderr_drain(id: SidecarId, stderr: ChildStderr) {
    tokio::spawn(async move {
        let mut lines = BufReader::new(stderr).lines();
        loop {
            match lines.next_line().await {
                Ok(Some(line)) => tracing::debug!(sidecar = %id, "{line}"),
                Ok(None) => break,
                Err(error) => {
                    tracing::warn!(sidecar = %id, %error, "failed to read sidecar stderr");
                    break;
                }
            }
        }
    });
}
