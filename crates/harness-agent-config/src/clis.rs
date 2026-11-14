//! One trait per concept, implemented per CLI. A concept or scope a CLI does not have returns
//! [`ConfigError::Unsupported`] (the trait default).

#![allow(async_fn_in_trait)]

use std::path::{Path, PathBuf};

use crate::error::{ConfigError, Result};
use crate::hooks;
use crate::mcp::{self, Dialect};
use crate::md_store;
use crate::paths::ConfigPaths;
use crate::plan::WritePlan;
use crate::types::{
    AgentKind, Command, Concept, ConceptSupport, Hook, InstructionFile, McpServer, McpTransport,
    Scope, Skill, Subagent,
};

/// Common base of the concept traits.
pub trait CliConfig {
    fn kind(&self) -> AgentKind;

    /// Error for a concept/scope this CLI lacks.
    fn unsupported(&self, concept: Concept, scope: Scope) -> ConfigError {
        let reason = support_of(self.kind(), concept)
            .reason
            .unwrap_or_else(|| "not available for this scope".into());
        ConfigError::unsupported(self.kind(), concept, scope, reason)
    }
}

/// MCP servers.
pub trait McpConfig: CliConfig {
    async fn list_mcp(&self, scope: Scope) -> Result<Vec<(McpServer, PathBuf)>>;
    async fn plan_mcp_upsert(
        &self,
        scope: Scope,
        server: &McpServer,
        previous_name: Option<&str>,
    ) -> Result<WritePlan>;
    async fn plan_mcp_delete(&self, scope: Scope, name: &str) -> Result<WritePlan>;
    async fn plan_mcp_enabled(&self, scope: Scope, name: &str, enabled: bool) -> Result<WritePlan>;
}

/// Skills.
pub trait SkillsConfig: CliConfig {
    async fn list_skills(&self, scope: Scope) -> Result<Vec<(Skill, PathBuf)>> {
        Err(self.unsupported(Concept::Skill, scope))
    }
    async fn plan_skill_upsert(
        &self,
        scope: Scope,
        _skill: &Skill,
        _previous_name: Option<&str>,
    ) -> Result<WritePlan> {
        Err(self.unsupported(Concept::Skill, scope))
    }
    async fn plan_skill_delete(&self, scope: Scope, _name: &str) -> Result<WritePlan> {
        Err(self.unsupported(Concept::Skill, scope))
    }
}

/// Slash commands / custom prompts.
pub trait CommandsConfig: CliConfig {
    async fn list_commands(&self, scope: Scope) -> Result<Vec<(Command, PathBuf)>> {
        Err(self.unsupported(Concept::Command, scope))
    }
    async fn plan_command_upsert(
        &self,
        scope: Scope,
        _command: &Command,
        _previous_name: Option<&str>,
    ) -> Result<WritePlan> {
        Err(self.unsupported(Concept::Command, scope))
    }
    async fn plan_command_delete(&self, scope: Scope, _name: &str) -> Result<WritePlan> {
        Err(self.unsupported(Concept::Command, scope))
    }
}

/// Hooks.
pub trait HooksConfig: CliConfig {
    async fn list_hooks(&self, scope: Scope) -> Result<Vec<(Hook, PathBuf)>> {
        Err(self.unsupported(Concept::Hook, scope))
    }
    async fn plan_hook_upsert(
        &self,
        scope: Scope,
        _hook: &Hook,
        _previous_id: Option<&str>,
    ) -> Result<WritePlan> {
        Err(self.unsupported(Concept::Hook, scope))
    }
    async fn plan_hook_delete(&self, scope: Scope, _id: &str) -> Result<WritePlan> {
        Err(self.unsupported(Concept::Hook, scope))
    }
}

/// Subagents.
pub trait SubagentsConfig: CliConfig {
    async fn list_subagents(&self, scope: Scope) -> Result<Vec<(Subagent, PathBuf)>> {
        Err(self.unsupported(Concept::Subagent, scope))
    }
    async fn plan_subagent_upsert(
        &self,
        scope: Scope,
        _agent: &Subagent,
        _previous_name: Option<&str>,
    ) -> Result<WritePlan> {
        Err(self.unsupported(Concept::Subagent, scope))
    }
    async fn plan_subagent_delete(&self, scope: Scope, _name: &str) -> Result<WritePlan> {
        Err(self.unsupported(Concept::Subagent, scope))
    }
}

