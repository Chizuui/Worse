//! Window-scoped local Host runtime (desktop only).
//!
//! Owns at most one [`HostController`]. The controller owns the startup cache;
//! this module only serialises spawns and maps errors for the commands.

mod launch;
mod layout;
mod node;
mod paths;
mod sink;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use harness_host_control::{HostController, StartupControl};
use harness_protocol::{CommandError, ErrorCode, HostAttachmentGrant, HostStateDto, HostStatus};
use serde_json::Value;
use tauri::AppHandle;
use tokio::sync::{Mutex as AsyncMutex, OnceCell};

use self::launch::HostLaunch;
use self::sink::{control_error, emit_status, ShellHostSink};

/// Lifecycle slot of the window's Host.
enum HostSlot {
    Stopped,
    Starting,
    Running(Arc<HostController>),
    Failed { code: ErrorCode, message: String },
}

/// Owns the Host process lifecycle for one window.
pub(crate) struct HostRuntime {
    app: AppHandle,
    launch: HostLaunch,
    /// Never held across `.await`; see [`HostRuntime::start_lock`].
    slot: Mutex<HostSlot>,
    /// Serialises spawn and shutdown so only one Host process exists.
    start_lock: AsyncMutex<()>,
    device_mid: OnceCell<String>,
    /// Set once the app is exiting or installing an update; no new Host may spawn.
    sealed: AtomicBool,
}

impl HostRuntime {
    pub(crate) fn new(app: &AppHandle) -> Self {
        Self {
            app: app.clone(),
            launch: HostLaunch::resolve(app),
            slot: Mutex::new(HostSlot::Stopped),
            start_lock: AsyncMutex::new(()),
            device_mid: OnceCell::new(),
            sealed: AtomicBool::new(false),
        }
    }

    pub(crate) fn status(&self) -> HostStatus {
        match &*self.slot() {
            HostSlot::Stopped => sink::status(HostStateDto::Stopped),
            HostSlot::Starting => sink::status(HostStateDto::Starting),
            HostSlot::Running(controller) => sink::status_from_state(&controller.state()),
            HostSlot::Failed { code, message } => HostStatus {
                state: HostStateDto::Failed,
                error_code: Some(crate::error::wire_name(*code)),
                error_message: Some(message.clone()),
            },
        }
    }

    pub(crate) async fn open_attachment(&self) -> Result<HostAttachmentGrant, CommandError> {
        let controller = self.ensure_controller().await?;
        let grant = controller
            .open_attachment()
            .await
            .map_err(|error| control_error(&error))?;
        Ok(HostAttachmentGrant {
            attachment_id: grant.attachment_id,
            url: grant.url,
            token: grant.token,
            database_startup_id: grant.database_startup_id,
            startup_state: grant.startup_state,
        })
    }

    pub(crate) async fn control(&self, control: Value) -> Result<(), CommandError> {
        let action = StartupControl::parse(&control)
            .ok_or_else(|| crate::error::invalid_input("invalid database startup control"))?;
        let controller = self.current_controller();
        match (action, controller) {
            (StartupControl::Snapshot, Some(controller)) => {
                let cached = controller
                    .database_startup_control(control)
                    .await
                    .map_err(|error| control_error(&error))?;
                if let Some(state) = cached {
                    sink::emit_database_startup(&self.app, &state);
                }
                Ok(())
            }
            (StartupControl::Snapshot, None) => Ok(()),
            (StartupControl::Retry, Some(controller)) => controller
                .database_startup_control(control)
                .await
                .map(|_cached| ())
                .map_err(|error| control_error(&error)),
            (StartupControl::Retry, None) => Err(crate::error::host(
                ErrorCode::HostNotStarted,
                "the local Host is not running",
            )),
            (StartupControl::Exit, controller) => {
                if let Some(controller) = controller {
                    if let Err(error) = controller.database_startup_control(control).await {
                        tracing::debug!(%error, "exit control was not delivered to the Host");
                    }
                }
                self.shutdown_for_exit().await;
                self.app.exit(0);
                Ok(())
            }
        }
    }

