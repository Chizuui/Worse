//! Public lifecycle API of one supervised Host sidecar.

mod remote_control;
mod resource_usage;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use harness_core::SidecarId;
use harness_host_protocol::{HostState, ShellLine, HOST_ATTACH_TIMEOUT};
use harness_sidecar::{SidecarError, TokioSupervisor};
use serde_json::{json, Value};
use tokio::sync::{oneshot, OnceCell};
use tokio::task::JoinHandle;

use crate::attach::{self, AttachCredentials};
use crate::config::HostConfig;
use crate::control::StartupControl;
use crate::dispatcher;
use crate::error::HostControlError;
use crate::handshake;
use crate::shared::{lock, HostShared};
use crate::sink::HostSink;

/// How long shutdown waits for the reader to observe stdout EOF after the kill.
const READER_DRAIN_TIMEOUT: Duration = Duration::from_secs(2);

/// One attachment grant handed to the webview.
#[derive(Debug, Clone, PartialEq)]
pub struct AttachmentGrant {
    /// Attachment id echoed by the WebSocket hello.
    pub attachment_id: String,
    /// Loopback WebSocket URL; never contains the token.
    pub url: String,
    /// Single-use 64-hex token; never log it.
    pub token: String,
    /// Startup id this Host process publishes `database-startup-state` under.
    pub database_startup_id: String,
    /// Latest accepted `database-startup-state`, if any.
    pub startup_state: Option<Value>,
}

/// Supervises exactly one Node Host sidecar.
pub struct HostController {
    supervisor: TokioSupervisor,
    sidecar_id: SidecarId,
    shared: Arc<HostShared>,
    init_message: Value,
    attachment_ttl: Duration,
    reader: Mutex<Option<JoinHandle<()>>>,
    shutdown: OnceCell<Result<(), HostControlError>>,
}

impl HostController {
    /// Spawns `node --no-warnings <launcher>` and waits for `ready`.
    ///
    /// # Errors
    ///
    /// Returns [`HostControlError::Spawn`] when a launch file is missing or the
    /// sidecar exits or fails before `ready`, [`HostControlError::Timeout`]
    /// when `ready` does not arrive in time, and [`HostControlError::Protocol`]
    /// for an invalid `init-local` message or bridge version.
    pub async fn start(
        config: HostConfig,
        sink: Arc<dyn HostSink>,
    ) -> Result<Self, HostControlError> {
        let (init_message, startup_id) = attach::prepare_init_message(&config.init_message)?;
        handshake::ensure_file(&config.launcher, "sidecar launcher").await?;
        handshake::ensure_file(&config.host_entry, "Host entry").await?;

        sink.on_state(&HostState::Starting { pid: None });
        let supervisor = TokioSupervisor::new();
        let sidecar_id = SidecarId::new("harness-host-node");
        let spec = handshake::sidecar_spec(&sidecar_id, &config);
        let (mut io, pid) = match handshake::spawn(&supervisor, spec).await {
            Ok(spawned) => spawned,
            Err(error) => {
                sink.on_state(&failed_state(&error));
                return Err(error);
            }
        };
        let shared = Arc::new(HostShared::new(io.stdin.clone(), sink, startup_id));
        shared.set_state(&HostState::Starting { pid });

        let ready = match handshake::wait_ready(&shared, &mut io).await {
            Ok(ready) => ready,
            Err(error) => {
                shared.begin_shutdown();
                if let Err(stop) = supervisor.shutdown(&sidecar_id).await {
                    tracing::warn!(%stop, "failed to stop a sidecar that never became ready");
                }
                shared.set_state(&failed_state(&error));
                return Err(error);
            }
        };
        shared.set_ws_port(ready.ws_port);
        shared.set_state(&HostState::Ready {
            pid: ready.host_pid,
            ws_port: ready.ws_port,
        });
        tracing::info!(
            pid = ready.host_pid,
            port = ready.ws_port,
            "Host sidecar ready"
        );
        let reader = dispatcher::spawn_reader(Arc::clone(&shared), io.stdout);

        Ok(Self {
            supervisor,
            sidecar_id,
            shared,
            init_message,
            attachment_ttl: config.attachment_ttl,
            reader: Mutex::new(Some(reader)),
            shutdown: OnceCell::new(),
        })
    }