/// Instructions files (CLAUDE.md / AGENTS.md / GEMINI.md): the fixed candidate files the user picks from.
pub trait InstructionsConfig: CliConfig {
    fn instruction_paths(&self, scope: Scope) -> Result<Vec<PathBuf>> {
        Err(self.unsupported(Concept::Instructions, scope))
    }
}

/// Lists the instruction candidates with existence and size (metadata only).
pub async fn instruction_files(
    cli: &impl InstructionsConfig,
    scope: Scope,
) -> Result<Vec<InstructionFile>> {
    let mut out = Vec::new();
    for path in cli.instruction_paths(scope)? {
        let meta = tokio::fs::metadata(&path).await.ok();
        out.push(InstructionFile {
            exists: meta.as_ref().is_some_and(std::fs::Metadata::is_file),
            size: meta.map_or(0, |m| m.len()),
            path,
            scope,
        });
    }
    Ok(out)
}

fn single(change: crate::plan::FileChange) -> WritePlan {
    WritePlan::single(change)
}

fn tag(items: Vec<McpServer>, path: &Path) -> Vec<(McpServer, PathBuf)> {
    items.into_iter().map(|s| (s, path.to_path_buf())).collect()
}

fn tag_hooks(items: Vec<Hook>, path: &Path) -> Vec<(Hook, PathBuf)> {
    items.into_iter().map(|h| (h, path.to_path_buf())).collect()
}

// ================================================================================================ Claude Code

/// Claude Code (`~/.claude`, `~/.claude.json`, `<ws>/.claude`, `<ws>/.mcp.json`).
pub struct ClaudeCodeConfig<'a> {
    pub paths: &'a ConfigPaths,
}

impl ClaudeCodeConfig<'_> {
    fn base(&self, scope: Scope) -> Result<PathBuf> {
        Ok(match scope {
            Scope::User => self.paths.claude_dir.clone(),
            Scope::Workspace => self.paths.workspace()?.join(".claude"),
        })
    }

    fn mcp_file(&self, scope: Scope) -> Result<PathBuf> {
        Ok(match scope {
            Scope::User => self.paths.claude_json.clone(),
            Scope::Workspace => self.paths.workspace()?.join(".mcp.json"),
        })
    }

    pub(crate) fn settings_file(&self, scope: Scope) -> Result<PathBuf> {
        Ok(self.base(scope)?.join("settings.json"))
    }

    pub(crate) fn settings_local_file(&self) -> Result<PathBuf> {
        Ok(self
            .paths
            .workspace()?
            .join(".claude")
            .join("settings.local.json"))
    }

    pub(crate) fn dirs(&self, scope: Scope) -> Result<Vec<PathBuf>> {
        let base = self.base(scope)?;
        Ok(vec![
            base.join("skills"),
            base.join("commands"),
            base.join("agents"),
        ])
    }
}

impl CliConfig for ClaudeCodeConfig<'_> {
    fn kind(&self) -> AgentKind {
        AgentKind::ClaudeCode
    }
}

impl McpConfig for ClaudeCodeConfig<'_> {
    async fn list_mcp(&self, scope: Scope) -> Result<Vec<(McpServer, PathBuf)>> {
        let path = self.mcp_file(scope)?;
        let mut servers = mcp::list_json(&path, Dialect::Claude).await?;
        if scope == Scope::Workspace {
            let approved = mcp::claude_project_enabled(&self.settings_local_file()?).await?;
            for s in &mut servers {
                s.enabled = Some(approved(&s.name));
            }
        }
        Ok(tag(servers, &path))
    }

    async fn plan_mcp_upsert(
        &self,
        scope: Scope,
        server: &McpServer,
        previous_name: Option<&str>,
    ) -> Result<WritePlan> {
        let mut plan = single(
            mcp::plan_upsert_json(
                &self.mcp_file(scope)?,
                Dialect::Claude,
                server,
                previous_name,
            )
            .await?,
        );
        if scope == Scope::Workspace {
            if let Some(enabled) = server.enabled {
                plan.changes.push(
                    mcp::plan_claude_project_enabled(
                        &self.settings_local_file()?,
                        &server.name,
                        enabled,
                    )
                    .await?,
                );
            }
        }
        Ok(plan)
    }

    async fn plan_mcp_delete(&self, scope: Scope, name: &str) -> Result<WritePlan> {
        Ok(single(
            mcp::plan_delete_json(&self.mcp_file(scope)?, name).await?,
        ))
    }

    async fn plan_mcp_enabled(&self, scope: Scope, name: &str, enabled: bool) -> Result<WritePlan> {
        if scope == Scope::User {
            return Err(ConfigError::unsupported(
                AgentKind::ClaudeCode,
                Concept::Mcp,
                scope,
                "Claude Code has no on/off switch for user-scope servers; delete the server instead",
            ));
        }
        if !self
            .list_mcp(scope)
            .await?
            .iter()
            .any(|(s, _)| s.name == name)
        {
            return Err(ConfigError::NotFound(name.to_owned()));
        }
        Ok(single(
            mcp::plan_claude_project_enabled(&self.settings_local_file()?, name, enabled).await?,
        ))
    }
}

