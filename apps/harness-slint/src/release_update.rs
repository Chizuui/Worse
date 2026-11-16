//! Release update: checks the newest GitHub release, downloads the build for this platform in the background,
//! replaces the installed executable and raises `update-ready` so the UI offers a restart.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

const LATEST_URL: &str = "https://api.github.com/repos/rraahh133/Worse/releases/latest";
const NO_UPDATE_ENV: &str = "HARNESS_NO_UPDATE";
const FIRST_CHECK: Duration = Duration::from_secs(20);
const CHECK_EVERY: Duration = Duration::from_secs(3 * 60 * 60);

static INSTALLED: Mutex<Option<PathBuf>> = Mutex::new(None);

struct Package {
    asset: &'static str,
    member: &'static str,
}

fn package() -> Option<Package> {
    if cfg!(all(windows, target_arch = "x86_64")) {
        Some(Package {
            asset: "Worse-Harness-windows-x64.zip",
            member: "Worse-Harness.exe",
        })
    } else if cfg!(target_os = "macos") {
        Some(Package {
            asset: "Worse-Harness-macos-universal.zip",
            member: "Harness.app/Contents/MacOS/harness-slint",
        })
    } else if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        Some(Package {
            asset: "Worse-Harness-linux-x64.tar.gz",
            member: "Worse-Harness-linux-x64/worse-harness",
        })
    } else {
        None
    }
}

#[must_use]
pub fn installed_exe() -> Option<PathBuf> {
    INSTALLED.lock().ok().and_then(|slot| slot.clone())
}

fn parse_version(text: &str) -> Option<Vec<u64>> {
    let core = text.trim().trim_start_matches(['v', 'V']);
    let core = core.split(['-', '+']).next()?;
    let parts: Option<Vec<u64>> = core.split('.').map(|p| p.parse().ok()).collect();
    parts.filter(|p| !p.is_empty())
}

fn is_newer(tag: &str, current: &str) -> bool {
    match (parse_version(tag), parse_version(current)) {
        (Some(tag), Some(current)) => tag > current,
        _ => false,
    }
}

fn is_dev_build(exe: &Path) -> bool {
    exe.components()
        .any(|part| part.as_os_str().eq_ignore_ascii_case("target"))
}

fn marker() -> PathBuf {
    crate::harness_home()
        .join("bin")
        .join("release-installed.txt")
}

pub fn watch(window: slint::Weak<crate::ui::AppWindow>) {
    if std::env::var_os(NO_UPDATE_ENV).is_some_and(|v| !v.is_empty())
        || std::env::var_os("HARNESS_UI_CORE").is_some_and(|v| !v.is_empty())
        || cfg!(feature = "automation")
        || (cfg!(windows) && !crate::live_update::is_shadow())
    {
        return;
    }
    let Some(package) = package() else {
        return;
    };
    let target = crate::live_update::launch_exe();
    if is_dev_build(&target) {
        return;
    }
    std::thread::Builder::new()
        .name("harness-release-update".into())
        .spawn(move || {
            let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            else {
                return;
            };
            std::thread::sleep(FIRST_CHECK);
            loop {
                match runtime.block_on(check(&package, &target)) {
                    Ok(true) => {
                        let weak = window.clone();
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(window) = weak.upgrade() {
                                window.set_update_ready(true);
                            }
                        });
                        return;
                    }
                    Ok(false) => {}
                    Err(error) => eprintln!("harness-slint: release update: {error}"),
                }
                std::thread::sleep(CHECK_EVERY);
            }
        })
        .ok();
}

