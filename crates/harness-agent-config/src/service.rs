//! `AgentConfigService`: the generic facade the settings pages use (list → plan → preview → apply).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use harness_settings::fsx;
use tokio::sync::broadcast;

use crate::auth::AuthProbe;
use crate::clis::{
    instruction_files, support_of, AgyConfig, ClaudeCodeConfig, CodexConfig, CommandsConfig,
    HarnessOverlayConfig, HooksConfig, InstructionsConfig, McpConfig, SkillsConfig,
    SubagentsConfig,
};
use crate::error::{ConfigError, Result};
use crate::paths::ConfigPaths;
use crate::plan::{self, ApplyOutcome, FileChange, Kind, WritePlan};
use crate::types::{
    AgentKind, AuthState, Concept, ConceptSupport, ConfigChanged, ConfigTarget, Entry, EntryBody,
    McpServer, Scope,
};

/// Runs `$body` with `$c` bound to the concrete CLI config of `$agent`.
macro_rules! with_cli {
    ($paths:expr, $agent:expr, $c:ident => $body:expr) => {
        match $agent {
            AgentKind::ClaudeCode => {
                let $c = ClaudeCodeConfig { paths: $paths };
                $body
            }
            AgentKind::Codex => {
                let $c = CodexConfig { paths: $paths };
                $body
            }
            AgentKind::Agy => {
                let $c = AgyConfig { paths: $paths };
                $body
            }
            AgentKind::Harness => {
                let $c = HarnessOverlayConfig { paths: $paths };
                $body
            }
        }
    };
}

/// Facade over every CLI and concept.
pub struct AgentConfigService {
    paths: ConfigPaths,
    probe: Arc<dyn AuthProbe>,
    events: broadcast::Sender<ConfigChanged>,
}

impl AgentConfigService {
    #[must_use]
    pub fn new(paths: ConfigPaths, probe: Arc<dyn AuthProbe>) -> Self {
        let (events, _) = broadcast::channel(16);
        Self {
            paths,
            probe,
            events,
        }
    }

    #[must_use]
    pub fn paths(&self) -> &ConfigPaths {
        &self.paths
    }

    /// Switches the workspace used for `Scope::Workspace`.
    pub fn set_workspace(&mut self, workspace: Option<PathBuf>) {
        self.paths.workspace = workspace;
    }

    /// Notifications after every successful [`AgentConfigService::apply`].
    #[must_use]
    pub fn subscribe(&self) -> broadcast::Receiver<ConfigChanged> {
        self.events.subscribe()
    }

    /// Support table for `agent` (one row per concept).
    #[must_use]
    pub fn capabilities(&self, agent: AgentKind) -> Vec<ConceptSupport> {
        Concept::ALL.iter().map(|c| support_of(agent, *c)).collect()
    }

    fn check_support(target: ConfigTarget, concept: Concept) -> Result<()> {
        let s = support_of(target.agent, concept);
        let ok = match target.scope {
            Scope::User => s.user,
            Scope::Workspace => s.workspace,
        };
        if ok {
            Ok(())
        } else {
            Err(ConfigError::unsupported(
                target.agent,
                concept,
                target.scope,
                s.reason
                    .unwrap_or_else(|| "not available for this scope".into()),
            ))
        }
    }

    /// Login presence of `agent` (never reads credentials).
    pub async fn auth_state(&self, agent: AgentKind) -> AuthState {
        self.probe.probe(agent, &self.paths).await
    }

