//! Extra Claude Code accounts as agent profiles (phase-9-parity.md §9.6).
//!
//! Owner: the hub owns `<home>/accounts.json` and `<home>/accounts/<id>/`. Each account is an extra agent that runs
//! the `claude-code` adapter with `CLAUDE_CONFIG_DIR` pointing at its profile, so quota and failover work per
//! account with the existing machinery. Credentials are never opened: the login happens inside the CLI and
//! "signed in" is the existence of `.credentials.json`.

use std::path::{Path, PathBuf};

use harness_agent_config::{AgentKind, AuthState, ConfigPaths};
use harness_core_api::{AccountInfo, AddAccount, AgentId, ApiError, AuthPresence};
use serde::{Deserialize, Serialize};

use crate::agents::AgentSpec;
use crate::config::HubConfig;

/// File name of the account list inside the Harness home.
pub const ACCOUNTS_FILE: &str = "accounts.json";
/// Directory holding one profile per account.
pub const ACCOUNTS_DIR: &str = "accounts";
/// Env var Claude Code reads its config dir from.
pub const CLAUDE_CONFIG_ENV: &str = "CLAUDE_CONFIG_DIR";
const BASE_CLAUDE: &str = "claude-code";
const LABEL_MAX: usize = 40;
const SLUG_MAX: usize = 24;
/// Shared from the main `~/.claude` into every profile (linked, so installs show up everywhere).
const SHARED_DIRS: &[&str] = &[
    "skills",
    "agents",
    "commands",
    "plugins",
    "output-styles",
    "hooks",
];
/// Copied into the profile when the main file is newer.
const SHARED_FILES: &[&str] = &["settings.json", "CLAUDE.md"];

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub(crate) struct AccountsFile {
    #[serde(default)]
    pub claude: Vec<AccountEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct AccountEntry {
    pub id: String,
    pub label: String,
}

pub(crate) fn profile_dir(home: &Path, id: &str) -> PathBuf {
    home.join(ACCOUNTS_DIR).join(id)
}

pub(crate) async fn load(home: &Path) -> Result<AccountsFile, ApiError> {
    let path = home.join(ACCOUNTS_FILE);
    match tokio::fs::read(&path).await {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map_err(|e| ApiError::Storage(format!("{}: {e}", path.display()))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(AccountsFile::default()),
        Err(e) => Err(ApiError::Storage(format!("{}: {e}", path.display()))),
    }
}

async fn save(home: &Path, file: &AccountsFile) -> Result<(), ApiError> {
    let path = home.join(ACCOUNTS_FILE);
    let tmp = home.join(format!("{ACCOUNTS_FILE}.tmp"));
    let bytes = serde_json::to_vec_pretty(file).map_err(|e| ApiError::Internal(e.to_string()))?;
    let storage = |e: std::io::Error| ApiError::Storage(format!("{}: {e}", path.display()));
    tokio::fs::create_dir_all(home).await.map_err(storage)?;
    tokio::fs::write(&tmp, bytes).await.map_err(storage)?;
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
        "account".into()
    } else {
        out
    }
}

/// Adds the accounts of `<home>/accounts.json` to the agents (after the last Claude Code entry of the default
/// order) and refreshes their profiles. A broken file is logged and skipped: the main agents must still start.
pub(crate) async fn apply(config: &mut HubConfig) {
    let file = match load(&config.home).await {
        Ok(file) => file,
        Err(error) => {
            tracing::warn!(%error, "accounts.json ignored");
            return;
        }
    };
    if file.claude.is_empty() {
        return;
    }
    let Some(base) = config
        .agents
        .iter()
        .find(|a| a.id.as_str() == BASE_CLAUDE)
        .cloned()
    else {
        tracing::warn!("accounts.json: no claude-code agent to base accounts on");
        return;
    };
    let main_dir = main_claude_dir(&config.home);
    for entry in file.claude {
        let id = AgentId::new(entry.id.clone());
        if config.agents.iter().any(|a| a.id == id) {
            continue;
        }
        let dir = profile_dir(&config.home, &entry.id);
        if let Some(main) = &main_dir {
            prepare_profile(main, &dir).await;
        }
        let mut spec: AgentSpec = base.clone();
        spec.id = id.clone();
        spec.display_name = format!("{} · {}", base.display_name, entry.label);
        spec.env.retain(|(k, _)| k != CLAUDE_CONFIG_ENV);
        spec.env
            .push((CLAUDE_CONFIG_ENV.into(), dir.to_string_lossy().into_owned()));
        config.agent_kinds.insert(id.clone(), AgentKind::ClaudeCode);
        let position = config
            .default_fallback_order
            .iter()
            .rposition(|a| is_claude(config, a))
            .map_or(config.default_fallback_order.len(), |i| i + 1);
        config.default_fallback_order.insert(position, id);
        config.agents.push(spec);
    }
}

pub(crate) fn is_claude(config: &HubConfig, agent: &AgentId) -> bool {
    agent.as_str() == BASE_CLAUDE || config.agent_kinds.get(agent) == Some(&AgentKind::ClaudeCode)
}

fn main_claude_dir(home: &Path) -> Option<PathBuf> {
    ConfigPaths::from_env(home, None).ok().map(|p| p.claude_dir)
}

/// Creates the profile and shares the main config into it (never credentials or `.claude.json`).
pub(crate) async fn prepare_profile(main: &Path, profile: &Path) {
    if main == profile {
        return;
    }
    if let Err(error) = tokio::fs::create_dir_all(profile).await {
        tracing::warn!(%error, profile = %profile.display(), "account profile not created");
        return;
    }
    for name in SHARED_DIRS {
        let src = main.join(name);
        let dst = profile.join(name);
        let src_is_dir = tokio::fs::metadata(&src).await.is_ok_and(|m| m.is_dir());
        if !src_is_dir || tokio::fs::symlink_metadata(&dst).await.is_ok() {
            continue;
        }
        if let Err(error) = link_dir(&src, &dst).await {
            tracing::warn!(%error, dir = %dst.display(), "account profile link failed");
        }
    }
    for name in SHARED_FILES {
        let src = main.join(name);
        let dst = profile.join(name);
        let Ok(src_meta) = tokio::fs::metadata(&src).await else {
            continue;
        };
        let newer = match tokio::fs::metadata(&dst).await {
            Ok(dst_meta) => match (src_meta.modified(), dst_meta.modified()) {
                (Ok(s), Ok(d)) => s > d,
                _ => true,
            },
            Err(_) => true,
        };
        if newer {
            if let Err(error) = tokio::fs::copy(&src, &dst).await {
                tracing::warn!(%error, file = %dst.display(), "account profile copy failed");
            }
        }
    }
}

#[cfg(windows)]
async fn link_dir(src: &Path, dst: &Path) -> std::io::Result<()> {
    // Junctions need no admin rights or developer mode (unlike directory symlinks).
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let status = tokio::process::Command::new("cmd")
        .arg("/C")
        .arg("mklink")
        .arg("/J")
        .arg(dst)
        .arg(src)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .creation_flags(CREATE_NO_WINDOW)
        .status()
        .await?;
    if status.success() {
        Ok(())
    } else {
        Err(std::io::Error::other(format!(
            "mklink /J exited with {status}"
        )))
    }
}

#[cfg(unix)]
async fn link_dir(src: &Path, dst: &Path) -> std::io::Result<()> {
    tokio::fs::symlink(src, dst).await
}

#[cfg(not(any(windows, unix)))]
async fn link_dir(_src: &Path, _dst: &Path) -> std::io::Result<()> {
    Err(std::io::Error::other(
        "links not supported on this platform",
    ))
}

async fn auth_of(probe: &crate::config::SharedAuthProbe, home: &Path, dir: &Path) -> AuthPresence {
    let Some(mut paths) = ConfigPaths::from_env(home, None).ok() else {
        return AuthPresence::Unknown;
    };
    paths.claude_json = dir.join(".claude.json");
    paths.claude_dir = dir.to_path_buf();
    match probe.0.probe(AgentKind::ClaudeCode, &paths).await {
        AuthState::Present => AuthPresence::Present,
        AuthState::Absent => AuthPresence::Absent,
        AuthState::Unknown => AuthPresence::Unknown,
    }
}

/// Login presence of an agent whose spec sets `CLAUDE_CONFIG_DIR` (account agents), else `None`.
pub(crate) async fn auth_for_spec(config: &HubConfig, spec: &AgentSpec) -> Option<AuthPresence> {
    let (_, dir) = spec.env.iter().find(|(k, _)| k == CLAUDE_CONFIG_ENV)?;
    Some(auth_of(&config.auth_probe, &config.home, Path::new(dir)).await)
}

pub(crate) async fn list(
    config: &HubConfig,
    loaded: &(dyn Fn(&AgentId) -> bool + Sync),
) -> Result<Vec<AccountInfo>, ApiError> {
    let file = load(&config.home).await?;
    let mut out = Vec::with_capacity(file.claude.len());
    for entry in file.claude {
        out.push(info(config, entry, loaded).await);
    }
    Ok(out)
}

async fn info(
    config: &HubConfig,
    entry: AccountEntry,
    loaded: &(dyn Fn(&AgentId) -> bool + Sync),
) -> AccountInfo {
    let id = AgentId::new(entry.id.clone());
    let dir = profile_dir(&config.home, &entry.id);
    AccountInfo {
        loaded: loaded(&id),
        auth: auth_of(&config.auth_probe, &config.home, &dir).await,
        profile_dir: dir.to_string_lossy().into_owned(),
        base: AgentId::new(BASE_CLAUDE),
        label: entry.label,
        id,
    }
}

pub(crate) async fn add(
    config: &HubConfig,
    request: AddAccount,
    loaded: &(dyn Fn(&AgentId) -> bool + Sync),
) -> Result<AccountInfo, ApiError> {
    if request.base.as_str() != BASE_CLAUDE {
        return Err(ApiError::InvalidArgument(format!(
            "accounts are only supported for {BASE_CLAUDE}"
        )));
    }
    let label = request.label.trim().to_owned();
    if label.is_empty() || label.chars().count() > LABEL_MAX {
        return Err(ApiError::InvalidArgument(format!(
            "label must be 1-{LABEL_MAX} characters"
        )));
    }
    let mut file = load(&config.home).await?;
    let taken = |id: &str| {
        file.claude.iter().any(|e| e.id == id) || config.agents.iter().any(|a| a.id.as_str() == id)
    };
    let stem = format!("{BASE_CLAUDE}-{}", slug(&label));
    let mut id = stem.clone();
    let mut n = 2;
    while taken(&id) {
        id = format!("{stem}-{n}");
        n += 1;
    }
    let entry = AccountEntry {
        id: id.clone(),
        label,
    };
    file.claude.push(entry.clone());
    save(&config.home, &file).await?;
    let dir = profile_dir(&config.home, &id);
    match main_claude_dir(&config.home) {
        Some(main) => prepare_profile(&main, &dir).await,
        None => {
            tokio::fs::create_dir_all(&dir)
                .await
                .map_err(|e| ApiError::Storage(format!("{}: {e}", dir.display())))?;
        }
    }
    Ok(info(config, entry, loaded).await)
}

pub(crate) async fn remove(config: &HubConfig, id: &AgentId) -> Result<(), ApiError> {
    let mut file = load(&config.home).await?;
    let before = file.claude.len();
    file.claude.retain(|e| e.id != id.as_str());
    if file.claude.len() == before {
        return Err(ApiError::NotFound(format!("account {id}")));
    }
    save(&config.home, &file).await
}
