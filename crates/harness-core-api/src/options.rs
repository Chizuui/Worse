//! Session modes, models and config options (API 0.3.0, spec `phase-9-parity.md` §9.1.5).

use serde::{Deserialize, Serialize};

use crate::types::AgentId;

/// One agent mode (ACP `SessionMode`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModeInfo {
    pub id: String,
    pub name: String,
    pub description: Option<String>,
}

/// One model (ACP `ModelInfo`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelInfo {
    pub id: String,
    pub name: String,
    pub description: Option<String>,
}

/// One allowed value of a select config option.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfigValue {
    pub value: String,
    pub name: String,
    pub description: Option<String>,
}

/// One session config option (ACP `configOptions`, e.g. reasoning effort).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfigOption {
    pub id: String,
    pub name: String,
    pub description: Option<String>,
    /// ACP category (`mode`, `model`, `thought_level`, ...), when given.
    pub category: Option<String>,
    pub current: Option<String>,
    pub values: Vec<ConfigValue>,
}

/// Modes of a session's active agent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionModes {
    pub agent: AgentId,
    /// The session's choice, else the configured default, else what the agent reported.
    pub current: Option<String>,
    pub available: Vec<ModeInfo>,
    /// `false` when the agent never advertised modes (pickers show "unknown" instead of an empty list).
    pub known: bool,
}

/// Models of a session's active agent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionModels {
    pub agent: AgentId,
    pub current: Option<String>,
    pub available: Vec<ModelInfo>,
    pub known: bool,
}

/// Config options of a session's active agent; `current` of each option already reflects the session's choice.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionConfigOptions {
    pub agent: AgentId,
    pub options: Vec<ConfigOption>,
    pub known: bool,
}

/// Per-agent defaults applied to a fresh native session without an own choice (AGT-007).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentDefaults {
    pub mode: Option<String>,
    pub model: Option<String>,
    /// Config option id → value (e.g. `effort` → `high`).
    #[serde(default)]
    pub config: std::collections::BTreeMap<String, String>,
}

/// Cached option lists of one agent without a session (settings page, AGT-007); `current` = configured default,
/// else what the agent last reported.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentOptions {
    pub modes: SessionModes,
    pub models: SessionModels,
    pub config: SessionConfigOptions,
}
