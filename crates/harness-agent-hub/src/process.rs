//! Spawning and killing agent child processes.

use std::path::Path;
use std::process::Stdio;

use tokio::process::{Child, Command};

/// `CREATE_NO_WINDOW`: console programs started by a GUI app must not flash a console window.
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Spawns a child with piped stdio, no console window, killed when dropped.
pub(crate) fn spawn_piped(
    program: &Path,
    args: &[String],
    env: &[(String, String)],
    cwd: Option<&Path>,
) -> std::io::Result<Child> {
    let mut command = Command::new(program);
    command
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    for (key, value) in env {
        command.env(key, value);
    }
    if let Some(cwd) = cwd {
        command.current_dir(cwd);
    }
    #[cfg(windows)]
    command.creation_flags(CREATE_NO_WINDOW);
    #[cfg(unix)]
    command.process_group(0);
    command.spawn()
}

/// Kills the process tree rooted at `pid` (our own child only).
pub(crate) async fn kill_tree(pid: u32) {
    #[cfg(windows)]
    {
        let mut command = Command::new("taskkill");
        command
            .args(["/T", "/F", "/PID", &pid.to_string()])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .creation_flags(CREATE_NO_WINDOW);
        if let Err(error) = command.status().await {
            tracing::warn!(%pid, %error, "taskkill failed");
        }
    }
    #[cfg(unix)]
    {
        let group = format!("-{pid}");
        let result = Command::new("kill")
            .args(["-s", "KILL", "--", &group])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await;
        if let Err(error) = result {
            tracing::warn!(%pid, %error, "kill failed");
        }
    }
}
