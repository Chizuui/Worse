//! Custom OpenAI-compatible providers as agents (phase-9-parity.md §9.7), and API keys of the Command Code CLI.
//!
//! Owner: the hub owns `<home>/providers.json` (the only place the API key is stored). Each provider becomes an
//! agent that runs Harness's own ACP agent `harness-api-agent`; the key reaches that process only through the env
//! var `HARNESS_PROVIDER_API_KEY`. The key is never returned by the API or logged.
//!
//! An entry of kind `command-code` is one API key of the Command Code CLI instead: it becomes an agent that
//! runs `command-code acp` with `COMMAND_CODE_API_KEY` set (the CLI's documented headless login), next to the
//! built-in Command Code agent, which uses the CLI's own login.
//!
//! Key pools: entries that stand for the same service with different keys (all Command Code keys; providers
//! with the same endpoint and model) form a pool. A new session asked for a pool member takes the members in
//! turn (round robin, see `Hub::rotated`), so sessions and delegated subagents spread over the keys; failover
//! on a usage limit walks the same agents, as for any other agent.

use std::path::{Path, PathBuf};

use harness_core_api::{AddProvider, AgentId, ApiError, ProviderInfo, UpdateProvider};
use serde::{Deserialize, Serialize};

use crate::agents::{AgentSpec, ErrorDialect, Transport, COMMAND_CODE};
use crate::config::HubConfig;

/// File name of the provider list inside the Harness home.
pub const PROVIDERS_FILE: &str = "providers.json";
/// Directory holding per-provider agent state (sessions).
pub const PROVIDERS_DIR: &str = "providers";
/// Env vars read by `harness-api-agent`.
pub const ENV_BASE_URL: &str = "HARNESS_PROVIDER_BASE_URL";
pub const ENV_MODEL: &str = "HARNESS_PROVIDER_MODEL";
pub const ENV_API_KEY: &str = "HARNESS_PROVIDER_API_KEY";
pub const ENV_LABEL: &str = "HARNESS_PROVIDER_LABEL";
pub const ENV_STATE: &str = "HARNESS_PROVIDER_STATE";
/// Kind of an entry that is one API key of the Command Code CLI.
pub const KIND_COMMAND_CODE: &str = "command-code";
/// Env var the Command Code CLI reads its API key from (its documented headless login).
pub const ENV_COMMAND_CODE_KEY: &str = "COMMAND_CODE_API_KEY";
const ID_PREFIX: &str = "api-";
/// Id prefix of Command Code key agents (the name also picks the Command Code logo in the UI).
const COMMAND_CODE_PREFIX: &str = "command-code-";
const LABEL_MAX: usize = 40;
const SLUG_MAX: usize = 24;

#[derive(Default, Clone, Serialize, Deserialize)]
pub(crate) struct ProvidersFile {
    #[serde(default)]
    pub providers: Vec<ProviderEntry>,
    /// `"off"` turns the round robin over key pools off (absent: on).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rotation: Option<String>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ProviderEntry {
    pub id: String,
    pub label: String,
    pub base_url: String,
    pub model: String,
    #[serde(default)]
    pub api_key: String,
    /// `""` = OpenAI-compatible endpoint; `"command-code"` = a Command Code CLI key.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub kind: String,
}

impl ProviderEntry {
    fn is_command_code(&self) -> bool {
        self.kind == KIND_COMMAND_CODE
    }
}

pub(crate) fn state_dir(home: &Path, id: &str) -> PathBuf {
    home.join(PROVIDERS_DIR).join(id)
}

