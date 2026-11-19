//! Opt-in live check: builds a `reach eval` command line, never starts it.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::types::AgentKind;

/// Opt-in live check (`reach eval`). Harness never starts it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct LiveCheck {
    pub enabled: bool,
    /// `reach` binary; resolved on `PATH` when relative.
    pub binary: String,
    /// Arguments of the command.
    pub command: Vec<String>,
    /// Where the JSON artifact goes.
    pub out: Option<PathBuf>,
}

impl Default for LiveCheck {
    fn default() -> Self {
        Self {
            enabled: false,
            binary: "reach".into(),
            command: Vec::new(),
            out: None,
        }
    }
}

/// skill-reach's agent id for a Harness agent.
#[must_use]
pub fn reach_agent(agent: AgentKind) -> Option<&'static str> {
    match agent {
        AgentKind::ClaudeCode => Some("claude-code"),
        AgentKind::Agy => Some("antigravity-cli"),
        AgentKind::Codex | AgentKind::Harness => None,
    }
}

impl LiveCheck {
    /// Builds the command for one skill (`Some`) or the whole catalog in `skills_dir`; `None` while disabled.
    #[must_use]
    pub fn build(
        enabled: bool,
        binary: &str,
        agent: AgentKind,
        skills_dir: &Path,
        skill: Option<&str>,
    ) -> Option<Self> {
        if !enabled {
            return None;
        }
        let cli = reach_agent(agent)?;
        let mut command = match skill {
            Some(name) => vec![
                "eval".to_owned(),
                name.to_owned(),
                "--skills".to_owned(),
                skills_dir.display().to_string(),
            ],
            None => vec![
                "eval".to_owned(),
                skills_dir.display().to_string(),
                "--auto".to_owned(),
            ],
        };
        command.extend([
            "--agent".to_owned(),
            cli.to_owned(),
            "--yes".to_owned(),
            "--format".to_owned(),
            "json".to_owned(),
        ]);
        Some(Self {
            enabled,
            binary: binary.to_owned(),
            command,
            out: None,
        })
    }

    /// Command line for display (`binary` followed by the arguments).
    #[must_use]
    pub fn to_command_line(&self) -> String {
        std::iter::once(&self.binary)
            .chain(self.command.iter())
            .map(|s| {
                if s.contains(' ') {
                    format!("\u{201c}{s}\u{201d}")
                } else {
                    s.clone()
                }
            })
            .collect::<Vec<_>>()
            .join(" ")
    }
}
