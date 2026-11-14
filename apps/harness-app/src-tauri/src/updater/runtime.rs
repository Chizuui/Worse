//! Desktop updater runtime: the phase machine, network access through
//! `tauri-plugin-updater`, and the install orchestration (spec section 4).
//!
//! Locking: `inner` is a std mutex that is never held across `.await`;
//! events are emitted after the guard is dropped.

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use harness_protocol::CommandError;
use serde::Deserialize;
use serde_json::{json, Value};
use tauri::async_runtime::JoinHandle;
use tauri::{AppHandle, Emitter, Manager, State, Url};
use tauri_plugin_updater::{Update, UpdaterExt};

use super::model::{self, CheckResult, FeedDecision, Phase, StatePayload};
use super::store::Store;
use crate::state::AppState;

/// `UpdateStatePayload` broadcast.
const STATE_EVENT: &str = "harness://updater/state";
/// `UpdateCheckResultPayload` broadcast (manual checks, download failures).
const CHECK_RESULT_EVENT: &str = "harness://updater/check-result";
/// Version string once a verified package is ready (`onUpdateReady`).
const READY_EVENT: &str = "harness://updater/ready";

/// `ZCode` `AUTO_UPDATE_POLL_INTERVAL_MS`.
const POLL_INTERVAL: Duration = Duration::from_secs(15 * 60);
/// `ZCode` `AUTO_UPDATE_VISIBLE_POLL_INTERVAL_MS`.
const VISIBLE_POLL_INTERVAL: Duration = Duration::from_secs(2 * 60);
/// Upper bound for one `latest.json` request (downloads have no total timeout).
const CHECK_TIMEOUT: Duration = Duration::from_secs(30);

const FEED_ENV: &str = "HARNESS_UPDATE_FEED_URL";
const DATA_DIR_ENV: &str = "HARNESS_APP_DATA_DIR";
/// Production public key, supplied when the shell is compiled (spec section 9).
const BUILD_PUBKEY: Option<&str> = option_env!("HARNESS_UPDATER_PUBKEY");

/// Feed + key of an enabled updater.
struct Active {
    feed: Url,
    pubkey: String,
    loopback: bool,
}

/// A downloaded package whose minisign signature was verified in this process.
struct ReadyPackage {
    update: Update,
    bytes: Arc<Vec<u8>>,
}

struct Inner {
    phase: Phase,
    check_in_flight: bool,
    /// The update announced by the last check (`Available`).
    pending: Option<Update>,
    ready: Option<ReadyPackage>,
    download: Option<JoinHandle<()>>,
    /// Bumped on every start/cancel so stale download callbacks are dropped.
    download_generation: u64,
    last_error: Option<String>,
}

/// Owner of the window's update state.
pub(crate) struct UpdaterRuntime {
    app: AppHandle,
    active: Result<Active, String>,
    current_version: String,
    store: Store,
    inner: Mutex<Inner>,
}

/// Events to send once the state lock is released.
#[derive(Default)]
struct Outbox {
    state: Option<StatePayload>,
    check: Option<CheckResult>,
    ready: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct Params {
    manual: bool,
    version: Option<String>,
    enabled: Option<bool>,
}

impl UpdaterRuntime {
    /// Resolves feed, key and storage; never touches the network.
    pub(crate) fn new(app: &AppHandle) -> Arc<Self> {
        let plugin_config = app.config().plugins.0.get("updater").cloned();
        let configured = |key: &str| {
            plugin_config
                .as_ref()
                .and_then(|config| config.get(key))
                .cloned()
        };
        let configured_feed = configured("endpoints").and_then(|endpoints| {
            endpoints
                .as_array()
                .and_then(|list| list.first())
                .and_then(Value::as_str)
                .map(ToOwned::to_owned)
        });
        let configured_pubkey =
            configured("pubkey").and_then(|value| value.as_str().map(ToOwned::to_owned));
        let insecure_loopback_allowed = cfg!(debug_assertions)
            || configured("dangerousInsecureTransportProtocol")
                .and_then(|value| value.as_bool())
                .unwrap_or(false);
        let env_feed = std::env::var(FEED_ENV).ok();
        let active = match model::resolve_feed(
            env_feed.as_deref(),
            configured_feed.as_deref(),
            insecure_loopback_allowed,
        ) {
            FeedDecision::Off(reason) => Err(reason),
            FeedDecision::On(feed) => {
                match model::resolve_pubkey(BUILD_PUBKEY, configured_pubkey.as_deref()) {
                    None => {
                        Err("no updater signing key is configured (placeholder key)".to_owned())
                    }
                    Some(pubkey) => Ok(Active {
                        loopback: model::is_loopback(&feed),
                        feed,
                        pubkey,
                    }),
                }
            }
        };
        Arc::new(Self {
            app: app.clone(),
            active,
            current_version: app.package_info().version.to_string(),
            store: Store::new(data_directory(app)),
            inner: Mutex::new(Inner {
                phase: Phase::Idle,
                check_in_flight: false,
                pending: None,
                ready: None,
                download: None,
                download_generation: 0,
                last_error: None,
            }),
        })
    }

