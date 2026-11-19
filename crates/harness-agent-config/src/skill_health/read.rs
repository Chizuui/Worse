//! Reads the installed skills of the CLI skill dirs and hands them to the static check.

use std::path::Path;

use crate::clis::{ClaudeCodeConfig, CodexConfig, HarnessOverlayConfig, SkillsConfig};
use crate::error::{ConfigError, Result};
use crate::paths::ConfigPaths;
use crate::skill_health::rules::{check, InstalledSkill, Report};
use crate::types::{AgentKind, Scope, Skill};

/// Health of the skills installed in `dir`; `client` is the CLI id shown on the row.
///
/// # Errors
///
/// I/O while reading the skill directory.
pub async fn report_for_dir(dir: &Path, client: &str) -> Result<Report> {
    let skills = crate::md_store::list_skills(dir).await?;
    Ok(check(&listed(&skills, client)))
}

/// Health across `agents` at `scope`. An agent without this concept or scope is skipped.
///
/// # Errors
///
/// I/O while reading any skill directory.
pub async fn report_for_scope(
    paths: &ConfigPaths,
    agents: &[AgentKind],
    scope: Scope,
) -> Result<Report> {
    let mut out = Vec::new();
    for agent in agents {
        if let Some(skills) = read_agent(paths, *agent, scope).await? {
            out.extend(listed(&skills, agent.agent_id()));
        }
    }
    Ok(check(&out))
}

async fn read_agent(
    paths: &ConfigPaths,
    agent: AgentKind,
    scope: Scope,
) -> Result<Option<Vec<(Skill, std::path::PathBuf)>>> {
    let read = match agent {
        AgentKind::ClaudeCode => ClaudeCodeConfig { paths }.list_skills(scope).await,
        AgentKind::Codex => CodexConfig { paths }.list_skills(scope).await,
        AgentKind::Harness => HarnessOverlayConfig { paths }.list_skills(scope).await,
        AgentKind::Agy => return Ok(None),
    };
    match read {
        Ok(skills) => Ok(Some(skills)),
        Err(ConfigError::Unsupported { .. }) => Ok(None),
        Err(e) => Err(e),
    }
}

/// Skills directory of `agent` at `scope`, where an opt-in live check would run (`None` when the agent has no
/// skills at that scope, e.g. agy).
#[must_use]
pub fn skills_dir(
    paths: &ConfigPaths,
    agent: AgentKind,
    scope: Scope,
) -> Option<std::path::PathBuf> {
    let base = match (agent, scope) {
        (AgentKind::ClaudeCode, Scope::User) => paths.claude_dir.clone(),
        (AgentKind::ClaudeCode, Scope::Workspace) => on_disk(paths, ".claude")?,
        (AgentKind::Codex, Scope::User) => paths.codex_home.clone(),
        (AgentKind::Codex, Scope::Workspace) => on_disk(paths, ".codex")?,
        (AgentKind::Harness, Scope::User) => paths.harness_dir.clone(),
        (AgentKind::Harness | AgentKind::Agy, Scope::Workspace) | (AgentKind::Agy, Scope::User) => {
            return None
        }
    };
    Some(base.join("skills"))
}

/// `<workspace>/<dir>`; `None` without a workspace.
fn on_disk(paths: &ConfigPaths, dir: &str) -> Option<std::path::PathBuf> {
    Some(paths.workspace().ok()?.join(dir))
}

fn listed(skills: &[(Skill, std::path::PathBuf)], client: &str) -> Vec<InstalledSkill> {
    skills
        .iter()
        .map(|(s, _)| InstalledSkill::new(s.name.clone(), client, s.description.clone()))
        .collect()
}