pub(crate) async fn load(home: &Path) -> Result<ProvidersFile, ApiError> {
    let path = home.join(PROVIDERS_FILE);
    match tokio::fs::read(&path).await {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map_err(|e| ApiError::Storage(format!("{}: {e}", path.display()))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(ProvidersFile::default()),
        Err(e) => Err(ApiError::Storage(format!("{}: {e}", path.display()))),
    }
}

async fn save(home: &Path, file: &ProvidersFile) -> Result<(), ApiError> {
    let path = home.join(PROVIDERS_FILE);
    let tmp = home.join(format!("{PROVIDERS_FILE}.tmp"));
    let bytes = serde_json::to_vec_pretty(file).map_err(|e| ApiError::Internal(e.to_string()))?;
    let storage = |e: std::io::Error| ApiError::Storage(format!("{}: {e}", path.display()));
    tokio::fs::create_dir_all(home).await.map_err(storage)?;
    tokio::fs::write(&tmp, bytes).await.map_err(storage)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        tokio::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))
            .await
            .map_err(storage)?;
    }
    tokio::fs::rename(&tmp, &path).await.map_err(storage)
}

fn slug(label: &str) -> String {
    let mut out = String::new();
    for c in label.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else if !out.ends_with('-') && !out.is_empty() {
            out.push('-');
        }
        if out.len() >= SLUG_MAX {
            break;
        }
    }
    let out = out.trim_end_matches('-').to_owned();
    if out.is_empty() {
        "provider".into()
    } else {
        out
    }
}

fn clean_label(label: &str) -> Result<String, ApiError> {
    let label = label.trim().to_owned();
    if label.is_empty() || label.chars().count() > LABEL_MAX {
        return Err(ApiError::InvalidArgument(format!(
            "name must be 1-{LABEL_MAX} characters"
        )));
    }
    Ok(label)
}

/// Normalises a base URL: scheme check, trailing `/` and `/chat/completions` removed.
fn clean_base_url(url: &str) -> Result<String, ApiError> {
    let mut url = url.trim().to_owned();
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return Err(ApiError::InvalidArgument(
            "endpoint must start with http:// or https://".into(),
        ));
    }
    while url.ends_with('/') {
        url.pop();
    }
    if let Some(stripped) = url.strip_suffix("/chat/completions") {
        url = stripped.trim_end_matches('/').to_owned();
    }
    if url.len() <= "https://".len() {
        return Err(ApiError::InvalidArgument("endpoint has no host".into()));
    }
    Ok(url)
}

fn clean_model(model: &str) -> Result<String, ApiError> {
    let model = model.trim().to_owned();
    if model.is_empty() {
        return Err(ApiError::InvalidArgument("model must not be empty".into()));
    }
    Ok(model)
}

/// Agent of one Command Code key: the built-in Command Code agent with the key in its environment.
fn command_code_spec(config: &HubConfig, entry: &ProviderEntry) -> AgentSpec {
    let mut spec = config
        .agents
        .iter()
        .find(|a| a.id.as_str() == COMMAND_CODE)
        .cloned()
        .or_else(|| {
            crate::agents::builtin_agents()
                .into_iter()
                .find(|a| a.id.as_str() == COMMAND_CODE)
        })
        .unwrap_or_else(|| AgentSpec {
            id: AgentId::new(COMMAND_CODE),
            display_name: "Command Code".into(),
            program: "command-code".into(),
            args: vec!["acp".into()],
            env: Vec::new(),
            install_hint: "install Command Code: npm i -g command-code".into(),
            transport: Transport::Acp,
            dialect: ErrorDialect::Generic,
        });
    spec.id = AgentId::new(entry.id.clone());
    spec.display_name = format!("Command Code · {}", entry.label);
    spec.env.retain(|(k, _)| k != ENV_COMMAND_CODE_KEY);
    spec.env
        .push((ENV_COMMAND_CODE_KEY.into(), entry.api_key.clone()));
    spec
}