impl SkillsConfig for ClaudeCodeConfig<'_> {
    async fn list_skills(&self, scope: Scope) -> Result<Vec<(Skill, PathBuf)>> {
        md_store::list_skills(&self.base(scope)?.join("skills")).await
    }
    async fn plan_skill_upsert(
        &self,
        scope: Scope,
        skill: &Skill,
        previous_name: Option<&str>,
    ) -> Result<WritePlan> {
        md_store::plan_skill_upsert(&self.base(scope)?.join("skills"), skill, previous_name).await
    }
    async fn plan_skill_delete(&self, scope: Scope, name: &str) -> Result<WritePlan> {
        md_store::plan_skill_delete(&self.base(scope)?.join("skills"), name).await
    }
}

impl CommandsConfig for ClaudeCodeConfig<'_> {
    async fn list_commands(&self, scope: Scope) -> Result<Vec<(Command, PathBuf)>> {
        md_store::list_commands(&self.base(scope)?.join("commands"), true).await
    }
    async fn plan_command_upsert(
        &self,
        scope: Scope,
        command: &Command,
        previous_name: Option<&str>,
    ) -> Result<WritePlan> {
        md_store::plan_command_upsert(
            &self.base(scope)?.join("commands"),
            true,
            command,
            previous_name,
        )
        .await
    }
    async fn plan_command_delete(&self, scope: Scope, name: &str) -> Result<WritePlan> {
        md_store::plan_command_delete(&self.base(scope)?.join("commands"), true, name).await
    }
}

impl HooksConfig for ClaudeCodeConfig<'_> {
    async fn list_hooks(&self, scope: Scope) -> Result<Vec<(Hook, PathBuf)>> {
        let path = self.settings_file(scope)?;
        Ok(tag_hooks(hooks::list(&path).await?, &path))
    }
    async fn plan_hook_upsert(
        &self,
        scope: Scope,
        hook: &Hook,
        previous_id: Option<&str>,
    ) -> Result<WritePlan> {
        Ok(single(
            hooks::plan_upsert(&self.settings_file(scope)?, hook, previous_id).await?,
        ))
    }
    async fn plan_hook_delete(&self, scope: Scope, id: &str) -> Result<WritePlan> {
        Ok(single(
            hooks::plan_delete(&self.settings_file(scope)?, id).await?,
        ))
    }
}

impl SubagentsConfig for ClaudeCodeConfig<'_> {
    async fn list_subagents(&self, scope: Scope) -> Result<Vec<(Subagent, PathBuf)>> {
        md_store::list_subagents(&self.base(scope)?.join("agents")).await
    }
    async fn plan_subagent_upsert(
        &self,
        scope: Scope,
        agent: &Subagent,
        previous_name: Option<&str>,
    ) -> Result<WritePlan> {
        md_store::plan_subagent_upsert(&self.base(scope)?.join("agents"), agent, previous_name)
            .await
    }
    async fn plan_subagent_delete(&self, scope: Scope, name: &str) -> Result<WritePlan> {
        md_store::plan_subagent_delete(&self.base(scope)?.join("agents"), name).await
    }
}

impl InstructionsConfig for ClaudeCodeConfig<'_> {
    fn instruction_paths(&self, scope: Scope) -> Result<Vec<PathBuf>> {
        Ok(match scope {
            Scope::User => vec![self.paths.claude_dir.join("CLAUDE.md")],
            Scope::Workspace => {
                let ws = self.paths.workspace()?;
                vec![
                    ws.join("CLAUDE.md"),
                    ws.join(".claude").join("CLAUDE.md"),
                    ws.join("CLAUDE.local.md"),
                ]
            }
        })
    }
}

// ================================================================================================ Codex

/// Codex (`$CODEX_HOME`, `<ws>/.codex`).
pub struct CodexConfig<'a> {
    pub paths: &'a ConfigPaths,
}

