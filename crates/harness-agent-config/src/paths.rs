//! Where each CLI keeps its configuration. Injected so tests and gates use isolated fixtures.

use std::path::{Path, PathBuf};

use crate::error::{ConfigError, Result};

/// Config locations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigPaths {
    /// User home.
    pub home: PathBuf,
    /// Claude Code config dir (`CLAUDE_CONFIG_DIR` or `~/.claude`).
    pub claude_dir: PathBuf,
    /// Claude Code global state file holding user MCP servers (`~/.claude.json`, or inside `CLAUDE_CONFIG_DIR`).
    pub claude_json: PathBuf,
    /// Codex home (`CODEX_HOME` or `~/.codex`).
    pub codex_home: PathBuf,
    /// agy / Gemini dir (`~/.gemini`).
    pub gemini_dir: PathBuf,
    /// Harness data dir (overlay `mcp.json`, `agents.json`).
    pub harness_dir: PathBuf,
    /// Open workspace, if any.
    pub workspace: Option<PathBuf>,
}

impl ConfigPaths {
    /// Paths for `home` with the CLI defaults (no environment lookups).
    #[must_use]
    pub fn for_home(home: &Path, harness_dir: &Path, workspace: Option<PathBuf>) -> Self {
        Self {
            home: home.to_path_buf(),
            claude_dir: home.join(".claude"),
            claude_json: home.join(".claude.json"),
            codex_home: home.join(".codex"),
            gemini_dir: home.join(".gemini"),
            harness_dir: harness_dir.to_path_buf(),
            workspace,
        }
    }

    /// Paths from the environment: `CLAUDE_CONFIG_DIR`, `CODEX_HOME`, user home.
    ///
    /// # Errors
    ///
    /// No home directory.
    pub fn from_env(harness_dir: &Path, workspace: Option<PathBuf>) -> Result<Self> {
        let keys: &[&str] = if cfg!(windows) {
            &["USERPROFILE", "HOME"]
        } else {
            &["HOME"]
        };
        let home = keys
            .iter()
            .filter_map(std::env::var_os)
            .find(|v| !v.is_empty())
            .map(PathBuf::from)
            .ok_or_else(|| ConfigError::invalid("no home directory"))?;
        let mut paths = Self::for_home(&home, harness_dir, workspace);
        if let Some(dir) = env_path("CLAUDE_CONFIG_DIR") {
            paths.claude_json = dir.join(".claude.json");
            paths.claude_dir = dir;
        }
        if let Some(dir) = env_path("CODEX_HOME") {
            paths.codex_home = dir;
        }
        Ok(paths)
    }

    /// The workspace, or [`ConfigError::NoWorkspace`].
    ///
    /// # Errors
    ///
    /// No workspace set.
    pub fn workspace(&self) -> Result<&Path> {
        self.workspace.as_deref().ok_or(ConfigError::NoWorkspace)
    }
}

fn env_path(key: &str) -> Option<PathBuf> {
    std::env::var_os(key)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}