fn spec_of(config: &HubConfig, entry: &ProviderEntry) -> AgentSpec {
    if entry.is_command_code() {
        return command_code_spec(config, entry);
    }
    let state = state_dir(&config.home, &entry.id);
    // §9.9：程序与参数只由 HelperLaunch 决定（产品里是 harness-slint --api-agent）。
    let helper = crate::helpers::Helper::ApiAgent;
    AgentSpec {
        id: AgentId::new(entry.id.clone()),
        display_name: format!("{} (API)", entry.label),
        program: config
            .helpers
            .program(helper)
            .to_string_lossy()
            .into_owned(),
        args: config.helpers.args(helper),
        env: vec![
            (ENV_BASE_URL.into(), entry.base_url.clone()),
            (ENV_MODEL.into(), entry.model.clone()),
            (ENV_API_KEY.into(), entry.api_key.clone()),
            (ENV_LABEL.into(), entry.label.clone()),
            (ENV_STATE.into(), state.to_string_lossy().into_owned()),
        ],
        install_hint:
            "the Harness executable that runs the API agent is missing; reinstall Harness".into(),
        transport: Transport::Acp,
        dialect: ErrorDialect::Generic,
    }
}

/// Adds the providers of `<home>/providers.json` to the agents, at the end of the default order. A broken file
/// is logged (without content) and skipped: the other agents must still start.
pub(crate) async fn apply(config: &mut HubConfig) {
    let file = match load(&config.home).await {
        Ok(file) => file,
        Err(error) => {
            tracing::warn!(%error, "providers.json ignored");
            return;
        }
    };
    for entry in &file.providers {
        let id = AgentId::new(entry.id.clone());
        if config.agents.iter().any(|a| a.id == id) {
            continue;
        }
        let spec = spec_of(config, entry);
        if entry.is_command_code() {
            // Next to the other Command Code agents, so failover tries the next key before another agent.
            let position = config
                .default_fallback_order
                .iter()
                .rposition(|a| is_command_code_agent(a.as_str()))
                .map_or(config.default_fallback_order.len(), |i| i + 1);
            config.default_fallback_order.insert(position, id);
        } else {
            config.default_fallback_order.push(id);
        }
        config.agents.push(spec);
    }
    config.key_pools = pools(&file.providers);
    if file.rotation.as_deref().is_some_and(|r| r.eq_ignore_ascii_case("off")) {
        config.rotate_keys = false;
    }
}

fn is_command_code_agent(id: &str) -> bool {
    id == COMMAND_CODE || id.starts_with(COMMAND_CODE_PREFIX)
}

/// Key pools of the entries: all Command Code keys (with the built-in agent as the name of the pool, so a
/// session asked for "Command Code" rotates over the keys), and every group of two or more providers that
/// share endpoint and model.
pub(crate) fn pools(entries: &[ProviderEntry]) -> Vec<Vec<AgentId>> {
    let mut out = Vec::new();
    let keys: Vec<AgentId> = entries
        .iter()
        .filter(|e| e.is_command_code() && !e.api_key.trim().is_empty())
        .map(|e| AgentId::new(e.id.clone()))
        .collect();
    if !keys.is_empty() {
        out.push(keys);
    }
    let mut seen: Vec<(&str, &str)> = Vec::new();
    for entry in entries.iter().filter(|e| !e.is_command_code()) {
        let key = (entry.base_url.as_str(), entry.model.as_str());
        if seen.contains(&key) {
            continue;
        }
        seen.push(key);
        let group: Vec<AgentId> = entries
            .iter()
            .filter(|e| !e.is_command_code() && (e.base_url.as_str(), e.model.as_str()) == key)
            .map(|e| AgentId::new(e.id.clone()))
            .collect();
        if group.len() > 1 {
            out.push(group);
        }
    }
    out
}

/// The pool a session asked for `agent` rotates over, if any. The built-in Command Code agent stands for the
/// pool of Command Code keys.
pub(crate) fn pool_of<'a>(pools: &'a [Vec<AgentId>], agent: &AgentId) -> Option<(usize, &'a [AgentId])> {
    pools.iter().enumerate().find_map(|(i, pool)| {
        let member = pool.contains(agent)
            || (agent.as_str() == COMMAND_CODE && pool.iter().all(|a| is_command_code_agent(a.as_str())));
        member.then_some((i, pool.as_slice()))
    })
}

