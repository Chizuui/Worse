//! Hub configuration.

use std::path::PathBuf;
use std::time::Duration;

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

use harness_agent_config::{AgentKind, AuthProbe, FileExistenceProbe};
use harness_core_api::{AgentDefaults, AgentId, FileSearchSource};

use serde::Deserialize;

use crate::agents::{builtin_agents, AgentSpec, ErrorDialect, Transport};
use crate::error::HubError;
use crate::helpers::HelperLaunch;

/// File name of the agent configuration under the hub home.
pub const AGENTS_FILE: &str = "agents.json";

/// Default idle period after which an unused agent process is stopped (spec §10).
pub const DEFAULT_AGENT_IDLE: Duration = Duration::from_secs(300);
/// Grace period between closing an agent's stdin and killing its process tree.
pub const SHUTDOWN_GRACE: Duration = Duration::from_secs(3);

/// Everything the hub needs to start.
#[derive(Debug, Clone)]
pub struct HubConfig {
    /// Data directory (`~/.harness`, or `HARNESS_HOME`).
    pub home: PathBuf,
    /// Configured agents, built-ins first.
    pub agents: Vec<AgentSpec>,
    /// Failover order used until the user sets one.
    pub default_fallback_order: Vec<AgentId>,
    /// Agents that stand for the same service with different API keys (`providers.rs`). A new session asked
    /// for a member takes the members in turn (round robin). Filled when `providers.json` is applied.
    pub key_pools: Vec<Vec<AgentId>>,
    /// Round robin over `key_pools` (off: every session keeps the agent it asked for; failover still walks the
    /// keys). `from_env`: on unless `HARNESS_KEY_ROTATION=off`; `providers.json` `"rotation": "off"` also
    /// turns it off.
    pub rotate_keys: bool,
    /// Idle period before an agent process is stopped.
    pub agent_idle_timeout: Duration,
    /// How helper processes (memory / delegate MCP servers, API provider agent) are launched (§9.9).
    pub helpers: HelperLaunch,
    /// Broadcast capacity of the event stream.
    pub event_capacity: usize,
    /// Apply `<home>/agents.json` in [`crate::Hub::start`] (spec §15.1 request 1). `from_env`: true.
    pub load_agents_file: bool,
    /// Take git tree snapshots around turns for `turn_diff` (spec §15.2). `from_env`: true unless
    /// `HARNESS_TURN_SNAPSHOTS=0`.
    pub turn_snapshots: bool,
    /// Per-agent default mode/model/config for fresh native sessions without an own choice (phase-9-parity.md
    /// §9.1.5, AGT-007). The UI fills it from settings `agents.<id>`; `from_env`: empty.
    pub agent_defaults: HashMap<AgentId, AgentDefaults>,
    /// CLI kind of an agent id for login presence (phase-9-parity.md §9.1.5a); ids not listed map by name
    /// (`claude-code`, `codex`, `agy`). `from_env`: empty.
    pub agent_kinds: HashMap<AgentId, AgentKind>,
    /// Login presence probe (never reads secrets). `from_env`: file/env existence.
    pub auth_probe: SharedAuthProbe,
    /// File-name search borrowed from the UI's workspace index for `@` mentions and the command center
    /// (phase-9-parity.md §9.2.1). `from_env`: none.
    pub file_search: Option<SharedFileSearch>,
    /// Where the CLIs keep slash command files (`list_commands`). `from_env`: `ConfigPaths::from_env`.
    pub config_paths: Option<harness_agent_config::ConfigPaths>,
    /// Read-only history sources for `import_scan` (decision 14A). `from_env`: `~/.zcode` and `~/.claude`.
    pub import_roots: harness_import::ImportRoots,
    /// Smallest `Interval` of a scheduled prompt in seconds. `from_env`: 300.
    pub schedule_min_interval_secs: u32,
    /// Credential store for Antigravity accounts. `from_env`: unavailable (the app injects the OS store).
    pub agy_creds: crate::agy_cred::SharedAgyCredStore,
    /// Subagent limit and nesting (settings `orchestra.maxConcurrent`, `orchestra.nestedSubagents`). The UI keeps
    /// it in step with settings while the hub runs. `from_env`: 4 at once, no nesting.
    pub delegation: DelegationPolicy,
    /// Fact check for deep research reports (docs/specs/harness/deep-research.md, section "Fact check"). The UI
    /// keeps it in step with settings `research.factCheck`. `from_env`: on.
    pub fact_check: FactCheckPolicy,
}

/// Live fact-check switch shared between the UI and the hub.
#[derive(Debug, Clone)]
pub struct FactCheckPolicy {
    on: Arc<AtomicBool>,
}