    /// Starts the startup check and the poll loop when the updater is on.
    pub(crate) fn start(self: &Arc<Self>) {
        match &self.active {
            Err(reason) => {
                tracing::info!(reason = %reason, "auto-update is off");
                return;
            }
            Ok(active) => tracing::info!(
                feed = %redact(&active.feed),
                version = %self.current_version,
                "auto-update is on"
            ),
        }
        let runtime = Arc::clone(self);
        tauri::async_runtime::spawn(async move {
            loop {
                runtime.check(false).await;
                let interval = if runtime.lock().phase.has_visible_target() {
                    VISIBLE_POLL_INTERVAL
                } else {
                    POLL_INTERVAL
                };
                tokio::time::sleep(interval).await;
            }
        });
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn send(&self, outbox: Outbox) {
        if let Some(state) = outbox.state {
            self.emit(STATE_EVENT, &state);
        }
        if let Some(check) = outbox.check {
            self.emit(CHECK_RESULT_EVENT, &check);
        }
        if let Some(version) = outbox.ready {
            self.emit(READY_EVENT, &version);
        }
    }

    fn emit<T: serde::Serialize + Clone>(&self, event: &str, payload: &T) {
        if let Err(error) = self.app.emit(event, payload.clone()) {
            tracing::warn!(%error, event, "failed to emit an updater event");
        }
    }

    /// One feed check (startup, poll or manual). Results go out as events.
    pub(crate) async fn check(self: &Arc<Self>, manual: bool) {
        let active = match &self.active {
            Ok(active) => active,
            Err(reason) => {
                tracing::info!(reason = %reason, manual, "update check skipped: updater is off");
                if manual {
                    self.send(Outbox {
                        check: Some(CheckResult::DevSkipped),
                        ..Outbox::default()
                    });
                }
                return;
            }
        };
        let mut outbox = Outbox::default();
        {
            let mut inner = self.lock();
            match &inner.phase {
                Phase::Installing { .. } => return,
                Phase::Downloading {
                    version,
                    transferred,
                    total,
                    ..
                } => {
                    if manual {
                        outbox.check = Some(CheckResult::AlreadyDownloading {
                            version: version.clone(),
                            progress: model::progress_percent(*transferred, *total),
                        });
                        drop(inner);
                        self.send(outbox);
                    }
                    return;
                }
                _ => {}
            }
            if inner.check_in_flight {
                drop(inner);
                if manual {
                    self.send(Outbox {
                        check: Some(CheckResult::Error {
                            message: "Update check already in progress.".to_owned(),
                        }),
                        ..Outbox::default()
                    });
                }
                return;
            }
            inner.check_in_flight = true;
            // 已展示的更新目标不能被一次复查盖成 checking（与 ZCode 一致）。
            if matches!(inner.phase, Phase::Idle) {
                inner.phase = Phase::Checking;
                outbox.state = Some(inner.phase.payload());
            }
        }
        self.send(outbox);
        if manual {
            // 用户手动检查代表重新关注被跳过的版本（ZCode clearSkippedUpdateVersionForManualCheck）。
            if let Err(error) = self
                .store
                .update(|value| value.skipped_version = None)
                .await
            {
                tracing::warn!(%error, "failed to clear the skipped update version");
            }
        }
        let outcome = self.fetch(active).await;
        self.settle_check(outcome, manual).await;
    }

    async fn fetch(&self, active: &Active) -> Result<Option<Update>, String> {
        let mut builder = self
            .app
            .updater_builder()
            .pubkey(active.pubkey.clone())
            .timeout(CHECK_TIMEOUT)
            .endpoints(vec![active.feed.clone()])
            .map_err(|error| error.to_string())?;
        if active.loopback {
            // 回环测试 feed 不能被系统代理转发。
            builder = builder.no_proxy();
        }
        let updater = builder.build().map_err(|error| error.to_string())?;
        updater.check().await.map_err(|error| error.to_string())
    }

    async fn settle_check(self: &Arc<Self>, outcome: Result<Option<Update>, String>, manual: bool) {
        let preferences = self.store.load().await;
        let skipped = if manual {
            None
        } else {
            preferences.skipped_version
        };
        let mut outbox = Outbox::default();
        let mut start_download = false;
        {
            let mut inner = self.lock();
            inner.check_in_flight = false;
            if matches!(inner.phase, Phase::Installing { .. }) {
                return;
            }
            match outcome {
                Err(message) => {
                    tracing::warn!(error = %message, manual, "update check failed");
                    inner.last_error = Some(message.clone());
                    if matches!(inner.phase, Phase::Checking) {
                        inner.phase = Phase::Idle;
                    }
                    if manual {
                        outbox.check = Some(CheckResult::Error { message });
                    }
                }
                Ok(None) => {
                    tracing::info!(current = %self.current_version, "no newer update in the feed");
                    inner.last_error = None;
                    match &inner.phase {
                        Phase::Ready { version, .. } => {
                            if manual {
                                outbox.check = Some(CheckResult::Ready {
                                    version: version.clone(),
                                });
                            }
                        }
                        Phase::Downloading { .. } => {}
                        _ => {
                            inner.pending = None;
                            inner.phase = Phase::Idle;
                            if manual {
                                outbox.check = Some(CheckResult::UpToDate {
                                    current_version: self.current_version.clone(),
                                });
                            }
                        }
                    }
                }
                Ok(Some(update)) => {
                    inner.last_error = None;
                    let version = update.version.clone();
                    tracing::info!(version = %version, current = %self.current_version, "update available");
                    let ready_version = match &inner.phase {
                        Phase::Ready { version, .. } => Some(version.clone()),
                        _ => None,
                    };
                    if matches!(inner.phase, Phase::Downloading { .. }) {
                        // 下载进行中：这次检查不打断它，下载结束后的轮询再比较目标。
                    } else if ready_version.as_deref() == Some(version.as_str()) {
                        if manual {
                            outbox.check = Some(CheckResult::Ready { version });
                        }
                    } else if skipped.as_deref() == Some(version.as_str()) {
                        tracing::info!(version = %version, "ignoring a skipped update version");
                        if ready_version.is_none() {
                            inner.pending = None;
                            inner.phase = Phase::Idle;
                        }
                    } else {
                        let notes = model::release_notes(
                            &version,
                            update.body.as_deref(),
                            update.raw_json.get("pub_date").and_then(Value::as_str),
                        );
                        // 远端目标已高于已下载的包：丢弃旧包，绝不安装低于目标的版本。
                        inner.ready = None;
                        inner.pending = Some(update);
                        inner.phase = Phase::Available {
                            version: version.clone(),
                            notes: notes.clone(),
                        };
                        if manual {
                            outbox.check = Some(CheckResult::Available {
                                version,
                                release_notes: notes,
                            });
                        }
                        start_download = preferences.auto_download_and_install_updates;
                    }
                }
            }
            outbox.state = Some(inner.phase.payload());
        }
        self.send(outbox);
        if start_download {
            self.start_download("auto-download");
        }
    }

    /// Starts downloading the available update; a no-op in any other phase.
    pub(crate) fn start_download(self: &Arc<Self>, reason: &str) {
        let (update, generation, payload) = {
            let mut inner = self.lock();
            let Phase::Available { version, notes } = inner.phase.clone() else {
                tracing::info!(
                    reason,
                    state = inner.phase.kind(),
                    "download request ignored"
                );
                return;
            };
            let Some(update) = inner.pending.clone() else {
                tracing::info!(reason, "download request ignored: no checked update");
                return;
            };
            inner.download_generation = inner.download_generation.wrapping_add(1);
            inner.phase = Phase::Downloading {
                version,
                notes,
                transferred: 0,
                total: None,
            };
            (update, inner.download_generation, inner.phase.payload())
        };
        tracing::info!(reason, version = %update.version, "downloading update");
        self.send(Outbox {
            state: Some(payload),
            ..Outbox::default()
        });
        let runtime = Arc::clone(self);
        let handle = tauri::async_runtime::spawn(async move {
            runtime.run_download(update, generation).await;
        });
        let mut inner = self.lock();
        if inner.download_generation == generation
            && matches!(inner.phase, Phase::Downloading { .. })
        {
            inner.download = Some(handle);
        }
    }

    async fn run_download(self: Arc<Self>, update: Update, generation: u64) {
        let progress = Arc::clone(&self);
        let mut transferred: u64 = 0;
        let mut last_percent: Option<String> = None;
        let result = update
            .download(
                move |chunk, total| {
                    transferred =
                        transferred.saturating_add(u64::try_from(chunk).unwrap_or(u64::MAX));
                    let percent = model::progress_percent(transferred, total);
                    if last_percent.as_deref() == Some(percent.as_str()) {
                        return;
                    }
                    last_percent = Some(percent);
                    progress.on_progress(generation, transferred, total);
                },
                || {},
            )
            .await;
        self.finish_download(generation, update, result);
    }

    fn on_progress(&self, generation: u64, bytes: u64, size: Option<u64>) {
        let payload = {
            let mut inner = self.lock();
            if inner.download_generation != generation {
                return;
            }
            let Phase::Downloading {
                transferred, total, ..
            } = &mut inner.phase
            else {
                return;
            };
            *transferred = bytes;
            *total = size;
            inner.phase.payload()
        };
        self.send(Outbox {
            state: Some(payload),
            ..Outbox::default()
        });
    }

    fn finish_download(
        &self,
        generation: u64,
        update: Update,
        result: Result<Vec<u8>, tauri_plugin_updater::Error>,
    ) {
        let mut outbox = Outbox::default();
        {
            let mut inner = self.lock();
            let Phase::Downloading { version, notes, .. } = inner.phase.clone() else {
                return;
            };
            if inner.download_generation != generation {
                // 已取消或已跳过的下载：迟到结果不能把状态推回下载完成。
                return;
            }
            inner.download = None;
            match result {
                Ok(bytes) => {
                    tracing::info!(version = %version, bytes = bytes.len(), "update downloaded and signature verified");
                    inner.ready = Some(ReadyPackage {
                        update,
                        bytes: Arc::new(bytes),
                    });
                    inner.pending = None;
                    inner.last_error = None;
                    inner.phase = Phase::Ready {
                        version: version.clone(),
                        notes,
                    };
                    outbox.ready = Some(version);
                }
                Err(error) => {
                    // 签名/版本校验失败或网络失败：回到“发现更新”，绝不进入可安装状态（fail closed）。
                    let message = error.to_string();
                    tracing::warn!(version = %version, error = %message, "update download rejected");
                    inner.last_error = Some(message.clone());
                    inner.phase = Phase::Available { version, notes };
                    outbox.check = Some(CheckResult::Error { message });
                }
            }
            outbox.state = Some(inner.phase.payload());
        }
        self.send(outbox);
    }

    /// Cancels the running download and returns to `update-available`.
    pub(crate) fn cancel_download(&self) {
        let payload = {
            let mut inner = self.lock();
            let Phase::Downloading { version, notes, .. } = inner.phase.clone() else {
                return;
            };
            inner.download_generation = inner.download_generation.wrapping_add(1);
            if let Some(handle) = inner.download.take() {
                handle.abort();
            }
            tracing::info!(version = %version, "update download cancelled");
            inner.phase = Phase::Available { version, notes };
            inner.phase.payload()
        };
        self.send(Outbox {
            state: Some(payload),
            ..Outbox::default()
        });
    }

    /// Skips the announced version (also while it downloads) and persists it.
    pub(crate) async fn skip_version(&self, version: &str) {
        let payload = {
            let mut inner = self.lock();
            let announced = match &inner.phase {
                Phase::Available { version, .. } | Phase::Downloading { version, .. } => {
                    version.clone()
                }
                _ => String::new(),
            };
            if announced != version {
                tracing::info!(version, state = inner.phase.kind(), "skip request ignored");
                return;
            }
            inner.download_generation = inner.download_generation.wrapping_add(1);
            if let Some(handle) = inner.download.take() {
                handle.abort();
            }
            inner.pending = None;
            inner.phase = Phase::Idle;
            inner.phase.payload()
        };
        self.send(Outbox {
            state: Some(payload),
            ..Outbox::default()
        });
        let skipped = version.to_owned();
        if let Err(error) = self
            .store
            .update(|value| value.skipped_version = Some(skipped))
            .await
        {
            tracing::warn!(%error, "failed to persist the skipped update version");
        }
    }

    /// Drains the Host, then hands the verified package to the installer.
    /// Returns only when the install was refused.
    pub(crate) async fn quit_and_install(&self) -> Result<(), CommandError> {
        let (update, bytes, version, notes, payload) = {
            let mut inner = self.lock();
            let not_ready = || {
                crate::error::invalid_input(format!(
                    "Update is not ready to install: state={}",
                    inner.phase.kind()
                ))
            };
            let Phase::Ready { version, notes } = inner.phase.clone() else {
                return Err(not_ready());
            };
            let Some((update, bytes)) = inner
                .ready
                .as_ref()
                .map(|package| (package.update.clone(), Arc::clone(&package.bytes)))
            else {
                return Err(not_ready());
            };
            inner.phase = Phase::Installing {
                version: version.clone(),
                notes: notes.clone(),
            };
            (update, bytes, version, notes, inner.phase.payload())
        };
        self.send(Outbox {
            state: Some(payload),
            ..Outbox::default()
        });
        let Some(app_state) = self.app.try_state::<AppState>() else {
            self.restore_ready(version, notes);
            return Err(crate::error::internal("the shell state is not ready"));
        };
        if app_state.begin_exit() {
            // 关闭屏障已在运行并会结束进程；不能再并行启动安装器。
            self.restore_ready(version, notes);
            return Err(crate::error::internal("the app is already shutting down"));
        }
        if let Some(notes) = notes {
            if let Err(error) = self
                .store
                .update(|value| value.pending_release_notes = Some(notes))
                .await
            {
                tracing::warn!(%error, "failed to persist the post-update release notes");
            }
        }
        tracing::info!(version = %version, "update install: draining the Host before the installer starts");
        app_state.host().shutdown_for_exit().await;
        tracing::info!(version = %version, "update install: Host drained, launching the installer");
        let installed =
            tauri::async_runtime::spawn_blocking(move || update.install(bytes.as_slice())).await;
        match installed {
            Ok(Ok(())) => tracing::info!(version = %version, "update installed; restarting"),
            Ok(Err(error)) => {
                tracing::error!(%error, version = %version, "update install failed; restarting the current version");
            }
            Err(error) => {
                tracing::error!(%error, version = %version, "update install task failed; restarting the current version");
            }
        }
        // Host 已按退出屏障关闭：安装成功（macOS/Linux）或失败都必须重启，
        // 不能停留在没有 Host/远控的旧窗口里（对应 ZCode recoverFromFailedUpdateInstall）。
        self.app.restart()
    }

    fn restore_ready(&self, version: String, notes: Option<model::ReleaseNotes>) {
        let payload = {
            let mut inner = self.lock();
            inner.phase = Phase::Ready { version, notes };
            inner.phase.payload()
        };
        self.send(Outbox {
            state: Some(payload),
            ..Outbox::default()
        });
    }

    /// macOS/Linux install-on-quit after the close barrier (`ZCode` `before-quit`).
    #[cfg(not(windows))]
    async fn install_ready_on_quit(&self) {
        let package = {
            let mut inner = self.lock();
            let Phase::Ready { version, notes } = inner.phase.clone() else {
                return;
            };
            let Some(package) = inner
                .ready
                .as_ref()
                .map(|package| (package.update.clone(), Arc::clone(&package.bytes)))
            else {
                return;
            };
            inner.phase = Phase::Installing { version, notes };
            package
        };
        let (update, bytes) = package;
        let version = update.version.clone();
        match tauri::async_runtime::spawn_blocking(move || update.install(bytes.as_slice())).await {
            Ok(Ok(())) => tracing::info!(version = %version, "update installed on quit"),
            Ok(Err(error)) => tracing::error!(%error, version = %version, "install on quit failed"),
            Err(error) => {
                tracing::error!(%error, version = %version, "install on quit task failed")
            }
        }
    }

    async fn take_release_notes(&self) -> Value {
        let preferences = self.store.load().await;
        match preferences.pending_release_notes {
            Some(notes) if notes.version == self.current_version => {
                serde_json::to_value(notes).unwrap_or(Value::Null)
            }
            _ => Value::Null,
        }
    }

    async fn ack_release_notes(&self, version: &str) {
        let result = self
            .store
            .update(|value| {
                if value
                    .pending_release_notes
                    .as_ref()
                    .is_some_and(|notes| notes.version == version)
                {
                    value.pending_release_notes = None;
                }
            })
            .await;
        if let Err(error) = result {
            tracing::warn!(%error, "failed to clear the post-update release notes");
        }
    }

    fn diagnostics(&self) -> Value {
        let inner = self.lock();
        json!({
            "phase": inner.phase.kind(),
            "state": inner.phase.payload(),
            "lastError": inner.last_error,
            "currentVersion": self.current_version,
            "enabled": self.active.is_ok(),
            "disabledReason": self.active.as_ref().err(),
        })
    }

    async fn dispatch(
        &self,
        this: &Arc<Self>,
        method: &str,
        params: Params,
    ) -> Result<Value, CommandError> {
        match method {
            "getState" => {
                Ok(serde_json::to_value(self.lock().phase.payload()).unwrap_or(Value::Null))
            }
            "check" => {
                this.check(params.manual).await;
                Ok(Value::Null)
            }
            "download" => {
                this.start_download("renderer");
                Ok(Value::Null)
            }
            "cancelDownload" => {
                self.cancel_download();
                Ok(Value::Null)
            }
            "skipVersion" => {
                let version = params.version.unwrap_or_default();
                let version = version.trim();
                if version.is_empty() {
                    return Err(crate::error::invalid_input("skipVersion needs a version"));
                }
                self.skip_version(version).await;
                Ok(Value::Null)
            }
            "quitAndInstall" => self.quit_and_install().await.map(|()| Value::Null),
            "getPreferences" => {
                let preferences = self.store.load().await;
                Ok(
                    json!({ "autoDownloadAndInstallUpdates": preferences.auto_download_and_install_updates }),
                )
            }
            "setAutoDownload" => {
                let enabled = params.enabled.unwrap_or(false);
                self.store
                    .update(|value| value.auto_download_and_install_updates = enabled)
                    .await
                    .map_err(|error| {
                        crate::error::internal(format!(
                            "failed to save the update preference: {error}"
                        ))
                    })?;
                Ok(Value::Null)
            }
            "takeReleaseNotes" => Ok(self.take_release_notes().await),
            "ackReleaseNotes" => {
                self.ack_release_notes(params.version.as_deref().unwrap_or_default())
                    .await;
                Ok(Value::Null)
            }
            "getDiagnostics" => Ok(self.diagnostics()),
            other => Err(crate::error::invalid_input(format!(
                "unknown updater method `{other}`"
            ))),
        }
    }
}

/// Renderer entry point for every update member of `IPlatformService`.
#[tauri::command]
pub(crate) async fn updater_call(
    updater: State<'_, Arc<UpdaterRuntime>>,
    method: String,
    params: Option<Value>,
) -> Result<Value, CommandError> {
    let params = match params {
        None | Some(Value::Null) => Params::default(),
        Some(value) => serde_json::from_value(value).map_err(|error| {
            crate::error::invalid_input(format!("invalid updater params: {error}"))
        })?,
    };
    let runtime = Arc::clone(updater.inner());
    runtime.dispatch(&runtime, &method, params).await
}

/// Close-barrier hook: installs a verified update on macOS/Linux after the
/// Host has been drained. Windows installs only on explicit request.
#[cfg(not(windows))]
pub(crate) async fn install_on_quit<R: tauri::Runtime>(app: &AppHandle<R>) {
    if let Some(updater) = app.try_state::<Arc<UpdaterRuntime>>() {
        let runtime = Arc::clone(updater.inner());
        runtime.install_ready_on_quit().await;
    }
}

/// Windows never installs on quit (`ZCode` parity: explicit install only).
#[cfg(windows)]
#[allow(clippy::unused_async)]
pub(crate) async fn install_on_quit<R: tauri::Runtime>(_app: &AppHandle<R>) {}

fn data_directory(app: &AppHandle) -> std::path::PathBuf {
    if let Some(explicit) = std::env::var_os(DATA_DIR_ENV).filter(|value| !value.is_empty()) {
        return explicit.into();
    }
    app.path().app_data_dir().unwrap_or_else(|error| {
        tracing::warn!(%error, "app data directory is unavailable; updater preferences use the temp dir");
        std::env::temp_dir().join("harness-updater")
    })
}

/// Feed URL for logs without query or credentials.
fn redact(url: &Url) -> String {
    format!(
        "{}://{}{}",
        url.scheme(),
        url.host_str().unwrap_or_default(),
        url.path()
    )
}