/// True when the agent is a provider agent (its env carries the provider base URL).
pub(crate) fn is_provider(spec: &AgentSpec) -> bool {
    spec.env.iter().any(|(k, _)| k == ENV_BASE_URL)
}

fn info(entry: &ProviderEntry, loaded: &(dyn Fn(&AgentId) -> bool + Sync)) -> ProviderInfo {
    let id = AgentId::new(entry.id.clone());
    ProviderInfo {
        loaded: loaded(&id),
        id,
        label: entry.label.clone(),
        base_url: entry.base_url.clone(),
        model: entry.model.clone(),
        has_key: !entry.api_key.trim().is_empty(),
        kind: entry.kind.clone(),
    }
}

pub(crate) async fn list(
    config: &HubConfig,
    loaded: &(dyn Fn(&AgentId) -> bool + Sync),
) -> Result<Vec<ProviderInfo>, ApiError> {
    let file = load(&config.home).await?;
    Ok(file.providers.iter().map(|e| info(e, loaded)).collect())
}

pub(crate) async fn add(
    config: &HubConfig,
    request: AddProvider,
    loaded: &(dyn Fn(&AgentId) -> bool + Sync),
) -> Result<ProviderInfo, ApiError> {
    let label = clean_label(&request.label)?;
    let command_code = match request.kind.trim() {
        "" => false,
        KIND_COMMAND_CODE => true,
        other => {
            return Err(ApiError::InvalidArgument(format!("unknown provider kind {other}")));
        }
    };
    // A Command Code key has no endpoint or model of its own: the CLI knows both.
    let (base_url, model) = if command_code {
        if request.api_key.trim().is_empty() {
            return Err(ApiError::InvalidArgument("the API key must not be empty".into()));
        }
        (String::new(), String::new())
    } else {
        (clean_base_url(&request.base_url)?, clean_model(&request.model)?)
    };
    let mut file = load(&config.home).await?;
    let taken = |id: &str| {
        file.providers.iter().any(|e| e.id == id)
            || config.agents.iter().any(|a| a.id.as_str() == id)
    };
    let prefix = if command_code { COMMAND_CODE_PREFIX } else { ID_PREFIX };
    let stem = format!("{prefix}{}", slug(&label));
    let mut id = stem.clone();
    let mut n = 2;
    while taken(&id) {
        id = format!("{stem}-{n}");
        n += 1;
    }
    let entry = ProviderEntry {
        id,
        label,
        base_url,
        model,
        api_key: request.api_key.trim().to_owned(),
        kind: if command_code { KIND_COMMAND_CODE.into() } else { String::new() },
    };
    file.providers.push(entry.clone());
    save(&config.home, &file).await?;
    Ok(info(&entry, loaded))
}

pub(crate) async fn update(
    config: &HubConfig,
    request: UpdateProvider,
    loaded: &(dyn Fn(&AgentId) -> bool + Sync),
) -> Result<ProviderInfo, ApiError> {
    let mut file = load(&config.home).await?;
    let Some(entry) = file
        .providers
        .iter_mut()
        .find(|e| e.id == request.id.as_str())
    else {
        return Err(ApiError::NotFound(format!("provider {}", request.id)));
    };
    if let Some(label) = &request.label {
        entry.label = clean_label(label)?;
    }
    // A Command Code key keeps its empty endpoint and model.
    if let Some(url) = request.base_url.as_ref().filter(|_| !entry.is_command_code()) {
        entry.base_url = clean_base_url(url)?;
    }
    if let Some(model) = request.model.as_ref().filter(|_| !entry.is_command_code()) {
        entry.model = clean_model(model)?;
    }
    if let Some(key) = &request.api_key {
        entry.api_key = key.trim().to_owned();
    }
    let out = info(entry, loaded);
    save(&config.home, &file).await?;
    Ok(out)
}