    /// Entries of `concept` for `target`.
    ///
    /// # Errors
    ///
    /// Unsupported, invalid file, I/O.
    pub async fn list(&self, concept: Concept, target: ConfigTarget) -> Result<Vec<Entry>> {
        Self::check_support(target, concept)?;
        let scope = target.scope;
        let p = &self.paths;
        Ok(match concept {
            Concept::Mcp => with_cli!(p, target.agent, c => c.list_mcp(scope).await?)
                .into_iter()
                .map(|(s, path)| Entry {
                    name: s.name.clone(),
                    scope,
                    enabled: s.enabled,
                    summary: mcp_summary(&s),
                    source_path: path,
                    body: EntryBody::Mcp(s),
                })
                .collect(),
            Concept::Skill => with_cli!(p, target.agent, c => c.list_skills(scope).await?)
                .into_iter()
                .map(|(s, path)| Entry {
                    name: s.name.clone(),
                    scope,
                    enabled: None,
                    summary: s.description.clone().unwrap_or_default(),
                    source_path: path,
                    body: EntryBody::Skill(s),
                })
                .collect(),
            Concept::Command => with_cli!(p, target.agent, c => c.list_commands(scope).await?)
                .into_iter()
                .map(|(s, path)| Entry {
                    name: s.name.clone(),
                    scope,
                    enabled: None,
                    summary: s.description.clone().unwrap_or_default(),
                    source_path: path,
                    body: EntryBody::Command(s),
                })
                .collect(),
            Concept::Hook => with_cli!(p, target.agent, c => c.list_hooks(scope).await?)
                .into_iter()
                .map(|(h, path)| Entry {
                    name: h.id.clone(),
                    scope,
                    enabled: None,
                    summary: format!(
                        "{} {}: {}",
                        h.event,
                        h.matcher.clone().unwrap_or_default(),
                        h.command
                    ),
                    source_path: path,
                    body: EntryBody::Hook(h),
                })
                .collect(),
            Concept::Subagent => with_cli!(p, target.agent, c => c.list_subagents(scope).await?)
                .into_iter()
                .map(|(s, path)| Entry {
                    name: s.name.clone(),
                    scope,
                    enabled: None,
                    summary: s.description.clone().unwrap_or_default(),
                    source_path: path,
                    body: EntryBody::Subagent(s),
                })
                .collect(),
            Concept::Instructions => {
                let files = with_cli!(p, target.agent, c => instruction_files(&c, scope).await?);
                files
                    .into_iter()
                    .map(|f| Entry {
                        name: f
                            .path
                            .file_name()
                            .map(|n| n.to_string_lossy().into_owned())
                            .unwrap_or_default(),
                        scope,
                        enabled: None,
                        summary: f.path.display().to_string(),
                        source_path: f.path.clone(),
                        body: EntryBody::Instructions {
                            file: f,
                            text: None,
                        },
                    })
                    .collect()
            }
        })
    }

    /// Plan to create or update `body` (renaming from `previous_name`; for hooks the previous id).
    ///
    /// # Errors
    ///
    /// Unsupported, invalid input or file, I/O.
    pub async fn plan_upsert(
        &self,
        target: ConfigTarget,
        body: &EntryBody,
        previous_name: Option<&str>,
    ) -> Result<WritePlan> {
        Self::check_support(target, body.concept())?;
        let scope = target.scope;
        let p = &self.paths;
        match body {
            EntryBody::Mcp(s) => {
                with_cli!(p, target.agent, c => c.plan_mcp_upsert(scope, s, previous_name).await)
            }
            EntryBody::Skill(s) => {
                with_cli!(p, target.agent, c => c.plan_skill_upsert(scope, s, previous_name).await)
            }
            EntryBody::Command(s) => {
                with_cli!(p, target.agent, c => c.plan_command_upsert(scope, s, previous_name).await)
            }
            EntryBody::Hook(h) => {
                with_cli!(p, target.agent, c => c.plan_hook_upsert(scope, h, previous_name).await)
            }
            EntryBody::Subagent(s) => {
                with_cli!(p, target.agent, c => c.plan_subagent_upsert(scope, s, previous_name).await)
            }
            EntryBody::Instructions { file, text } => {
                self.plan_instructions(target, &file.path, text.as_deref().unwrap_or_default())
                    .await
            }
        }
    }

