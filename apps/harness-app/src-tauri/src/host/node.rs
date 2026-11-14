//! Locates the Node.js runtime that runs the Host sidecar.
//!
//! Order (Phase 6, `Worse/docs/specs/harness/phase-6-packaging.md` 2.3):
//! `HARNESS_NODE_BIN`, then the runtime bundled in the layout
//! (`host/runtime/node/node(.exe)`, staged by `stage-sidecar-resources.mjs`),
//! then `node` from `PATH` (dev checkouts have no bundled runtime). The chosen
//! runtime must satisfy the repository `engines` floor (Node 24; the Host uses
//! `node:sqlite`). The agent CLI inherits it through `process.execPath`.

use std::fmt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use harness_protocol::{CommandError, ErrorCode};

use super::paths::{absolute, env_path};

/// Minimum Node.js major version (`engines.node` in the root `package.json`).
pub(crate) const MIN_NODE_MAJOR: u32 = 24;
const VERSION_PROBE_TIMEOUT: Duration = Duration::from_secs(10);
#[cfg(windows)]
const NODE_EXECUTABLE: &str = "node.exe";
#[cfg(not(windows))]
const NODE_EXECUTABLE: &str = "node";

/// Where the Node.js runtime came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NodeSource {
    /// `HARNESS_NODE_BIN`.
    Explicit,
    /// `<layout root>/runtime/node` shipped with the app.
    Bundled,
    /// `node` found on `PATH`.
    Path,
}

/// A verified Node.js runtime.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NodeRuntime {
    pub(crate) program: PathBuf,
    pub(crate) version: String,
    pub(crate) source: NodeSource,
}

/// Why no usable Node.js runtime was found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum NodeRuntimeError {
    /// `HARNESS_NODE_BIN` is unset and no `node` executable is on `PATH`.
    NotFound,
    /// `HARNESS_NODE_BIN` does not name a file.
    MissingExplicit(PathBuf),
    /// The executable could not report its version.
    Unusable { program: PathBuf, reason: String },
    /// The runtime is older than [`MIN_NODE_MAJOR`].
    TooOld { program: PathBuf, version: String },
}

impl fmt::Display for NodeRuntimeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound => write!(
                f,
                "Node.js {MIN_NODE_MAJOR}+ is required to run the local Host, but `{NODE_EXECUTABLE}` \
                 was not found on PATH; install Node.js or set HARNESS_NODE_BIN"
            ),
            Self::MissingExplicit(path) => {
                write!(f, "HARNESS_NODE_BIN points to a missing file: {}", path.display())
            }
            Self::Unusable { program, reason } => {
                write!(f, "{} could not report its version: {reason}", program.display())
            }
            Self::TooOld { program, version } => write!(
                f,
                "{} is Node.js {version}; the local Host requires Node.js {MIN_NODE_MAJOR}+",
                program.display()
            ),
        }
    }
}

impl From<NodeRuntimeError> for CommandError {
    fn from(error: NodeRuntimeError) -> Self {
        crate::error::host(ErrorCode::HostSpawnFailed, error.to_string())
    }
}

/// Finds and verifies the Node.js runtime. `bundled` is the layout's
/// `runtime/node` executable; `packaged` marks a release layout, where a
/// missing bundled runtime is a defect worth a warning before falling back.
pub(crate) async fn locate(
    bundled: &Path,
    packaged: bool,
) -> Result<NodeRuntime, NodeRuntimeError> {
    let (program, source) = select(
        env_path("HARNESS_NODE_BIN"),
        bundled,
        std::env::var_os("PATH"),
    )
    .await?;
    if packaged && source == NodeSource::Path {
        tracing::warn!(
            bundled = %bundled.display(),
            "bundled Node.js runtime missing from the packaged layout; falling back to PATH"
        );
    }
    let version = probe_version(&program).await?;
    match parse_major(&version) {
        Some(major) if major >= MIN_NODE_MAJOR => Ok(NodeRuntime {
            program,
            version,
            source,
        }),
        _ => Err(NodeRuntimeError::TooOld { program, version }),
    }
}

/// Pure candidate order, separated for tests: explicit, bundled, `PATH`.
async fn select(
    explicit: Option<PathBuf>,
    bundled: &Path,
    path: Option<std::ffi::OsString>,
) -> Result<(PathBuf, NodeSource), NodeRuntimeError> {
    if let Some(explicit) = explicit {
        let explicit = absolute(&explicit);
        if !is_file(&explicit).await {
            return Err(NodeRuntimeError::MissingExplicit(explicit));
        }
        return Ok((explicit, NodeSource::Explicit));
    }
    if is_file(bundled).await {
        return Ok((bundled.to_path_buf(), NodeSource::Bundled));
    }
    search_path(path)
        .await
        .map(|program| (program, NodeSource::Path))
        .ok_or(NodeRuntimeError::NotFound)
}