async fn check(package: &Package, target: &Path) -> Result<bool, String> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(15))
        .timeout(Duration::from_secs(600))
        .user_agent(concat!("harness/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|e| format!("http client: {e}"))?;
    let release: serde_json::Value = client
        .get(LATEST_URL)
        .header("Accept", "application/vnd.github+json")
        .send()
        .await
        .and_then(reqwest::Response::error_for_status)
        .map_err(|e| format!("latest release: {e}"))?
        .json()
        .await
        .map_err(|e| format!("latest release: {e}"))?;
    let tag = release
        .get("tag_name")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_owned();
    if !is_newer(&tag, env!("CARGO_PKG_VERSION")) {
        return Ok(false);
    }
    if std::fs::read_to_string(marker()).is_ok_and(|done| done.trim() == tag) {
        remember(target);
        return Ok(true);
    }
    let Some(url) = asset_url(&release, package.asset) else {
        return Ok(false);
    };
    let bytes = client
        .get(url)
        .send()
        .await
        .and_then(reqwest::Response::error_for_status)
        .map_err(|e| format!("download: {e}"))?
        .bytes()
        .await
        .map_err(|e| format!("download: {e}"))?;
    let target = target.to_path_buf();
    let member = package.member;
    let asset = package.asset;
    tokio::task::spawn_blocking(move || install(&bytes, asset, member, &target, &tag))
        .await
        .map_err(|e| format!("install: {e}"))??;
    Ok(true)
}

fn asset_url<'a>(release: &'a serde_json::Value, name: &str) -> Option<&'a str> {
    release
        .get("assets")?
        .as_array()?
        .iter()
        .find(|asset| asset.get("name").and_then(serde_json::Value::as_str) == Some(name))?
        .get("browser_download_url")?
        .as_str()
}

fn remember(target: &Path) {
    if let Ok(mut slot) = INSTALLED.lock() {
        *slot = Some(target.to_path_buf());
    }
}

fn install(
    bytes: &[u8],
    asset: &str,
    member: &str,
    target: &Path,
    tag: &str,
) -> Result<(), String> {
    let work = crate::harness_home().join("bin").join("release-download");
    let _ = std::fs::remove_dir_all(&work);
    std::fs::create_dir_all(&work).map_err(|e| format!("{}: {e}", work.display()))?;
    let archive = work.join(asset);
    std::fs::write(&archive, bytes).map_err(|e| format!("{}: {e}", archive.display()))?;
    let mut tar = std::process::Command::new("tar");
    tar.arg("-xf").arg(&archive).arg("-C").arg(&work);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        tar.creation_flags(0x0800_0000);
    }
    let status = tar.status().map_err(|e| format!("tar: {e}"))?;
    if !status.success() {
        return Err(format!("tar failed: {status}"));
    }
    let fresh = work.join(member);
    if !fresh.is_file() {
        return Err(format!("{member} missing in {asset}"));
    }
    let staged = target.with_extension("update");
    std::fs::copy(&fresh, &staged).map_err(|e| format!("{}: {e}", staged.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o755));
    }
    if let Err(error) = std::fs::rename(&staged, target) {
        let _ = std::fs::remove_file(&staged);
        return Err(format!("{}: {error}", target.display()));
    }
    let _ = std::fs::remove_dir_all(&work);
    let _ = std::fs::write(marker(), tag);
    remember(target);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{asset_url, is_dev_build, is_newer};
    use std::path::Path;

    #[test]
    fn newer_tags_only() {
        assert!(is_newer("v0.1.3", "0.1.2"));
        assert!(is_newer("v0.2.0", "0.1.9"));
        assert!(is_newer("v0.1.10", "0.1.9"));
        assert!(!is_newer("v0.1.2", "0.1.2"));
        assert!(!is_newer("v0.1.1", "0.1.2"));
        assert!(!is_newer("nightly", "0.1.2"));
    }

    #[test]
    fn dev_builds_are_skipped() {
        assert!(is_dev_build(Path::new(
            "C:/code/Worse/target/release/harness-slint.exe"
        )));
        assert!(!is_dev_build(Path::new("C:/Apps/Worse-Harness.exe")));
    }

    #[test]
    fn finds_the_platform_asset() {
        let release = serde_json::json!({"assets": [
            {"name": "a.zip", "browser_download_url": "https://x/a.zip"},
            {"name": "b.zip", "browser_download_url": "https://x/b.zip"},
        ]});
        assert_eq!(asset_url(&release, "b.zip"), Some("https://x/b.zip"));
        assert_eq!(asset_url(&release, "c.zip"), None);
    }
}