    /// Plan to delete entry `name` (hook id for hooks).
    ///
    /// # Errors
    ///
    /// Unsupported, not found, invalid file, I/O.
    pub async fn plan_delete(
        &self,
        concept: Concept,
        target: ConfigTarget,
        name: &str,
    ) -> Result<WritePlan> {
        Self::check_support(target, concept)?;
        let scope = target.scope;
        let p = &self.paths;
        match concept {
            Concept::Mcp => with_cli!(p, target.agent, c => c.plan_mcp_delete(scope, name).await),
            Concept::Skill => {
                with_cli!(p, target.agent, c => c.plan_skill_delete(scope, name).await)
            }
            Concept::Command => {
                with_cli!(p, target.agent, c => c.plan_command_delete(scope, name).await)
            }
            Concept::Hook => with_cli!(p, target.agent, c => c.plan_hook_delete(scope, name).await),
            Concept::Subagent => {
                with_cli!(p, target.agent, c => c.plan_subagent_delete(scope, name).await)
            }
            Concept::Instructions => Err(ConfigError::unsupported(
                target.agent,
                concept,
                scope,
                "Harness does not delete instruction files",
            )),
        }
    }

    /// Plan to enable/disable entry `name` (MCP only).
    ///
    /// # Errors
    ///
    /// Unsupported, not found, invalid file, I/O.
    pub async fn plan_set_enabled(
        &self,
        concept: Concept,
        target: ConfigTarget,
        name: &str,
        enabled: bool,
    ) -> Result<WritePlan> {
        Self::check_support(target, concept)?;
        if concept != Concept::Mcp {
            return Err(ConfigError::unsupported(
                target.agent,
                concept,
                target.scope,
                "only MCP servers can be switched off",
            ));
        }
        let p = &self.paths;
        with_cli!(p, target.agent, c => c.plan_mcp_enabled(target.scope, name, enabled).await)
    }

    /// Text of an instructions file (must be one of the candidates of `target`).
    ///
    /// # Errors
    ///
    /// Not a candidate, I/O.
    pub async fn read_instructions(
        &self,
        target: ConfigTarget,
        path: &Path,
    ) -> Result<Option<String>> {
        self.check_instruction_path(target, path)?;
        Ok(fsx::read_to_string_opt(path).await?)
    }

    /// Plan to write an instructions file the user picked.
    ///
    /// # Errors
    ///
    /// Not a candidate, I/O.
    pub async fn plan_instructions(
        &self,
        target: ConfigTarget,
        path: &Path,
        text: &str,
    ) -> Result<WritePlan> {
        self.check_instruction_path(target, path)?;
        let before = fsx::read_to_string_opt(path).await?;
        Ok(WritePlan::single(FileChange::write(
            path.to_path_buf(),
            before,
            text.to_owned(),
            Kind::Text,
        )))
    }

    fn check_instruction_path(&self, target: ConfigTarget, path: &Path) -> Result<()> {
        Self::check_support(target, Concept::Instructions)?;
        let p = &self.paths;
        let allowed = with_cli!(p, target.agent, c => c.instruction_paths(target.scope)?);
        if allowed.iter().any(|a| a == path) {
            Ok(())
        } else {
            Err(ConfigError::Forbidden(format!(
                "{} is not an instructions file of this CLI",
                path.display()
            )))
        }
    }

    /// Plan to replace a whole config file with `text` (raw editor). Only files that hold supported concepts are
    /// accepted, JSON/TOML must parse, credentials files are refused.
    ///
    /// # Errors
    ///
    /// Not a known config file, invalid text, I/O.
    pub async fn plan_raw(&self, path: &Path, text: &str) -> Result<WritePlan> {
        if fsx::is_forbidden(path) || !self.is_config_path(path) {
            return Err(ConfigError::Forbidden(format!(
                "{} is not an editable config file",
                path.display()
            )));
        }
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        let kind = match ext.as_str() {
            "json" => {
                serde_json::from_str::<serde_json::Value>(text)
                    .map_err(|e| ConfigError::invalid(e.to_string()))?;
                Kind::Json
            }
            "toml" => {
                text.parse::<toml_edit::DocumentMut>()
                    .map_err(|e| ConfigError::invalid(e.to_string()))?;
                Kind::Toml
            }
            _ => Kind::Text,
        };
        let before = fsx::read_to_string_opt(path).await?;
        Ok(WritePlan::single(FileChange::write(
            path.to_path_buf(),
            before,
            text.to_owned(),
            kind,
        )))
    }

