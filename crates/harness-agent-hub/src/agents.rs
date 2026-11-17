//! Agent adapters (spec §6). Round 8.1: every agent speaks ACP over stdio; adapters differ in launch
//! command and error classification.

use std::path::{Path, PathBuf};

use harness_core_api::{AgentId, Availability, ErrorClass};

use crate::quota::{classify_generic, AgentErrorInfo};

/// Wire protocol of an agent process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transport {
    /// Agent Client Protocol over stdio (JSON-RPC, newline-delimited).
    Acp,
}

/// How errors of this agent are classified (spec §6.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorDialect {
    /// Claude Code (through claude-agent-acp): usage-limit texts `Claude AI usage limit reached|<unix>`.
    ClaudeCode,
    /// Codex (through codex-acp): `codexErrorInfo` = `usageLimitExceeded` / `rateLimitExceeded`.
    Codex,
    /// agy: format unknown, generic rules only.
    Agy,
    /// Harness convention `data.harnessErrorKind` + generic text rules (fake agents, extra agents).
    Generic,
}

/// One configured agent. `Debug` prints env names only (values may be API keys, §9.7).
#[derive(Clone)]
pub struct AgentSpec {
    pub id: AgentId,
    pub display_name: String,
    /// Command name (PATH lookup) or absolute path.
    pub program: String,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    pub install_hint: String,
    pub transport: Transport,
    pub dialect: ErrorDialect,
}

impl std::fmt::Debug for AgentSpec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let env: Vec<&str> = self.env.iter().map(|(k, _)| k.as_str()).collect();
        f.debug_struct("AgentSpec")
            .field("id", &self.id)
            .field("display_name", &self.display_name)
            .field("program", &self.program)
            .field("args", &self.args)
            .field("env", &env)
            .field("transport", &self.transport)
            .field("dialect", &self.dialect)
            .finish_non_exhaustive()
    }
}

impl AgentSpec {
    /// Resolves availability by PATH lookup only (never runs the CLI).
    #[must_use]
    pub fn probe(&self) -> Availability {
        match resolve_program(&self.program) {
            Some(path) => Availability::Available {
                command_path: path.to_string_lossy().into_owned(),
            },
            // §9.10：适配器缺失但 CLI 已安装时报告 AdapterMissing，而不是笼统的 "Not installed"。
            None => crate::setup::probe_missing(self),
        }
    }

    /// Classifies an agent failure (quota / transient / fatal).
    #[must_use]
    pub fn classify_error(&self, info: &AgentErrorInfo) -> ErrorClass {
        classify_generic(info, self.dialect)
    }
}

/// Built-in agents in default failover order: Claude Code, Codex, agy, Command Code.
#[must_use]
pub fn builtin_agents() -> Vec<AgentSpec> {
    vec![
        AgentSpec {
            id: AgentId::new("claude-code"),
            display_name: "Claude Code".into(),
            // 修复：@zed-industries/claude-code-acp 已弃用并改名为 @agentclientprotocol/claude-agent-acp（bin 为 claude-agent-acp）。
            program: "claude-agent-acp".into(),
            args: Vec::new(),
            env: Vec::new(),
            install_hint: "install the ACP adapter: npm i -g @agentclientprotocol/claude-agent-acp (uses your `claude` login)".into(),
            transport: Transport::Acp,
            dialect: ErrorDialect::ClaudeCode,
        },
        AgentSpec {
            id: AgentId::new("codex"),
            display_name: "Codex".into(),
            // 修复：@zed-industries/codex-acp 已弃用，改由 @agentclientprotocol/codex-acp 提供（bin 名不变）。
            program: "codex-acp".into(),
            args: Vec::new(),
            env: Vec::new(),
            install_hint: "install the ACP adapter: npm i -g @agentclientprotocol/codex-acp (uses your `codex` login)".into(),
            transport: Transport::Acp,
            dialect: ErrorDialect::Codex,
        },
        AgentSpec {
            id: AgentId::new("agy"),
            display_name: "Antigravity (agy)".into(),
            program: "agy".into(),
            // agy 1.5.x: ACP 是子命令 `agy acp`，旧的 `--acp` 参数会让进程直接退出（-32099）。
            args: vec!["acp".into()],
            env: Vec::new(),
            install_hint: "install the Antigravity CLI (agy) and sign in by running `agy`; Harness starts it as `agy acp`".into(),
            transport: Transport::Acp,
            dialect: ErrorDialect::Agy,
        },
        AgentSpec {
            id: AgentId::new(COMMAND_CODE),
            display_name: "Command Code".into(),
            // The CLI speaks ACP itself (`command-code acp`, checked with 1.89.2). The npm package also
            // installs `cmd`, but that name is the Windows shell, so the long name is used everywhere.
            program: "command-code".into(),
            args: vec!["acp".into()],
            env: Vec::new(),
            install_hint: "install Command Code: npm i -g command-code, then sign in with `command-code login`, or add API keys under Settings › Agents › API providers".into(),
            transport: Transport::Acp,
            dialect: ErrorDialect::Generic,
        },
    ]
}

/// Agent id of the built-in Command Code agent (the one that uses the CLI's own login).
pub const COMMAND_CODE: &str = "command-code";

/// Finds an executable: absolute/relative paths as-is, bare names on PATH (with PATHEXT on Windows).
#[must_use]
pub fn resolve_program(program: &str) -> Option<PathBuf> {
    let candidate = Path::new(program);
    if candidate.components().count() > 1 || candidate.is_absolute() {
        return candidate.is_file().then(|| candidate.to_path_buf());
    }
    // §9.10：PATH 之后再查安装适配器后发现的 npm 全局 bin 目录，免重启即可用。
    let mut dirs: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).collect())
        .unwrap_or_default();
    dirs.extend(crate::setup::extra_search_dirs());
    let exts: Vec<String> = if cfg!(windows) {
        std::env::var("PATHEXT")
            .unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".into())
            .split(';')
            .filter(|e| !e.is_empty())
            .map(str::to_ascii_lowercase)
            .collect()
    } else {
        Vec::new()
    };
    for dir in dirs {
        if cfg!(windows) {
            for ext in &exts {
                let file = dir.join(format!("{program}{ext}"));
                if file.is_file() {
                    return Some(file);
                }
            }
        } else {
            let file = dir.join(program);
            if file.is_file() {
                return Some(file);
            }
        }
    }
    None
}
