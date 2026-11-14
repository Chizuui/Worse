//! Phase 7 kill switch (spec `docs/specs/harness/phase-7-parity.md` §7).
//!
//! When engaged, Harness does not start: no window, no Host, no write to the
//! shared `~/.zcode` data or to Harness's own data. It hands the user to `ZCode`
//! instead. Nothing is deleted; removing the switch restores Harness as it was.
//!
//! Engaged by either:
//! - `HARNESS_KILL_SWITCH` set to `1` / `true` / `on`, or
//! - the marker file `<home>/.zcode/harness/kill-switch` (home resolved like the
//!   Host's settings home: `ZCODE_DESKTOP_HOME_DIR`, `HOME`, `USERPROFILE`).
//!
//! `ZCode` is located from `HARNESS_ZCODE_EXE`, else the first non-empty line of
//! the marker file, else the per-user `ZCode` install location of the platform.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const ENV_SWITCH: &str = "HARNESS_KILL_SWITCH";
const ENV_ZCODE_EXE: &str = "HARNESS_ZCODE_EXE";

/// What engaged the switch and where `ZCode` is expected.
pub(crate) struct KillSwitch {
    reason: String,
    zcode: Option<PathBuf>,
}

fn env_value(key: &str) -> Option<String> {
    let value = std::env::var(key).ok()?;
    let trimmed = value.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_owned())
}

fn marker_path() -> Option<PathBuf> {
    let home = env_value("ZCODE_DESKTOP_HOME_DIR")
        .or_else(|| env_value("HOME"))
        .or_else(|| env_value("USERPROFILE"))?;
    Some(
        PathBuf::from(home)
            .join(".zcode")
            .join("harness")
            .join("kill-switch"),
    )
}

/// Per-user `ZCode` install location (electron-builder NSIS per-user default on Windows).
fn default_zcode() -> Option<PathBuf> {
    if cfg!(windows) {
        env_value("LOCALAPPDATA").map(|local| {
            PathBuf::from(local)
                .join("Programs")
                .join("ZCode")
                .join("ZCode.exe")
        })
    } else if cfg!(target_os = "macos") {
        Some(PathBuf::from("/Applications/ZCode.app"))
    } else {
        None
    }
}

/// Returns the engaged switch, or `None` when Harness should start normally.
///
/// Runs before the Tauri runtime exists; the marker is read through Tauri's
/// async runtime so no blocking file IO happens on a worker.
pub(crate) fn engaged() -> Option<KillSwitch> {
    let from_env = env_value(ENV_SWITCH)
        .filter(|value| matches!(value.to_ascii_lowercase().as_str(), "1" | "true" | "on"));
    let marker = marker_path();
    let marker_text = marker.as_ref().and_then(|path| {
        let path = path.clone();
        tauri::async_runtime::block_on(async move { tokio::fs::read_to_string(&path).await.ok() })
    });
    let reason = match (&from_env, &marker_text, &marker) {
        (Some(_), _, _) => format!("{ENV_SWITCH} is set"),
        (None, Some(_), Some(path)) => format!("marker {} exists", path.display()),
        _ => return None,
    };
    let from_marker = marker_text.as_deref().and_then(|text| {
        text.lines()
            .map(str::trim)
            .find(|line| !line.is_empty())
            .map(PathBuf::from)
    });
    let zcode = env_value(ENV_ZCODE_EXE)
        .map(PathBuf::from)
        .or(from_marker)
        .or_else(default_zcode);
    Some(KillSwitch { reason, zcode })
}

fn launch(zcode: &Path) -> std::io::Result<u32> {
    let mut command = if cfg!(target_os = "macos")
        && zcode
            .extension()
            .is_some_and(|extension| extension == "app")
    {
        let mut open = Command::new("open");
        open.arg("-a").arg(zcode);
        open
    } else {
        Command::new(zcode)
    };
    // ZCode 独立运行：不继承 Harness 的标准流，Harness 随即退出，不等待子进程。
    let child = command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    Ok(child.id())
}

impl KillSwitch {
    /// Hands the user to `ZCode` and reports the outcome on stderr.
    /// The caller returns from `run()` afterwards, so Harness exits with no side effects.
    #[allow(clippy::print_stderr)]
    pub(crate) fn route_to_zcode(&self) {
        let outcome = match self.zcode.as_deref() {
            Some(zcode) if zcode.exists() => match launch(zcode) {
                Ok(pid) => format!("launched ZCode {} (pid {pid})", zcode.display()),
                Err(error) => format!("could not launch ZCode {}: {error}", zcode.display()),
            },
            Some(zcode) => format!(
                "ZCode was not found at {}; set {ENV_ZCODE_EXE} or put its path in the marker file",
                zcode.display()
            ),
            None => format!("no ZCode location is known; set {ENV_ZCODE_EXE}"),
        };
        eprintln!(
            "HARNESS_KILL_SWITCH engaged ({}): Harness did not start and changed no data; {outcome}",
            self.reason
        );
    }
}