    fn is_config_path(&self, path: &Path) -> bool {
        let p = &self.paths;
        let scopes: &[Scope] = if p.workspace.is_some() {
            &[Scope::User, Scope::Workspace]
        } else {
            &[Scope::User]
        };
        let mut files: Vec<PathBuf> = vec![
            p.harness_dir.join(crate::overlay::OVERLAY_FILE),
            p.harness_dir.join(crate::launch::AGENTS_FILE),
        ];
        let mut dirs: Vec<PathBuf> = Vec::new();
        for &scope in scopes {
            let claude = ClaudeCodeConfig { paths: p };
            let codex = CodexConfig { paths: p };
            let agy = AgyConfig { paths: p };
            files.extend(claude.settings_file(scope).ok());
            files.extend(codex.config_file(scope).ok());
            files.extend(agy.mcp_file(scope).ok());
            for kind in AgentKind::CLIS {
                files.extend(
                    with_cli!(p, kind, c => c.instruction_paths(scope)).unwrap_or_default(),
                );
            }
            dirs.extend(claude.dirs(scope).unwrap_or_default());
            dirs.extend(codex.dirs(scope).unwrap_or_default());
        }
        files.push(p.claude_json.clone());
        if let Some(ws) = &p.workspace {
            files.push(ws.join(".mcp.json"));
            files.push(ws.join(".claude").join("settings.local.json"));
        }
        let is_md = path
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("md"));
        let no_parent_refs = path
            .components()
            .all(|c| !matches!(c, std::path::Component::ParentDir));
        files.iter().any(|f| f == path)
            || (is_md && no_parent_refs && dirs.iter().any(|d| path.starts_with(d)))
    }

    /// Plan to copy entry `name` of `concept` from one target to another (MCP-004, SKL-003, COM-002).
    ///
    /// # Errors
    ///
    /// Unsupported on either side, not found, invalid, I/O.
    pub async fn plan_copy(
        &self,
        concept: Concept,
        from: ConfigTarget,
        to: ConfigTarget,
        name: &str,
    ) -> Result<WritePlan> {
        let entry = self
            .list(concept, from)
            .await?
            .into_iter()
            .find(|e| e.name == name)
            .ok_or_else(|| ConfigError::NotFound(name.to_owned()))?;
        let body = match entry.body {
            EntryBody::Mcp(mut s) => {
                // 目标 CLI 的开关语义不同：复制时只保留“启用”，避免把来源 CLI 的禁用状态误写成另一种格式。
                s.enabled = None;
                if to.agent != AgentKind::Harness {
                    s.agents.clear();
                }
                s.startup_timeout_sec = s
                    .startup_timeout_sec
                    .filter(|_| to.agent == AgentKind::Codex);
                s.tool_timeout_sec = s.tool_timeout_sec.filter(|_| to.agent == AgentKind::Codex);
                s.extra.clear();
                EntryBody::Mcp(s)
            }
            EntryBody::Hook(mut h) => {
                h.id.clear();
                EntryBody::Hook(h)
            }
            other => other,
        };
        let is_skill = matches!(body, EntryBody::Skill(_));
        let mut plan = self.plan_upsert(to, &body, None).await?;
        if is_skill {
            let source_dir = entry.source_path.parent().map(Path::to_path_buf);
            let target_dir = plan
                .changes
                .first()
                .and_then(|c| c.path.parent())
                .map(Path::to_path_buf);
            if let (Some(source_dir), Some(target_dir)) = (source_dir, target_dir) {
                plan.changes
                    .extend(skill_file_copies(&source_dir, &target_dir).await?);
            }
        }
        Ok(plan)
    }

    /// Applies a plan (conflict check, atomic writes) and broadcasts [`ConfigChanged`].
    ///
    /// # Errors
    ///
    /// Conflict or I/O.
    pub async fn apply(&self, plan: &WritePlan) -> Result<ApplyOutcome> {
        let outcome = plan::apply(plan).await?;
        if !outcome.written.is_empty() {
            tracing::info!(files = outcome.written.len(), "agent config written");
            let _ = self.events.send(ConfigChanged {
                paths: outcome.written.clone(),
            });
        }
        Ok(outcome)
    }
}

