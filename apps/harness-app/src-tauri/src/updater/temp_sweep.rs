//! Removes the installer directories `tauri-plugin-updater` leaves in `%TEMP%`
//! on Windows (spec `phase-6-updater.md` section 4.1).
//!
//! 修复原因：每次 Windows 更新安装后都会残留 `%TEMP%\<productName>-<新版本>-updater-<6 位随机>`
//! （内含约 51 MB 的 NSIS 安装包），phase6-update gate 每次运行都观察到这一目录。
//! 证据（tauri-plugin-updater 2.14.1 `src/updater.rs`）：`make_temp_dir()` 用
//! `tempfile::Builder::new().prefix("{app_name}-{version}-updater-").tempdir()?.keep()` 创建目录，
//! `keep()` 放弃了自动删除；随后 `install_inner()` 在 `ShellExecuteW` 启动安装器后直接
//! `std::process::exit(0)`，安装包的 `TempPath` 也不会被 drop。插件没有提供清理选项或钩子
//! （`on_before_exit` 发生在安装器启动之前，那时安装包还要用）。
//! 修复机制：新版本启动时（安装器可能仍在退出，被占用的文件删不掉时留待下次）以及窗口关闭屏障中
//! （安装器早已退出）各清理一次，只删除严格符合
//! `<本产品 productName>-<semver>-updater-<6 位字母数字>` 的普通目录、其版本 <= 当前运行版本、
//! 且目录内只有普通文件（不跟随符号链接/联接点，不碰其他产品或更新版本的目录）。

use std::path::{Path, PathBuf};

use semver::Version;
use tauri::{AppHandle, Runtime};

/// `tempfile`'s default number of random characters (ASCII alphanumerics).
const RANDOM_SUFFIX_LEN: usize = 6;
const MARKER: &str = "-updater-";

/// The version in a plugin temp directory name for `product`, or `None` unless
/// the name is exactly `<product>-<semver>-updater-<6 alphanumerics>`.
fn leftover_version(name: &str, product: &str) -> Option<Version> {
    let rest = name.strip_prefix(product)?.strip_prefix('-')?;
    let (version, suffix) = rest.rsplit_once(MARKER)?;
    if suffix.len() != RANDOM_SUFFIX_LEN || !suffix.bytes().all(|byte| byte.is_ascii_alphanumeric())
    {
        return None;
    }
    Version::parse(version).ok()
}

/// True when `dir` holds only regular files (what the plugin writes there).
fn only_regular_files(dir: &Path) -> std::io::Result<bool> {
    for entry in std::fs::read_dir(dir)? {
        if !entry?.file_type()?.is_file() {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Outcome of one sweep, for logging.
#[derive(Debug, Default)]
struct SweepReport {
    removed: Vec<PathBuf>,
    kept: Vec<(PathBuf, String)>,
}

/// Removes this product's leftover updater directories under `temp_dir` whose
/// version is at most `running`.
fn sweep(temp_dir: &Path, product: &str, running: &Version) -> SweepReport {
    let mut report = SweepReport::default();
    let Ok(entries) = std::fs::read_dir(temp_dir) else {
        return report;
    };
    for entry in entries.flatten() {
        let file_name = entry.file_name();
        let Some(version) = file_name
            .to_str()
            .and_then(|name| leftover_version(name, product))
        else {
            continue;
        };
        let path = entry.path();
        if version > *running {
            report
                .kept
                .push((path, format!("version {version} is newer than {running}")));
            continue;
        }
        // DirEntry::file_type 不跟随符号链接：联接点/符号链接不是 is_dir，直接跳过。
        if !entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            report.kept.push((path, "not a plain directory".to_owned()));
            continue;
        }
        match only_regular_files(&path) {
            Ok(true) => {}
            Ok(false) => {
                report
                    .kept
                    .push((path, "holds something other than files".to_owned()));
                continue;
            }
            Err(error) => {
                report.kept.push((path, error.to_string()));
                continue;
            }
        }
        match std::fs::remove_dir_all(&path) {
            Ok(()) => report.removed.push(path),
            // 安装器仍在退出时安装包被占用；留到下一次（关闭屏障或下次启动）再删。
            Err(error) => report.kept.push((path, error.to_string())),
        }
    }
    report
}

/// Sweeps `%TEMP%` for this product's leftover updater directories.
pub(crate) async fn sweep_leftover_installers<R: Runtime>(app: &AppHandle<R>, when: &'static str) {
    let product = app.package_info().name.clone();
    let running = app.package_info().version.clone();
    let temp_dir = std::env::temp_dir();
    let task = tauri::async_runtime::spawn_blocking(move || sweep(&temp_dir, &product, &running));
    match task.await {
        Ok(report) => {
            for path in &report.removed {
                tracing::info!(when, path = %path.display(), "removed a leftover updater temp directory");
            }
            for (path, reason) in &report.kept {
                tracing::info!(when, path = %path.display(), reason = %reason, "kept an updater temp directory");
            }
        }
        Err(error) => tracing::warn!(when, %error, "updater temp sweep failed"),
    }
}