async fn search_path(path: Option<std::ffi::OsString>) -> Option<PathBuf> {
    for directory in std::env::split_paths(&path?) {
        let candidate = directory.join(NODE_EXECUTABLE);
        if is_file(&candidate).await {
            return Some(candidate);
        }
    }
    None
}

async fn is_file(path: &Path) -> bool {
    tokio::fs::metadata(path)
        .await
        .is_ok_and(|metadata| metadata.is_file())
}

async fn probe_version(program: &Path) -> Result<String, NodeRuntimeError> {
    let unusable = |reason: String| NodeRuntimeError::Unusable {
        program: program.to_path_buf(),
        reason,
    };
    let mut command = tokio::process::Command::new(program);
    command
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    // GUI 进程启动控制台程序时避免闪出控制台窗口（与 sidecar 启动一致）。
    #[cfg(windows)]
    command.creation_flags(0x0800_0000);
    let output = tokio::time::timeout(VERSION_PROBE_TIMEOUT, command.output())
        .await
        .map_err(|_elapsed| unusable("timed out".to_owned()))?
        .map_err(|error| unusable(error.to_string()))?;
    if !output.status.success() {
        return Err(unusable(format!("exited with {}", output.status)));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

/// Parses the major version of `v24.11.1`.
fn parse_major(version: &str) -> Option<u32> {
    version
        .trim()
        .trim_start_matches('v')
        .split('.')
        .next()?
        .parse()
        .ok()
}

#[cfg(test)]
mod tests {
    use super::{parse_major, search_path, select, NodeRuntimeError, NodeSource, NODE_EXECUTABLE};

    #[test]
    fn parses_node_versions() {
        assert_eq!(parse_major("v24.11.1\n"), Some(24));
        assert_eq!(parse_major("v22.13.0"), Some(22));
        assert_eq!(parse_major("garbage"), None);
    }

    #[tokio::test]
    async fn path_search_skips_missing_directories() {
        let root = std::env::temp_dir().join(format!("harness-node-{}", std::process::id()));
        let bin = root.join("bin");
        tokio::fs::create_dir_all(&bin).await.ok();
        tokio::fs::write(bin.join(NODE_EXECUTABLE), b"").await.ok();
        let path = std::env::join_paths([root.join("missing"), bin.clone()]).ok();
        assert_eq!(search_path(path).await, Some(bin.join(NODE_EXECUTABLE)));
        assert_eq!(search_path(None).await, None);
        tokio::fs::remove_dir_all(&root).await.ok();
    }

    #[tokio::test]
    async fn candidates_are_explicit_then_bundled_then_path() {
        let root = std::env::temp_dir().join(format!("harness-node-order-{}", std::process::id()));
        let bundled_dir = root.join("runtime").join("node");
        let path_dir = root.join("path");
        tokio::fs::create_dir_all(&bundled_dir).await.ok();
        tokio::fs::create_dir_all(&path_dir).await.ok();
        let bundled = bundled_dir.join(NODE_EXECUTABLE);
        let on_path = path_dir.join(NODE_EXECUTABLE);
        let explicit = root.join("explicit-node");
        tokio::fs::write(&on_path, b"").await.ok();
        tokio::fs::write(&explicit, b"").await.ok();
        let path = || std::env::join_paths([path_dir.clone()]).ok();

        // No bundled runtime (dev checkout): PATH.
        assert_eq!(
            select(None, &bundled, path()).await,
            Ok((on_path.clone(), NodeSource::Path))
        );
        // Bundled runtime present: it wins over PATH.
        tokio::fs::write(&bundled, b"").await.ok();
        assert_eq!(
            select(None, &bundled, path()).await,
            Ok((bundled.clone(), NodeSource::Bundled))
        );
        // HARNESS_NODE_BIN wins over both; a missing explicit file is an error.
        assert_eq!(
            select(Some(explicit.clone()), &bundled, path()).await,
            Ok((explicit.clone(), NodeSource::Explicit))
        );
        assert_eq!(
            select(Some(root.join("missing")), &bundled, path()).await,
            Err(NodeRuntimeError::MissingExplicit(root.join("missing")))
        );
        // Nothing anywhere: NotFound.
        assert_eq!(
            select(None, &root.join("none"), None).await,
            Err(NodeRuntimeError::NotFound)
        );
        tokio::fs::remove_dir_all(&root).await.ok();
    }

    #[test]
    fn errors_name_the_remedy() {
        assert!(NodeRuntimeError::NotFound
            .to_string()
            .contains("HARNESS_NODE_BIN"));
    }
}