const SKILL_COPY_MAX_FILES: usize = 200;
const SKILL_COPY_MAX_BYTES: u64 = 1024 * 1024;

async fn skill_file_copies(source: &Path, target: &Path) -> Result<Vec<FileChange>> {
    let mut changes = Vec::new();
    let mut pending = vec![PathBuf::new()];
    while let Some(relative) = pending.pop() {
        let mut dir = match tokio::fs::read_dir(source.join(&relative)).await {
            Ok(dir) => dir,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
        };
        let mut names = Vec::new();
        while let Some(item) = dir.next_entry().await? {
            names.push((item.file_name(), item.file_type().await?));
        }
        names.sort_by(|a, b| a.0.cmp(&b.0));
        for (name, kind) in names {
            let child = relative.join(&name);
            let hidden = name.to_string_lossy().starts_with('.');
            if kind.is_dir() {
                if !hidden {
                    pending.push(child);
                }
                continue;
            }
            if !kind.is_file() || child.as_os_str() == "SKILL.md" {
                continue;
            }
            if changes.len() >= SKILL_COPY_MAX_FILES {
                return Ok(changes);
            }
            let from = source.join(&child);
            if tokio::fs::metadata(&from).await?.len() > SKILL_COPY_MAX_BYTES {
                continue;
            }
            let Ok(text) = String::from_utf8(tokio::fs::read(&from).await?) else {
                continue;
            };
            let to = target.join(&child);
            let before = fsx::read_to_string_opt(&to).await?;
            changes.push(FileChange::write(to, before, text, Kind::Text));
        }
    }
    Ok(changes)
}

fn mcp_summary(s: &McpServer) -> String {
    match (&s.command, &s.url) {
        (Some(cmd), _) if s.url.is_none() => std::iter::once(cmd.as_str())
            .chain(s.args.iter().map(String::as_str))
            .collect::<Vec<_>>()
            .join(" "),
        (_, Some(url)) => url.clone(),
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::StaticProbe;

    #[tokio::test]
    async fn copies_skill_folder_to_harness() -> Result<()> {
        let home = tempfile::tempdir()?;
        let data = tempfile::tempdir()?;
        let skill = home.path().join(".claude").join("skills").join("graph");
        std::fs::create_dir_all(skill.join("references"))?;
        std::fs::create_dir_all(skill.join(".cache"))?;
        std::fs::write(
            skill.join("SKILL.md"),
            "---\nname: graph\ndescription: Graph\n---\nBody\n",
        )?;
        std::fs::write(skill.join("references").join("api.md"), "ref text\n")?;
        std::fs::write(skill.join("logo.bin"), [0xff_u8, 0xfe, 0x00])?;
        std::fs::write(skill.join(".cache").join("x.md"), "hidden\n")?;
        let paths = ConfigPaths::for_home(home.path(), data.path(), None);
        let svc = AgentConfigService::new(paths, Arc::new(StaticProbe::default()));
        let from = ConfigTarget {
            agent: AgentKind::ClaudeCode,
            scope: Scope::User,
        };
        let to = ConfigTarget {
            agent: AgentKind::Harness,
            scope: Scope::User,
        };
        let plan = svc.plan_copy(Concept::Skill, from, to, "graph").await?;
        svc.apply(&plan).await?;
        let target = data.path().join("skills").join("graph");
        assert!(std::fs::read_to_string(target.join("SKILL.md"))?.contains("Body"));
        assert_eq!(
            std::fs::read_to_string(target.join("references").join("api.md"))?,
            "ref text\n"
        );
        assert!(!target.join("logo.bin").exists());
        assert!(!target.join(".cache").exists());
        let listed = svc.list(Concept::Skill, to).await?;
        assert_eq!(listed.len(), 1);
        Ok(())
    }
}
