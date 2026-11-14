//! Declaration of the sidecar process to supervise.

use std::path::PathBuf;
use std::time::Duration;

use harness_core::SidecarId;

/// Grace period used when a caller does not choose one.
pub const DEFAULT_SHUTDOWN_GRACE: Duration = Duration::from_secs(3);

/// Everything needed to start one supervised sidecar process.
#[derive(Debug, Clone)]
pub struct SidecarSpec {
    /// Stable identifier used to supervise and stop the process.
    pub id: SidecarId,
    /// Executable to launch.
    pub program: PathBuf,
    /// Arguments passed to the executable, one element per argument.
    pub args: Vec<String>,
    /// Working directory for the child process.
    pub cwd: Option<PathBuf>,
    /// Additional environment variables; the parent environment is inherited.
    pub env: Vec<(String, String)>,
    /// How long a graceful shutdown may take before the process tree is killed.
    pub shutdown_grace: Duration,
}

impl SidecarSpec {
    /// Creates a specification with [`DEFAULT_SHUTDOWN_GRACE`] and no arguments.
    #[must_use]
    pub fn new(id: impl Into<SidecarId>, program: impl Into<PathBuf>) -> Self {
        Self {
            id: id.into(),
            program: program.into(),
            args: Vec::new(),
            cwd: None,
            env: Vec::new(),
            shutdown_grace: DEFAULT_SHUTDOWN_GRACE,
        }
    }

    /// Replaces the argument list.
    #[must_use]
    pub fn with_args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.args = args.into_iter().map(Into::into).collect();
        self
    }

    /// Sets the working directory of the child process.
    #[must_use]
    pub fn with_cwd(mut self, cwd: impl Into<PathBuf>) -> Self {
        self.cwd = Some(cwd.into());
        self
    }

    /// Appends one environment variable for the child process.
    #[must_use]
    pub fn with_env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.env.push((key.into(), value.into()));
        self
    }

    /// Overrides the shutdown grace period.
    #[must_use]
    pub fn with_shutdown_grace(mut self, grace: Duration) -> Self {
        self.shutdown_grace = grace;
        self
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::time::Duration;

    use super::{SidecarSpec, DEFAULT_SHUTDOWN_GRACE};

    #[test]
    fn builders_populate_the_specification() {
        let spec = SidecarSpec::new("agent", "node")
            .with_args(["--version"])
            .with_cwd("/tmp")
            .with_env("HARNESS_MODE", "test")
            .with_shutdown_grace(Duration::from_secs(1));
        assert_eq!(spec.id.as_str(), "agent");
        assert_eq!(spec.program, PathBuf::from("node"));
        assert_eq!(spec.args, vec!["--version".to_owned()]);
        assert_eq!(spec.cwd.as_deref(), Some(Path::new("/tmp")));
        assert_eq!(
            spec.env,
            vec![("HARNESS_MODE".to_owned(), "test".to_owned())]
        );
        assert_eq!(spec.shutdown_grace, Duration::from_secs(1));
    }

    #[test]
    fn new_uses_the_default_grace_period() {
        let spec = SidecarSpec::new("agent", "node");
        assert_eq!(spec.shutdown_grace, DEFAULT_SHUTDOWN_GRACE);
        assert_eq!(spec.args.len(), 0);
        assert_eq!(spec.env.len(), 0);
        assert!(spec.cwd.is_none());
    }
}