impl Default for FactCheckPolicy {
    fn default() -> Self {
        Self {
            on: Arc::new(AtomicBool::new(true)),
        }
    }
}

impl FactCheckPolicy {
    #[must_use]
    pub fn on(&self) -> bool {
        self.on.load(Ordering::Relaxed)
    }

    pub fn set_on(&self, value: bool) {
        self.on.store(value, Ordering::Relaxed);
    }
}

/// Live delegation settings shared between the UI and the hub.
#[derive(Debug, Clone)]
pub struct DelegationPolicy {
    limit: Arc<AtomicUsize>,
    nested_limit: Arc<AtomicUsize>,
    nested: Arc<AtomicBool>,
}

impl Default for DelegationPolicy {
    fn default() -> Self {
        Self {
            limit: Arc::new(AtomicUsize::new(4)),
            nested_limit: Arc::new(AtomicUsize::new(4)),
            nested: Arc::new(AtomicBool::new(false)),
        }
    }
}

impl DelegationPolicy {
    /// Direct subagents that may run at once under one top-level session (at least 1); nested ones do not count.
    #[must_use]
    pub fn limit(&self) -> usize {
        self.limit.load(Ordering::Relaxed).max(1)
    }

    pub fn set_limit(&self, value: usize) {
        self.limit.store(value.max(1), Ordering::Relaxed);
    }

    /// Subagents that one subagent may run at once under itself (its own quota, at least 1).
    #[must_use]
    pub fn nested_limit(&self) -> usize {
        self.nested_limit.load(Ordering::Relaxed).max(1)
    }

    pub fn set_nested_limit(&self, value: usize) {
        self.nested_limit.store(value.max(1), Ordering::Relaxed);
    }

    /// The quota that applies to `parent`'s own subagents.
    #[must_use]
    pub fn limit_for(&self, parent_is_subagent: bool) -> usize {
        if parent_is_subagent {
            self.nested_limit()
        } else {
            self.limit()
        }
    }

    /// Whether a subagent may delegate to subagents of its own.
    #[must_use]
    pub fn nested(&self) -> bool {
        self.nested.load(Ordering::Relaxed)
    }

    pub fn set_nested(&self, value: bool) {
        self.nested.store(value, Ordering::Relaxed);
    }
}

/// Shared [`FileSearchSource`] (`HubConfig` stays `Debug`/`Clone`).
#[derive(Clone)]
pub struct SharedFileSearch(pub Arc<dyn FileSearchSource>);

impl std::fmt::Debug for SharedFileSearch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SharedFileSearch")
    }
}

/// Shared [`AuthProbe`] (gates inject a static one).
#[derive(Clone)]
pub struct SharedAuthProbe(pub Arc<dyn AuthProbe>);

impl Default for SharedAuthProbe {
    fn default() -> Self {
        Self(Arc::new(FileExistenceProbe))
    }
}

impl std::fmt::Debug for SharedAuthProbe {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SharedAuthProbe")
    }
}

