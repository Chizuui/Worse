//! One agent entry per Antigravity account, so the picker can pin a session to an account.
//!
//! agy reads a single login from the credential store, so an account agent swaps its login in before
//! its process starts. The base `agy` agent keeps using whichever account is active.

use std::sync::Arc;

use harness_agent_config::AgentKind;
use harness_core_api::{AgentId, AgentRuntimeState};

use crate::agy_accounts as accounts;
use crate::agy_cred::slot_target;
use crate::agy_ops::is_agy;
use crate::config::HubConfig;
use crate::hub::HubCtx;

const BASE_AGY: &str = "agy";
const PREFIX: &str = "agy-account-";

/// The account an agent is pinned to, if it is an account agent.
pub(crate) fn account_of(agent: &AgentId) -> Option<&str> {
    agent.as_str().strip_prefix(PREFIX).filter(|id| !id.is_empty())
}

/// Adds an agent per Antigravity account after the base agy agent.
pub(crate) async fn apply(config: &mut HubConfig) {
    if !config.agy_creds.0.available() {
        return;
    }
    let file = match accounts::load(&config.home).await {
        Ok(file) => file,
        Err(error) => {
            tracing::warn!(%error, "agy_accounts.json ignored");
            return;
        }
    };
    if file.accounts.is_empty() {
        return;
    }
    let Some(base) = config
        .agents
        .iter()
        .find(|a| a.id.as_str() == BASE_AGY)
        .cloned()
    else {
        return;
    };
    let mut position = config
        .default_fallback_order
        .iter()
        .position(|a| a.as_str() == BASE_AGY)
        .map_or(config.default_fallback_order.len(), |i| i + 1);
    for entry in &file.accounts {
        let id = AgentId::new(format!("{PREFIX}{}", entry.id));
        if config.agents.iter().any(|a| a.id == id) {
            continue;
        }
        let mut spec = base.clone();
        spec.id = id.clone();
        spec.display_name = format!("{} · {}", base.display_name, entry.name);
        config.agent_kinds.insert(id.clone(), AgentKind::Agy);
        config.default_fallback_order.insert(position, id);
        position += 1;
        config.agents.push(spec);
    }
}

/// Before an account agent starts: makes its account agy's live login. Fails when another agy process
/// is busy on a different account, since swapping would cut that turn off.
pub(crate) async fn ensure_account(ctx: &HubCtx, agent: &AgentId) -> Result<(), String> {
    let Some(account) = account_of(agent) else {
        return Ok(());
    };
    let account = account.to_owned();
    let store = Arc::clone(&ctx.config.agy_creds.0);
    if !store.available() {
        return Err("Antigravity accounts need Windows Credential Manager".into());
    }
    let _guard = ctx.agy.lock.lock().await;
    let home = ctx.config.home.clone();
    let mut file = accounts::load(&home).await.map_err(|e| e.to_string())?;
    file.normalize();
    let name = file
        .accounts
        .iter()
        .find(|a| a.id == account)
        .map(|a| a.name.clone())
        .ok_or_else(|| format!("Antigravity account {account} was removed"))?;
    if file.active.as_deref() == Some(account.as_str()) {
        return Ok(());
    }
    for other in ctx.specs.keys().filter(|id| is_agy(ctx, id) && *id != agent) {
        if ctx.pool.runtime_state(other).await == AgentRuntimeState::Busy {
            return Err(format!(
                "Antigravity is busy on another account; wait for it to finish before using {name}"
            ));
        }
    }
    let probe = Arc::clone(&store);
    let target = slot_target(&account);
    let has = tokio::task::spawn_blocking(move || probe.exists(&target))
        .await
        .map_err(|e| e.to_string())??;
    if !has {
        return Err(format!(
            "Antigravity account {name} has no saved login; sign in under Settings → Agents"
        ));
    }
    crate::agy_ops::stop_agy(ctx).await;
    let file = tokio::task::spawn_blocking(move || {
        accounts::use_account(store.as_ref(), &mut file, &account).map(|()| file)
    })
    .await
    .map_err(|e| e.to_string())??;
    accounts::save(&home, &file).await.map_err(|e| e.to_string())?;
    tracing::info!("agy: switched login for a pinned account agent");
    Ok(())
}