impl CodexConfig<'_> {
    fn base(&self, scope: Scope) -> Result<PathBuf> {
        Ok(match scope {
            Scope::User => self.paths.codex_home.clone(),
            Scope::Workspace => self.paths.workspace()?.join(".codex"),
        })
    }

    pub(crate) fn config_file(&self, scope: Scope) -> Result<PathBuf> {
        Ok(self.base(scope)?.join("config.toml"))
    }

    fn prompts_dir(&self, scope: Scope) -> Result<PathBuf> {
        match scope {
            Scope::User => Ok(self.paths.codex_home.join("prompts")),
            Scope::Workspace => Err(self.unsupported(Concept::Command, scope)),
        }
    }

    pub(crate) fn dirs(&self, scope: Scope) -> Result<Vec<PathBuf>> {
        let base = self.base(scope)?;
        let mut dirs = vec![base.join("skills")];
        if scope == Scope::User {
            dirs.push(base.join("prompts"));
        }
        Ok(dirs)
    }
}

impl CliConfig for CodexConfig<'_> {
    fn kind(&self) -> AgentKind {
        AgentKind::Codex
    }
}

impl McpConfig for CodexConfig<'_> {
    async fn list_mcp(&self, scope: Scope) -> Result<Vec<(McpServer, PathBuf)>> {
        let path = self.config_file(scope)?;
        Ok(tag(mcp::list_toml(&path).await?, &path))
    }
    async fn plan_mcp_upsert(
        &self,
        scope: Scope,
        server: &McpServer,
        previous_name: Option<&str>,
    ) -> Result<WritePlan> {
        Ok(single(
            mcp::plan_upsert_toml(&self.config_file(scope)?, server, previous_name).await?,
        ))
    }
    async fn plan_mcp_delete(&self, scope: Scope, name: &str) -> Result<WritePlan> {
        Ok(single(
            mcp::plan_delete_toml(&self.config_file(scope)?, name).await?,
        ))
    }
    async fn plan_mcp_enabled(&self, scope: Scope, name: &str, enabled: bool) -> Result<WritePlan> {
        Ok(single(
            mcp::plan_enabled_toml(&self.config_file(scope)?, name, enabled).await?,
        ))
    }
}

impl SkillsConfig for CodexConfig<'_> {
    async fn list_skills(&self, scope: Scope) -> Result<Vec<(Skill, PathBuf)>> {
        md_store::list_skills(&self.base(scope)?.join("skills")).await
    }
    async fn plan_skill_upsert(
        &self,
        scope: Scope,
        skill: &Skill,
        previous_name: Option<&str>,
    ) -> Result<WritePlan> {
        md_store::plan_skill_upsert(&self.base(scope)?.join("skills"), skill, previous_name).await
    }
    async fn plan_skill_delete(&self, scope: Scope, name: &str) -> Result<WritePlan> {
        md_store::plan_skill_delete(&self.base(scope)?.join("skills"), name).await
    }
}

impl CommandsConfig for CodexConfig<'_> {
    async fn list_commands(&self, scope: Scope) -> Result<Vec<(Command, PathBuf)>> {
        md_store::list_commands(&self.prompts_dir(scope)?, false).await
    }
    async fn plan_command_upsert(
        &self,
        scope: Scope,
        command: &Command,
        previous_name: Option<&str>,
    ) -> Result<WritePlan> {
        if command.model.is_some() || command.allowed_tools.is_some() {
            return Err(ConfigError::invalid(
                "Codex prompts have no model or allowed-tools fields",
            ));
        }
        md_store::plan_command_upsert(&self.prompts_dir(scope)?, false, command, previous_name)
            .await
    }
    async fn plan_command_delete(&self, scope: Scope, name: &str) -> Result<WritePlan> {
        md_store::plan_command_delete(&self.prompts_dir(scope)?, false, name).await
    }
}

impl HooksConfig for CodexConfig<'_> {}
impl SubagentsConfig for CodexConfig<'_> {}

impl InstructionsConfig for CodexConfig<'_> {
    fn instruction_paths(&self, scope: Scope) -> Result<Vec<PathBuf>> {
        Ok(match scope {
            Scope::User => vec![self.paths.codex_home.join("AGENTS.md")],
            Scope::Workspace => vec![self.paths.workspace()?.join("AGENTS.md")],
        })
    }
}

// ================================================================================================ agy

