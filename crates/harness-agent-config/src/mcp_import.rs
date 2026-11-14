//! Import of user-scope MCP servers from Claude Code, Codex and agy into the Harness overlay
//! (spec phase-9-parity.md §9.1.4 "Import"). One `WritePlan` on `<data dir>/mcp.json`; server content is never
//! logged (env values and args can hold credentials).

use crate::clis::HarnessOverlayConfig;
use crate::error::{ConfigError, Result};
use crate::mcp::{self, Dialect};
use crate::plan::WritePlan;
use crate::service::AgentConfigService;
use crate::types::{AgentKind, Concept, ConfigTarget, EntryBody, McpServer, Scope};

#[derive(Debug, Clone, Default)]
pub struct McpImport {
    pub plan: Option<WritePlan>,
    pub imported: Vec<String>,
    pub skipped: usize,
    pub failed: Vec<AgentKind>,
}

const RESERVED_PREFIX: &str = "harness-";

fn imported_entry(mut server: McpServer) -> McpServer {
    server.enabled = Some(server.enabled != Some(false));
    server.agents.clear();
    server.startup_timeout_sec = None;
    server.tool_timeout_sec = None;
    server.extra.clear();
    server
}

impl AgentConfigService {
    pub async fn plan_import_mcp(&self, sources: &[AgentKind]) -> Result<McpImport> {
        if sources.is_empty() || sources.contains(&AgentKind::Harness) {
            return Err(ConfigError::invalid(
                "import needs Claude Code, Codex or agy as source",
            ));
        }
        let overlay = HarnessOverlayConfig {
            paths: self.paths(),
        };
        let path = overlay.file(Scope::User)?;
        let mut taken: Vec<String> = mcp::list_json(&path, Dialect::Overlay)
            .await?
            .into_iter()
            .map(|s| s.name)
            .collect();
        let mut result = McpImport::default();
        let mut add = Vec::new();
        for &agent in sources {
            let target = ConfigTarget {
                agent,
                scope: Scope::User,
            };
            let entries = match self.list(Concept::Mcp, target).await {
                Ok(entries) => entries,
                Err(error) => {
                    tracing::warn!(?agent, %error, "MCP import source unreadable");
                    result.failed.push(agent);
                    continue;
                }
            };
            for entry in entries {
                let EntryBody::Mcp(server) = entry.body else {
                    continue;
                };
                let server = imported_entry(server);
                if server.name.starts_with(RESERVED_PREFIX)
                    || taken.contains(&server.name)
                    || mcp::validate(&server).is_err()
                {
                    result.skipped += 1;
                    continue;
                }
                taken.push(server.name.clone());
                result.imported.push(server.name.clone());
                add.push(server);
            }
        }
        if result.failed.len() == sources.len() {
            return Err(ConfigError::invalid(
                "could not read the MCP configuration of the chosen CLI",
            ));
        }
        if !add.is_empty() {
            result.plan = Some(WritePlan {
                changes: vec![mcp::plan_add_many_json(&path, Dialect::Overlay, &add).await?],
            });
        }
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::auth::StaticProbe;
    use crate::clis::McpConfig;
    use crate::paths::ConfigPaths;

    fn names(v: &[(McpServer, std::path::PathBuf)]) -> Vec<String> {
        v.iter().map(|(s, _)| s.name.clone()).collect()
    }

    #[tokio::test]
    async fn imports_into_overlay() -> Result<()> {
        let home = tempfile::tempdir()?;
        let data = tempfile::tempdir()?;
        std::fs::write(
            home.path().join(".claude.json"),
            r#"{"mcpServers":{
                "alpha":{"command":"alpha-bin","args":["--x"],"env":{"FAKE_KEY":"fake"}},
                "web":{"type":"http","url":"http://127.0.0.1:9/mcp"},
                "harness-memory":{"command":"x"},
                "kept":{"command":"other"},
                "broken":{"type":"stdio"}},
              "projects":{"C:/p":{"mcpServers":{"proj":{"command":"p"}}}}}"#,
        )?;
        let codex = home.path().join(".codex");
        std::fs::create_dir_all(&codex)?;
        std::fs::write(
            codex.join("config.toml"),
            "[mcp_servers.alpha]\ncommand = \"dup\"\n\n[mcp_servers.off]\ncommand = \"o\"\nenabled = false\nstartup_timeout_sec = 5\n",
        )?;
        std::fs::write(
            data.path().join("mcp.json"),
            r#"{"mcpServers":{"kept":{"command":"mine","enabled":true}}}"#,
        )?;
        let paths = ConfigPaths::for_home(home.path(), data.path(), None);
        let svc = AgentConfigService::new(paths, Arc::new(StaticProbe::default()));
        let got = svc
            .plan_import_mcp(&[AgentKind::ClaudeCode, AgentKind::Codex, AgentKind::Agy])
            .await?;
        assert_eq!(got.imported, ["alpha", "web", "off"]);
        assert_eq!(got.skipped, 4);
        assert_eq!(got.failed, Vec::<AgentKind>::new());
        let plan = got.plan.ok_or_else(|| ConfigError::invalid("no plan"))?;
        svc.apply(&plan).await?;

        let overlay = HarnessOverlayConfig { paths: svc.paths() };
        let listed = overlay.list_mcp(Scope::User).await?;
        assert_eq!(names(&listed), ["kept", "alpha", "web", "off"]);
        let find = |n: &str| {
            listed
                .iter()
                .find(|(s, _)| s.name == n)
                .map(|(s, _)| s.clone())
        };
        let alpha = find("alpha").ok_or_else(|| ConfigError::invalid("alpha"))?;
        assert_eq!(alpha.command.as_deref(), Some("alpha-bin"));
        assert_eq!(alpha.enabled, Some(true));
        assert_eq!(alpha.agents, Vec::<String>::new());
        let off = find("off").ok_or_else(|| ConfigError::invalid("off"))?;
        assert_eq!(off.enabled, Some(false));
        assert_eq!(off.startup_timeout_sec, None);
        let kept = find("kept").ok_or_else(|| ConfigError::invalid("kept"))?;
        assert_eq!(kept.command.as_deref(), Some("mine"));

        let again = svc.plan_import_mcp(&[AgentKind::ClaudeCode]).await?;
        assert!(again.plan.is_none());
        assert_eq!(again.imported, Vec::<String>::new());
        assert_eq!(again.skipped, 5);
        Ok(())
    }

    #[tokio::test]
    async fn unreadable_source() -> Result<()> {
        let home = tempfile::tempdir()?;
        let data = tempfile::tempdir()?;
        std::fs::write(home.path().join(".claude.json"), "{ not json")?;
        let paths = ConfigPaths::for_home(home.path(), data.path(), None);
        let svc = AgentConfigService::new(paths, Arc::new(StaticProbe::default()));
        assert!(svc.plan_import_mcp(&[AgentKind::ClaudeCode]).await.is_err());
        let both = svc
            .plan_import_mcp(&[AgentKind::ClaudeCode, AgentKind::Agy])
            .await?;
        assert_eq!(both.failed, [AgentKind::ClaudeCode]);
        assert!(both.plan.is_none());
        assert!(svc.plan_import_mcp(&[AgentKind::Harness]).await.is_err());
        Ok(())
    }
}