pub(crate) async fn remove(config: &HubConfig, id: &AgentId) -> Result<(), ApiError> {
    let mut file = load(&config.home).await?;
    let before = file.providers.len();
    file.providers.retain(|e| e.id != id.as_str());
    if file.providers.len() == before {
        return Err(ApiError::NotFound(format!("provider {id}")));
    }
    save(&config.home, &file).await?;
    let dir = state_dir(&config.home, id.as_str());
    if let Err(error) = tokio::fs::remove_dir_all(&dir).await {
        if error.kind() != std::io::ErrorKind::NotFound {
            tracing::warn!(%error, dir = %dir.display(), "provider state not removed");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: &str, base_url: &str, model: &str, kind: &str) -> ProviderEntry {
        ProviderEntry {
            id: id.into(),
            label: id.into(),
            base_url: base_url.into(),
            model: model.into(),
            api_key: "key".into(),
            kind: kind.into(),
        }
    }

    fn ids(pool: &[AgentId]) -> Vec<&str> {
        pool.iter().map(AgentId::as_str).collect()
    }

    #[test]
    fn pools_group_command_code_keys_and_equal_providers() {
        let entries = vec![
            entry("command-code-a", "", "", KIND_COMMAND_CODE),
            entry("api-one", "https://x/v1", "m", ""),
            entry("command-code-b", "", "", KIND_COMMAND_CODE),
            entry("api-two", "https://x/v1", "m", ""),
            entry("api-other-model", "https://x/v1", "other", ""),
            entry("api-alone", "https://y/v1", "m", ""),
        ];
        let pools = pools(&entries);
        assert_eq!(pools.len(), 2);
        assert_eq!(ids(&pools[0]), ["command-code-a", "command-code-b"]);
        assert_eq!(ids(&pools[1]), ["api-one", "api-two"]);
    }

    #[test]
    fn one_command_code_key_is_still_a_pool_but_one_provider_is_not() {
        // A single key matters: a session asked for the built-in agent then uses that key.
        let pools = pools(&[entry("command-code-a", "", "", KIND_COMMAND_CODE), entry("api-one", "https://x/v1", "m", "")]);
        assert_eq!(pools.len(), 1);
        assert_eq!(ids(&pools[0]), ["command-code-a"]);
    }

    #[test]
    fn keys_without_a_key_stay_out() {
        let mut empty = entry("command-code-a", "", "", KIND_COMMAND_CODE);
        empty.api_key = "  ".into();
        assert!(pools(&[empty]).is_empty());
    }

    #[test]
    fn the_built_in_agent_stands_for_the_command_code_pool() {
        let pools = pools(&[
            entry("command-code-a", "", "", KIND_COMMAND_CODE),
            entry("api-one", "https://x/v1", "m", ""),
            entry("api-two", "https://x/v1", "m", ""),
        ]);
        let of = |id: &str| pool_of(&pools, &AgentId::new(id)).map(|(i, _)| i);
        assert_eq!(of(COMMAND_CODE), Some(0));
        assert_eq!(of("command-code-a"), Some(0));
        assert_eq!(of("api-two"), Some(1));
        assert_eq!(of("claude-code"), None);
        assert_eq!(of("api-unknown"), None);
    }

    #[test]
    fn stored_file_keeps_old_entries_readable() {
        // Files written before `kind` and `rotation` existed.
        let file: ProvidersFile = serde_json::from_str(
            r#"{"providers":[{"id":"api-a","label":"A","baseUrl":"https://x/v1","model":"m","apiKey":"k"}]}"#,
        )
        .unwrap();
        assert_eq!(file.providers[0].kind, "");
        assert!(file.rotation.is_none());
        // And a plain provider is written back without the new fields.
        let text = serde_json::to_string(&file).unwrap();
        assert!(!text.contains("kind") && !text.contains("rotation"));
    }
}