/// agy / Antigravity (`~/.gemini`, `<ws>/.agents`).
pub struct AgyConfig<'a> {
    pub paths: &'a ConfigPaths,
}

impl AgyConfig<'_> {
    pub(crate) fn mcp_file(&self, scope: Scope) -> Result<PathBuf> {
        Ok(match scope {
            Scope::User => self.paths.gemini_dir.join("config").join("mcp_config.json"),
            Scope::Workspace => self
                .paths
                .workspace()?
                .join(".agents")
                .join("mcp_config.json"),
        })
    }
}

impl CliConfig for AgyConfig<'_> {
    fn kind(&self) -> AgentKind {
        AgentKind::Agy
    }
}

impl McpConfig for AgyConfig<'_> {
    async fn list_mcp(&self, scope: Scope) -> Result<Vec<(McpServer, PathBuf)>> {
        let path = self.mcp_file(scope)?;
        Ok(tag(mcp::list_json(&path, Dialect::Agy).await?, &path))
    }
    async fn plan_mcp_upsert(
        &self,
        scope: Scope,
        server: &McpServer,
        previous_name: Option<&str>,
    ) -> Result<WritePlan> {
        if server.transport == McpTransport::Sse {
            return Err(ConfigError::invalid("agy servers are stdio or http"));
        }
        Ok(single(
            mcp::plan_upsert_json(&self.mcp_file(scope)?, Dialect::Agy, server, previous_name)
                .await?,
        ))
    }
    async fn plan_mcp_delete(&self, scope: Scope, name: &str) -> Result<WritePlan> {
        Ok(single(
            mcp::plan_delete_json(&self.mcp_file(scope)?, name).await?,
        ))
    }
    async fn plan_mcp_enabled(&self, scope: Scope, name: &str, enabled: bool) -> Result<WritePlan> {
        Ok(single(
            mcp::plan_enabled_json(&self.mcp_file(scope)?, Dialect::Agy, name, enabled).await?,
        ))
    }
}

impl SkillsConfig for AgyConfig<'_> {}
impl CommandsConfig for AgyConfig<'_> {}
impl HooksConfig for AgyConfig<'_> {}
impl SubagentsConfig for AgyConfig<'_> {}

impl InstructionsConfig for AgyConfig<'_> {
    fn instruction_paths(&self, scope: Scope) -> Result<Vec<PathBuf>> {
        Ok(match scope {
            Scope::User => vec![self.paths.gemini_dir.join("GEMINI.md")],
            Scope::Workspace => {
                let ws = self.paths.workspace()?;
                vec![ws.join("GEMINI.md"), ws.join("AGENTS.md")]
            }
        })
    }
}

// ================================================================================================ Harness overlay

/// Harness MCP overlay (`<data dir>/mcp.json`), user scope only.
pub struct HarnessOverlayConfig<'a> {
    pub paths: &'a ConfigPaths,
}

impl HarnessOverlayConfig<'_> {
    pub(crate) fn file(&self, scope: Scope) -> Result<PathBuf> {
        match scope {
            Scope::User => Ok(self.paths.harness_dir.join(crate::overlay::OVERLAY_FILE)),
            Scope::Workspace => Err(self.unsupported(Concept::Mcp, scope)),
        }
    }
}

impl CliConfig for HarnessOverlayConfig<'_> {
    fn kind(&self) -> AgentKind {
        AgentKind::Harness
    }
}

impl McpConfig for HarnessOverlayConfig<'_> {
    async fn list_mcp(&self, scope: Scope) -> Result<Vec<(McpServer, PathBuf)>> {
        let path = self.file(scope)?;
        Ok(tag(mcp::list_json(&path, Dialect::Overlay).await?, &path))
    }
    async fn plan_mcp_upsert(
        &self,
        scope: Scope,
        server: &McpServer,
        previous_name: Option<&str>,
    ) -> Result<WritePlan> {
        if crate::overlay::is_builtin(&server.name) {
            return Err(ConfigError::invalid(format!("{} is reserved", server.name)));
        }
        Ok(single(
            mcp::plan_upsert_json(&self.file(scope)?, Dialect::Overlay, server, previous_name)
                .await?,
        ))
    }
    async fn plan_mcp_delete(&self, scope: Scope, name: &str) -> Result<WritePlan> {
        Ok(single(
            mcp::plan_delete_json(&self.file(scope)?, name).await?,
        ))
    }
    async fn plan_mcp_enabled(&self, scope: Scope, name: &str, enabled: bool) -> Result<WritePlan> {
        Ok(single(
            mcp::plan_enabled_json(&self.file(scope)?, Dialect::Overlay, name, enabled).await?,
        ))
    }
}