impl HubConfig {
    /// Configuration from the environment: `HARNESS_HOME` (else `~/.harness`), `HARNESS_AGENT_IDLE_SECS`,
    /// helpers per [`HelperLaunch::from_env`] (standalone binaries; the app replaces them with itself, §9.9).
    ///
    /// # Errors
    ///
    /// When no home directory or helper path can be resolved.
    pub fn from_env() -> Result<Self, HubError> {
        let home = match std::env::var_os("HARNESS_HOME") {
            Some(home) if !home.is_empty() => PathBuf::from(home),
            _ => user_home()
                .ok_or_else(|| HubError::Config("no home directory (set HARNESS_HOME)".into()))?
                .join(".harness"),
        };
        let agent_idle_timeout = std::env::var("HARNESS_AGENT_IDLE_SECS")
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok())
            .map_or(DEFAULT_AGENT_IDLE, Duration::from_secs);
        let helpers = HelperLaunch::from_env()?;
        let agents = builtin_agents();
        let home_for_paths = home.clone();
        let default_fallback_order = agents.iter().map(|a| a.id.clone()).collect();
        Ok(Self {
            home,
            agents,
            default_fallback_order,
            key_pools: Vec::new(),
            rotate_keys: !std::env::var("HARNESS_KEY_ROTATION").is_ok_and(|v| {
                matches!(
                    v.trim().to_ascii_lowercase().as_str(),
                    "off" | "0" | "false"
                )
            }),
            agent_idle_timeout,
            helpers,
            event_capacity: 4096,
            load_agents_file: true,
            turn_snapshots: std::env::var("HARNESS_TURN_SNAPSHOTS")
                .map_or(true, |v| v.trim() != "0"),
            agent_defaults: HashMap::new(),
            agent_kinds: HashMap::new(),
            auth_probe: SharedAuthProbe::default(),
            file_search: None,
            config_paths: harness_agent_config::ConfigPaths::from_env(&home_for_paths, None).ok(),
            import_roots: harness_import::ImportRoots::from_env(),
            schedule_min_interval_secs: 300,
            agy_creds: crate::agy_cred::SharedAgyCredStore::default(),
            delegation: DelegationPolicy::default(),
            fact_check: FactCheckPolicy::default(),
        })
    }

    /// Path of the optional agent configuration file.
    #[must_use]
    pub fn agents_file(&self) -> PathBuf {
        self.home.join(AGENTS_FILE)
    }

    /// Applies `<home>/agents.json` when it exists (spec §15.1 request 1): an entry with a known id replaces
    /// that agent in place, a new id is appended to the agents and to the default fallback order;
    /// `replaceBuiltins: true` drops the previous list first.
    ///
    /// # Errors
    ///
    /// Unreadable or invalid file (the hub must not start with a half-read configuration).
    pub async fn apply_agents_file(&mut self) -> Result<(), HubError> {
        let path = self.agents_file();
        let bytes = match tokio::fs::read(&path).await {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(HubError::Config(format!("{}: {e}", path.display()))),
        };
        let file: AgentsFile = serde_json::from_slice(&bytes)
            .map_err(|e| HubError::Config(format!("{}: {e}", path.display())))?;
        let (replace, entries) = match file {
            AgentsFile::List(entries) => (false, entries),
            AgentsFile::Object {
                replace_builtins,
                agents,
            } => (replace_builtins, agents),
        };
        if replace {
            self.agents.clear();
            self.default_fallback_order.clear();
        }
        for entry in entries {
            let spec = entry.into_spec(&path)?;
            if let Some(slot) = self.agents.iter_mut().find(|a| a.id == spec.id) {
                *slot = spec;
            } else {
                self.default_fallback_order.push(spec.id.clone());
                self.agents.push(spec);
            }
        }
        if self.agents.is_empty() {
            return Err(HubError::Config(format!("{}: no agents", path.display())));
        }
        Ok(())
    }

    /// Path of the SQLite store.
    #[must_use]
    pub fn db_path(&self) -> PathBuf {
        self.home.join("harness.sqlite")
    }
}

fn user_home() -> Option<PathBuf> {
    let keys: &[&str] = if cfg!(windows) {
        &["USERPROFILE", "HOME"]
    } else {
        &["HOME"]
    };
    keys.iter()
        .filter_map(std::env::var_os)
        .find(|v| !v.is_empty())
        .map(PathBuf::from)
}

/// `agents.json`: a plain list or `{ replaceBuiltins, agents }`.
#[derive(Deserialize)]
#[serde(untagged)]
enum AgentsFile {
    List(Vec<AgentEntry>),
    #[serde(rename_all = "camelCase")]
    Object {
        #[serde(default)]
        replace_builtins: bool,
        agents: Vec<AgentEntry>,
    },
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AgentEntry {
    id: String,
    display_name: Option<String>,
    program: String,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    env: std::collections::BTreeMap<String, String>,
    install_hint: Option<String>,
    dialect: Option<String>,
}

impl AgentEntry {
    fn into_spec(self, file: &std::path::Path) -> Result<AgentSpec, HubError> {
        let id = self.id.trim().to_owned();
        if id.is_empty() || self.program.trim().is_empty() {
            return Err(HubError::Config(format!(
                "{}: every agent needs a non-empty id and program",
                file.display()
            )));
        }
        let dialect = match self.dialect.as_deref() {
            None | Some("generic") => ErrorDialect::Generic,
            Some("claudeCode") => ErrorDialect::ClaudeCode,
            Some("codex") => ErrorDialect::Codex,
            Some("agy") => ErrorDialect::Agy,
            Some(other) => {
                return Err(HubError::Config(format!(
                    "{}: unknown dialect {other} for agent {id}",
                    file.display()
                )))
            }
        };
        Ok(AgentSpec {
            display_name: self.display_name.unwrap_or_else(|| id.clone()),
            id: AgentId::new(id),
            program: self.program,
            args: self.args,
            env: self.env.into_iter().collect(),
            install_hint: self
                .install_hint
                .unwrap_or_else(|| format!("configured in {AGENTS_FILE}")),
            transport: Transport::Acp,
            dialect,
        })
    }
}