    /// Disposes the Host and waits for the sidecar barrier. Idempotent.
    pub(crate) async fn shutdown(&self) {
        let _start = self.start_lock.lock().await;
        let controller = match std::mem::replace(&mut *self.slot(), HostSlot::Stopped) {
            HostSlot::Running(controller) => Some(controller),
            HostSlot::Stopped | HostSlot::Starting | HostSlot::Failed { .. } => None,
        };
        if let Some(controller) = controller {
            if let Err(error) = controller.shutdown().await {
                tracing::warn!(%error, "local Host shutdown failed");
            }
        }
    }

    /// Seals the runtime against respawns, then runs the shutdown barrier.
    /// Used by every exit path (close, startup exit, update install).
    pub(crate) async fn shutdown_for_exit(&self) {
        self.sealed.store(true, Ordering::SeqCst);
        self.shutdown().await;
    }

    async fn ensure_controller(&self) -> Result<Arc<HostController>, CommandError> {
        if let Some(controller) = self.ready_controller() {
            return Ok(controller);
        }
        let _start = self.start_lock.lock().await;
        // 修复原因（Phase 6 更新安装）：退出屏障关闭 Host 后，渲染层重连会经 open_attachment
        // 再次拉起 Host，安装器随后覆盖仍被占用的安装目录。退出/安装开始后禁止再启动 Host。
        if self.sealed.load(Ordering::SeqCst) {
            return Err(crate::error::host(
                ErrorCode::HostNotStarted,
                "the app is exiting; the local Host will not be restarted",
            ));
        }
        if let Some(controller) = self.ready_controller() {
            return Ok(controller);
        }
        // 修复依据：Host 意外退出后槽位里仍是失效的 controller；先走完它的关闭屏障再重启，
        // 否则 open_attachment 会一直返回 hostNotStarted，直到重启整个应用。
        let stale = match std::mem::replace(&mut *self.slot(), HostSlot::Starting) {
            HostSlot::Running(controller) => Some(controller),
            HostSlot::Stopped | HostSlot::Starting | HostSlot::Failed { .. } => None,
        };
        emit_status(&self.app, &sink::status(HostStateDto::Starting));
        if let Some(stale) = stale {
            if let Err(error) = stale.shutdown().await {
                tracing::warn!(%error, "failed to clean up the exited Host");
            }
        }
        match self.spawn_controller().await {
            Ok(controller) => {
                *self.slot() = HostSlot::Running(Arc::clone(&controller));
                Ok(controller)
            }
            Err(error) => {
                *self.slot() = HostSlot::Failed {
                    code: error.code,
                    message: error.message.clone(),
                };
                emit_status(&self.app, &self.status());
                Err(error)
            }
        }
    }

    async fn spawn_controller(&self) -> Result<Arc<HostController>, CommandError> {
        self.launch.check_artifacts().await?;
        let node = node::locate(&self.launch.bundled_node(), self.launch.is_packaged()).await?;
        tracing::info!(
            program = %node.program.display(),
            version = node.version,
            source = ?node.source,
            "using the Node.js runtime for the local Host"
        );
        let device_mid = self
            .device_mid
            .get_or_try_init(|| launch::resolve_device_mid(&self.app))
            .await?;
        let config = self.launch.config(&node, device_mid).await;
        let sink = Arc::new(ShellHostSink::new(self.app.clone()));
        HostController::start(config, sink)
            .await
            .map(Arc::new)
            .map_err(|error| control_error(&error))
    }

    fn ready_controller(&self) -> Option<Arc<HostController>> {
        self.current_controller()
            .filter(|controller| controller.state().is_ready())
    }

    /// Returns the running, ready controller without spawning one
    /// (Phase 4 web remote control never starts the Host on its own).
    pub(crate) fn running_controller(&self) -> Option<Arc<HostController>> {
        self.ready_controller()
    }

    fn current_controller(&self) -> Option<Arc<HostController>> {
        match &*self.slot() {
            HostSlot::Running(controller) => Some(Arc::clone(controller)),
            HostSlot::Stopped | HostSlot::Starting | HostSlot::Failed { .. } => None,
        }
    }

    fn slot(&self) -> std::sync::MutexGuard<'_, HostSlot> {
        self.slot.lock().unwrap_or_else(PoisonError::into_inner)
    }
}