impl HarnessOverlayConfig<'_> {
    fn skills_dir(&self, scope: Scope) -> Result<PathBuf> {
        match scope {
            Scope::User => Ok(self.paths.harness_dir.join("skills")),
            Scope::Workspace => Err(self.unsupported(Concept::Skill, scope)),
        }
    }
}

impl SkillsConfig for HarnessOverlayConfig<'_> {
    async fn list_skills(&self, scope: Scope) -> Result<Vec<(Skill, PathBuf)>> {
        md_store::list_skills(&self.skills_dir(scope)?).await
    }
    async fn plan_skill_upsert(
        &self,
        scope: Scope,
        skill: &Skill,
        previous_name: Option<&str>,
    ) -> Result<WritePlan> {
        md_store::plan_skill_upsert(&self.skills_dir(scope)?, skill, previous_name).await
    }
    async fn plan_skill_delete(&self, scope: Scope, name: &str) -> Result<WritePlan> {
        md_store::plan_skill_delete(&self.skills_dir(scope)?, name).await
    }
}

impl CommandsConfig for HarnessOverlayConfig<'_> {}
impl HooksConfig for HarnessOverlayConfig<'_> {}
impl SubagentsConfig for HarnessOverlayConfig<'_> {}
impl InstructionsConfig for HarnessOverlayConfig<'_> {}

// ================================================================================================ support table

fn row(
    concept: Concept,
    user: bool,
    workspace: bool,
    can_disable: bool,
    reason: Option<&str>,
) -> ConceptSupport {
    ConceptSupport {
        concept,
        user,
        workspace,
        can_disable,
        transports: Vec::new(),
        reason: reason.map(str::to_owned),
    }
}

/// What `agent` supports for `concept`.
#[must_use]
pub fn support_of(agent: AgentKind, concept: Concept) -> ConceptSupport {
    use McpTransport::{Http, Sse, Stdio};
    let none = |why: &str| row(concept, false, false, false, Some(why));
    match (agent, concept) {
        (AgentKind::ClaudeCode, Concept::Mcp) => ConceptSupport {
            transports: vec![Stdio, Http, Sse],
            ..row(
                concept,
                true,
                true,
                true,
                Some("on/off applies to workspace (.mcp.json) servers only"),
            )
        },
        (AgentKind::ClaudeCode, Concept::Skill | Concept::Subagent) => {
            row(concept, true, true, false, None)
        }
        (AgentKind::ClaudeCode, Concept::Command | Concept::Hook | Concept::Instructions) => {
            row(concept, true, true, false, None)
        }
        (AgentKind::Codex, Concept::Mcp) => ConceptSupport {
            transports: vec![Stdio, Http],
            ..row(concept, true, true, true, None)
        },
        (AgentKind::Codex, Concept::Skill) => row(
            concept,
            true,
            true,
            false,
            Some("workspace skills (.codex/skills) are unverified"),
        ),
        (AgentKind::Codex, Concept::Command) => row(
            concept,
            true,
            false,
            false,
            Some("Codex custom prompts are user-scope only"),
        ),
        (AgentKind::Codex, Concept::Instructions) => row(concept, true, true, false, None),
        (AgentKind::Codex, Concept::Hook) => {
            none("Codex hook format is not verified; not supported")
        }
        (AgentKind::Codex, Concept::Subagent) => none("Codex has no subagent definitions"),
        (AgentKind::Agy, Concept::Mcp) => ConceptSupport {
            transports: vec![Stdio, Http],
            ..row(
                concept,
                true,
                true,
                true,
                Some("agy mcp_config.json format is unverified"),
            )
        },
        (AgentKind::Agy, Concept::Instructions) => row(concept, true, true, false, None),
        (AgentKind::Agy, _) => none("agy has no such configuration that Harness can verify"),
        (AgentKind::Harness, Concept::Mcp) => ConceptSupport {
            transports: vec![Stdio, Http, Sse],
            ..row(
                concept,
                true,
                false,
                true,
                Some("the Harness overlay is user scope only"),
            )
        },
        (AgentKind::Harness, Concept::Skill) => row(
            concept,
            true,
            false,
            false,
            Some("Harness skills are user scope only"),
        ),
        (AgentKind::Harness, _) => none("Harness holds its own MCP servers and skills only"),
    }
}
