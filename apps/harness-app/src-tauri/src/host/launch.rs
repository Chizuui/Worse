//! Launch inputs for the local Host sidecar and its `init-local` message.

use std::path::{Path, PathBuf};
use std::time::Duration;

use harness_host_control::HostConfig;
use harness_host_protocol::DEFAULT_ATTACHMENT_TTL;
use harness_protocol::{CommandError, ErrorCode};
use serde_json::{json, Value};
use tauri::{AppHandle, Manager};
use uuid::Uuid;

use super::layout::HostLayout;
use super::node::NodeRuntime;
use super::paths::{absolute, env_path, env_value, path_string};

const HOST_SHUTDOWN_GRACE: Duration = Duration::from_secs(3);
const HOST_ENTRY_BUILD: &str = "pnpm --filter @zcode/desktop exec tsup --config tsup.config.ts";
const SIDECAR_BUILD: &str = "node Worse/scripts/build-host-sidecar.mjs";

/// Process-independent launch inputs resolved once at shell startup.
pub(crate) struct HostLaunch {
    layout: HostLayout,
    launcher: PathBuf,
    host_entry: PathBuf,
    provider_config: PathBuf,
    locale: String,
    env: Vec<(String, String)>,
}

impl HostLaunch {
    /// Resolves the layout root (dev checkout or bundled resource) and applies
    /// the environment overrides.
    pub(crate) fn resolve(app: &AppHandle) -> Self {
        let layout = HostLayout::resolve(app);
        let environment = env_value("ZCODE_ENV").unwrap_or_else(|| "production".to_owned());
        let provider_config = env_path("ZCODE_BUILTIN_PROVIDER_CONFIG_FILE").map_or_else(
            || {
                layout.provider_config(if environment == "production" {
                    "zcode-builtin.json"
                } else {
                    "zcode-builtin.test.json"
                })
            },
            |explicit| absolute(&explicit),
        );
        let host_entry = env_path("HARNESS_HOST_ENTRY")
            .map_or_else(|| layout.host_entry(), |entry| absolute(&entry));
        let mut env = vec![
            ("ZCODE_ENV".to_owned(), environment),
            (
                "ZCODE_BUILTIN_PROVIDER_CONFIG_FILE".to_owned(),
                path_string(&provider_config),
            ),
        ];
        // 修复原因（Phase 3c 打包）：rust-bridge 原本按自身包目录猜 `../../target/release`，
        // 只因暂存目录镜像了仓库结构才碰巧正确。打包布局下由 layout.rs 明确给出 harness-host
        // 路径；用户显式设置的 HARNESS_RUST_HOST 仍然优先。
        if layout.is_packaged() && env_value("HARNESS_RUST_HOST").is_none() {
            env.push((
                "HARNESS_RUST_HOST".to_owned(),
                path_string(&layout.rust_host()),
            ));
        }
        // 修复原因（Phase 6 打包）：ZCode Main 把安装目录传给 Host（ZCODE_WINDOWS_APP_INSTALL_DIR），
        // 使 validateDataBaseDirTarget 拒绝把数据目录迁入安装目录；Harness 之前没有传，用户可以把
        // 数据迁进 %LOCALAPPDATA%\Harness，卸载时随安装目录一起被删除。
        if cfg!(windows) && env_value("ZCODE_WINDOWS_APP_INSTALL_DIR").is_none() {
            if let Some(install_dir) = layout.install_dir() {
                env.push((
                    "ZCODE_WINDOWS_APP_INSTALL_DIR".to_owned(),
                    path_string(&install_dir),
                ));
            }
        }
        Self {
            launcher: layout.launcher(),
            host_entry,
            provider_config,
            locale: match env_value("HARNESS_LOCALE").as_deref() {
                Some("zh-CN") => "zh-CN".to_owned(),
                _ => "en-US".to_owned(),
            },
            env,
            layout,
        }
    }

    /// The bundled Node.js executable of this layout (may not exist).
    pub(crate) fn bundled_node(&self) -> PathBuf {
        self.layout.bundled_node()
    }

    /// `true` for the bundled `host/` resource of a release build.
    pub(crate) fn is_packaged(&self) -> bool {
        self.layout.is_packaged()
    }

    /// Verifies the build outputs exist and names the command that builds them.
    pub(crate) async fn check_artifacts(&self) -> Result<(), CommandError> {
        for (path, build) in [
            (&self.host_entry, HOST_ENTRY_BUILD),
            (&self.launcher, SIDECAR_BUILD),
        ] {
            let exists = tokio::fs::metadata(path)
                .await
                .is_ok_and(|metadata| metadata.is_file());
            if !exists {
                return Err(crate::error::host(
                    ErrorCode::HostSpawnFailed,
                    format!("missing {}; {}", path.display(), self.layout.remedy(build)),
                ));
            }
        }
        for (name, path) in self.layout.optional_resources() {
            let exists = tokio::fs::metadata(&path)
                .await
                .is_ok_and(|metadata| metadata.is_file());
            if exists {
                tracing::info!(resource = name, path = %path.display(), "Host resource resolved");
            } else {
                tracing::warn!(
                    resource = name,
                    path = %path.display(),
                    "optional Host resource missing; the Host falls back to slower paths"
                );
            }
        }
        Ok(())
    }