    /// Issues one attachment grant and waits for `attach.opened`.
    ///
    /// The first attachment carries `init-local`; once any attachment became
    /// active, later ones carry `attach-service-port`.
    ///
    /// # Errors
    ///
    /// Returns [`HostControlError::NotRunning`] when the Host is not ready,
    /// [`HostControlError::Timeout`] after [`HOST_ATTACH_TIMEOUT`],
    /// [`HostControlError::Protocol`] when the sidecar rejects the request, and
    /// [`HostControlError::Io`] when stdin is closed.
    pub async fn open_attachment(&self) -> Result<AttachmentGrant, HostControlError> {
        let ws_port = self.running_port()?;
        let credentials = AttachCredentials::generate();
        let host_message = if self.shared.is_initialized() {
            attach::service_port_message(&credentials)
        } else {
            self.init_message.clone()
        };
        let line = credentials.open_line(self.attachment_ttl, host_message);

        let (sender, receiver) = oneshot::channel();
        // 先登记 pending 再写 attach.open，避免 sidecar 回复早于登记而丢失。
        self.shared
            .insert_pending(credentials.request_id.clone(), sender);
        if let Err(error) = self.shared.send(&ShellLine::attach_open(line)).await {
            self.shared.take_pending(&credentials.request_id);
            return Err(error);
        }
        let outcome = tokio::time::timeout(HOST_ATTACH_TIMEOUT, receiver).await;
        match outcome {
            Ok(Ok(Ok(()))) => {}
            Ok(Ok(Err(error))) => return Err(error),
            Ok(Err(_closed)) => return Err(HostControlError::NotRunning),
            Err(_elapsed) => {
                self.shared.take_pending(&credentials.request_id);
                return Err(HostControlError::Timeout(
                    "sidecar did not confirm the attachment".to_owned(),
                ));
            }
        }

        let startup = self.shared.startup();
        Ok(AttachmentGrant {
            attachment_id: credentials.attachment_id,
            url: attach::attach_url(ws_port),
            token: credentials.token,
            database_startup_id: startup.startup_id().to_owned(),
            startup_state: startup.latest(),
        })
    }

    /// Forwards a `DatabaseStartupControl` to the Host.
    ///
    /// Returns the cached snapshot for `snapshot`, otherwise `None`.
    ///
    /// # Errors
    ///
    /// Returns [`HostControlError::Protocol`] for an invalid control and
    /// [`HostControlError::NotRunning`] when `retry` cannot reach the Host.
    pub async fn database_startup_control(
        &self,
        control: Value,
    ) -> Result<Option<Value>, HostControlError> {
        let action = StartupControl::parse(&control).ok_or_else(|| {
            HostControlError::Protocol("invalid database startup control".to_owned())
        })?;
        let cached = match action {
            StartupControl::Snapshot => self.latest_startup_state(),
            StartupControl::Retry | StartupControl::Exit => None,
        };
        let message = json!({ "type": "database-startup-control", "control": control });
        match (self.running_port(), action) {
            (Ok(_), _) => self.shared.send_host(message).await?,
            (Err(error), StartupControl::Retry) => return Err(error),
            (Err(_), StartupControl::Snapshot | StartupControl::Exit) => {}
        }
        Ok(cached)
    }

    /// Returns the latest accepted `database-startup-state`.
    #[must_use]
    pub fn latest_startup_state(&self) -> Option<Value> {
        self.shared.startup().latest()
    }

    /// Returns the current lifecycle state.
    #[must_use]
    pub fn state(&self) -> HostState {
        self.shared.state()
    }

    /// Sends `dispose`, then runs the supervisor barrier (close stdin, wait
    /// the grace period, kill the tree). Idempotent; concurrent callers share
    /// one barrier and its result.
    ///
    /// # Errors
    ///
    /// Returns [`HostControlError::Shutdown`] when the process tree could not
    /// be stopped cleanly.
    pub async fn shutdown(&self) -> Result<(), HostControlError> {
        self.shutdown
            .get_or_init(|| self.run_shutdown())
            .await
            .clone()
    }

    async fn run_shutdown(&self) -> Result<(), HostControlError> {
        self.shared.begin_shutdown();
        if let Err(error) = self.shared.send_host(json!({ "type": "dispose" })).await {
            tracing::debug!(%error, "dispose could not be delivered; continuing the barrier");
        }
        let stopped = match self.supervisor.shutdown(&self.sidecar_id).await {
            Ok(()) | Err(SidecarError::NotFound(_)) => Ok(()),
            Err(error) => Err(HostControlError::Shutdown(error.to_string())),
        };
        let reader = lock(&self.reader).take();
        if let Some(reader) = reader {
            if tokio::time::timeout(READER_DRAIN_TIMEOUT, reader)
                .await
                .is_err()
            {
                tracing::warn!("Host reader did not drain after shutdown");
            }
        }
        self.shared.fail_pending(&HostControlError::NotRunning);
        self.shared.set_state(&HostState::Stopped);
        tracing::info!("Host sidecar shut down");
        stopped
    }

    fn running_port(&self) -> Result<u16, HostControlError> {
        if self.shared.is_shutting_down() || !self.shared.state().is_ready() {
            return Err(HostControlError::NotRunning);
        }
        self.shared.ws_port().ok_or(HostControlError::NotRunning)
    }
}

impl Drop for HostController {
    fn drop(&mut self) {
        if let Some(reader) = lock(&self.reader).take() {
            reader.abort();
        }
    }
}

fn failed_state(error: &HostControlError) -> HostState {
    HostState::Failed {
        code: error.code().to_owned(),
        message: error.to_string(),
    }
}
