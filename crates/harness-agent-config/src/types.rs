//! Public data types (spec §9.1.3).

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Which CLI's configuration. `Harness` is the Harness MCP overlay (`<data dir>/mcp.json`, MCP only).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum AgentKind {
    ClaudeCode,
    Codex,
    Agy,
    Harness,
}

impl AgentKind {
    /// The three agent CLIs (without the overlay).
    pub const CLIS: [AgentKind; 3] = [AgentKind::ClaudeCode, AgentKind::Codex, AgentKind::Agy];

    /// Hub agent id (`claude-code`, `codex`, `agy`, `harness`).
    #[must_use]
    pub fn agent_id(self) -> &'static str {
        match self {
            Self::ClaudeCode => "claude-code",
            Self::Codex => "codex",
            Self::Agy => "agy",
            Self::Harness => "harness",
        }
    }

    /// Inverse of [`AgentKind::agent_id`].
    #[must_use]
    pub fn from_agent_id(id: &str) -> Option<Self> {
        match id {
            "claude-code" => Some(Self::ClaudeCode),
            "codex" => Some(Self::Codex),
            "agy" => Some(Self::Agy),
            "harness" => Some(Self::Harness),
            _ => None,
        }
    }
}

/// User-wide or workspace configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Scope {
    User,
    Workspace,
}

/// What to read or write.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfigTarget {
    pub agent: AgentKind,
    pub scope: Scope,
}

impl ConfigTarget {
    #[must_use]
    pub fn new(agent: AgentKind, scope: Scope) -> Self {
        Self { agent, scope }
    }
}

/// Configuration concept.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Concept {
    Mcp,
    Skill,
    Command,
    Hook,
    Subagent,
    Instructions,
}

impl Concept {
    pub const ALL: [Concept; 6] = [
        Concept::Mcp,
        Concept::Skill,
        Concept::Command,
        Concept::Hook,
        Concept::Subagent,
        Concept::Instructions,
    ];
}

/// What a CLI supports for one concept (the UI greys out the rest without calling).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConceptSupport {
    pub concept: Concept,
    pub user: bool,
    pub workspace: bool,
    /// Entries can be enabled/disabled without deleting them.
    pub can_disable: bool,
    /// MCP transports (`stdio`, `http`, `sse`); empty for other concepts.
    pub transports: Vec<McpTransport>,
    /// Why something is missing or unverified (English, for a tooltip).
    pub reason: Option<String>,
}

/// MCP transport.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum McpTransport {
    #[default]
    Stdio,
    Http,
    Sse,
}

/// One MCP server definition.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct McpServer {
    pub name: String,
    pub transport: McpTransport,
    pub command: Option<String>,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub url: Option<String>,
    pub headers: BTreeMap<String, String>,
    /// `None` when this target cannot toggle servers.
    pub enabled: Option<bool>,
    pub startup_timeout_sec: Option<f64>,
    pub tool_timeout_sec: Option<f64>,
    /// Overlay only: agent ids that receive the server (empty = all).
    pub agents: Vec<String>,
    /// Unknown keys of the entry, written back unchanged.
    pub extra: serde_json::Map<String, serde_json::Value>,
}

/// One skill (`<dir>/<name>/SKILL.md`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Skill {
    pub name: String,
    pub description: Option<String>,
    pub body: String,
    /// Other front matter lines, verbatim.
    pub extra_front_matter: Vec<(String, String)>,
    /// Other files in the skill directory (relative), read-only information.
    pub files: Vec<String>,
    /// Problems found while reading (missing description, name mismatch, ...).
    pub diagnostics: Vec<String>,
}

/// One slash command / custom prompt.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Command {
    /// `a:b` for `a/b.md`.
    pub name: String,
    pub description: Option<String>,
    pub argument_hint: Option<String>,
    pub model: Option<String>,
    pub allowed_tools: Option<String>,
    pub body: String,
    pub extra_front_matter: Vec<(String, String)>,
}

/// One hook command (flattened from `hooks.<event>[].hooks[]`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Hook {
    /// Position id `<event>#<group>#<index>`, valid for the content it was read from.
    pub id: String,
    pub event: String,
    pub matcher: Option<String>,
    /// Hook type, `command` by default.
    pub hook_type: String,
    pub command: String,
    pub timeout: Option<f64>,
    pub status_message: Option<String>,
    pub run_async: Option<bool>,
    pub extra: serde_json::Map<String, serde_json::Value>,
}

/// One subagent definition (`agents/<name>.md`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Subagent {
    pub name: String,
    pub description: Option<String>,
    pub tools: Option<String>,
    pub model: Option<String>,
    pub color: Option<String>,
    /// System prompt.
    pub body: String,
    pub extra_front_matter: Vec<(String, String)>,
}

/// One instructions file candidate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InstructionFile {
    pub path: PathBuf,
    pub scope: Scope,
    pub exists: bool,
    pub size: u64,
}

/// Concept-specific content of an [`Entry`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "concept", rename_all = "camelCase")]
pub enum EntryBody {
    Mcp(McpServer),
    Skill(Skill),
    Command(Command),
    Hook(Hook),
    Subagent(Subagent),
    Instructions {
        file: InstructionFile,
        text: Option<String>,
    },
}

impl EntryBody {
    #[must_use]
    pub fn concept(&self) -> Concept {
        match self {
            Self::Mcp(_) => Concept::Mcp,
            Self::Skill(_) => Concept::Skill,
            Self::Command(_) => Concept::Command,
            Self::Hook(_) => Concept::Hook,
            Self::Subagent(_) => Concept::Subagent,
            Self::Instructions { .. } => Concept::Instructions,
        }
    }
}

/// Generic list row for the settings pages.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Entry {
    pub name: String,
    pub scope: Scope,
    /// `None` when the entry cannot be toggled.
    pub enabled: Option<bool>,
    /// One line for the list (command line, URL, description...).
    pub summary: String,
    /// File that defines the entry (for "open file" and `plan_raw`).
    pub source_path: PathBuf,
    pub body: EntryBody,
}

/// Login state of an agent CLI, from presence checks only (credential files are never read).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum AuthState {
    Present,
    Absent,
    Unknown,
}

/// Change notification after a successful apply.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfigChanged {
    pub paths: Vec<PathBuf>,
}