    /// Builds the controller configuration for one Host process.
    ///
    /// Each Host process gets a fresh `databaseStartupId`; the message never
    /// carries `hostId`, `deliveryKind`, or `workspacePath`. The custom data
    /// directory is re-read per spawn, so a change made in settings applies on
    /// the next Host start (`ZCode` needs an app restart for the same change).
    pub(crate) async fn config(&self, node: &NodeRuntime, device_mid: &str) -> HostConfig {
        let root = self.layout.root();
        let mut env = self.env.clone();
        if let Some(home) = settings_home() {
            if let Some(data_dir) = read_data_base_dir(&home).await {
                tracing::info!(data_dir = %data_dir, "using the custom data directory from setting.json");
                env.push(("ZCODE_DATA_BASE_DIR".to_owned(), data_dir));
            }
        }
        let init_message: Value = json!({
            "type": "init-local",
            "databaseStartupId": Uuid::new_v4().to_string(),
            "locale": self.locale,
            "zcodeBuiltinProviderConfigFilePath": path_string(&self.provider_config),
            "deviceMid": device_mid,
            "agentSpawnFallbackCwd": path_string(root),
        });
        HostConfig {
            node_program: node.program.clone(),
            launcher: self.launcher.clone(),
            host_entry: self.host_entry.clone(),
            // Host 的 findUpward 以 cwd 为起点查找 zcode.cjs，所以 cwd 必须是布局根。
            cwd: root.to_path_buf(),
            init_message,
            env,
            shutdown_grace: HOST_SHUTDOWN_GRACE,
            attachment_ttl: DEFAULT_ATTACHMENT_TTL,
        }
    }
}

/// Home directory whose `.zcode/v2/setting.json` the Host reads and writes
/// (`ZCode` `settingService` order: `ZCODE_DESKTOP_HOME_DIR`, `HOME`, `USERPROFILE`).
fn settings_home() -> Option<PathBuf> {
    env_path("ZCODE_DESKTOP_HOME_DIR")
        .or_else(|| env_path("HOME"))
        .or_else(|| env_path("USERPROFILE"))
}

/// `dataBaseDir` from `<home>/.zcode/v2/setting.json`, when it is a non-empty string.
///
// 修复原因（Phase 6 数据目录对齐）：ZCode Main 启动时读取 setting.json 的 dataBaseDir 并以
// ZCODE_DATA_BASE_DIR 传给 Host（desktopDataBaseDirBootstrap.ts / buildHostProcessEnv）。
// Harness 壳没有这一步，在 ZCode 里迁移过数据目录的用户打开 Harness 会看到空数据。
// 文件缺失或格式不对时按"未设置"处理，与 ZCode 一致。
async fn read_data_base_dir(home: &Path) -> Option<String> {
    let file = home.join(".zcode").join("v2").join("setting.json");
    let text = tokio::fs::read_to_string(&file).await.ok()?;
    let value: Value = serde_json::from_str(&text).ok()?;
    let dir = value.get("dataBaseDir")?.as_str()?.trim();
    (!dir.is_empty()).then(|| dir.to_owned())
}

/// Returns `HARNESS_DEVICE_MID`, else a UUID persisted under the app data dir.
pub(crate) async fn resolve_device_mid(app: &AppHandle) -> Result<String, CommandError> {
    if let Some(explicit) = env_value("HARNESS_DEVICE_MID") {
        return Ok(explicit);
    }
    let directory = app.path().app_data_dir().map_err(|error| {
        crate::error::internal(format!("app data directory is unavailable: {error}"))
    })?;
    tokio::fs::create_dir_all(&directory)
        .await
        .map_err(|error| {
            crate::error::internal(format!("failed to create the app data directory: {error}"))
        })?;
    let path = directory.join("device-mid");
    if let Ok(contents) = tokio::fs::read_to_string(&path).await {
        let trimmed = contents.trim();
        if !trimmed.is_empty() {
            return Ok(trimmed.to_owned());
        }
    }
    let identifier = Uuid::new_v4().to_string();
    tokio::fs::write(&path, identifier.as_bytes())
        .await
        .map_err(|error| {
            crate::error::internal(format!("failed to persist the device id: {error}"))
        })?;
    Ok(identifier)
}

#[cfg(test)]
mod tests {
    use super::read_data_base_dir;

    #[tokio::test]
    async fn data_base_dir_comes_from_setting_json() {
        let home = std::env::temp_dir().join(format!("harness-setting-{}", std::process::id()));
        let settings = home.join(".zcode").join("v2");
        tokio::fs::create_dir_all(&settings).await.ok();
        let file = settings.join("setting.json");

        assert_eq!(read_data_base_dir(&home.join("missing")).await, None);
        tokio::fs::write(&file, br#"{"dataBaseDir":"  D:\\data  ","theme":"dark"}"#)
            .await
            .ok();
        assert_eq!(read_data_base_dir(&home).await.as_deref(), Some(r"D:\data"));
        tokio::fs::write(&file, br#"{"dataBaseDir":"   "}"#)
            .await
            .ok();
        assert_eq!(read_data_base_dir(&home).await, None);
        tokio::fs::write(&file, b"not json").await.ok();
        assert_eq!(read_data_base_dir(&home).await, None);
        tokio::fs::remove_dir_all(&home).await.ok();
    }
}
