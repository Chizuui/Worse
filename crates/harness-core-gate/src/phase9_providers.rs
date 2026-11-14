//! Custom API providers (phase-9-parity.md §9.7): add, key never returned, restart → agent at the end of the order,
//! update, remove. No provider is contacted: the agent process is never started here (see
//! `scripts/e2e/phase9-api-agent.mjs` for the agent itself).

use std::path::Path;

use harness_core_api::{AddProvider, AgentId, AuthPresence, HarnessCore, UpdateProvider};

use super::phase9::{hub_config, start};
use super::{check, step, Env, R};

const KEY: &str = "gate-secret-key-7f3a9";

pub(super) async fn run(env: &Env, base: &Path) -> R<()> {
    let home = base.join("providers-data");
    std::fs::create_dir_all(&home).map_err(|e| format!("mkdir: {e}"))?;
    add_phase(env, &home).await?;
    restart_phase(env, &home).await?;
    step("providers");
    Ok(())
}

/// Add + validation + key handling, before the provider is an agent.
async fn add_phase(env: &Env, home: &Path) -> R<()> {
    let gate = start(hub_config(env, home, false)).await?;
    let added = gate
        .hub
        .add_provider(AddProvider {
            label: "Mock API".into(),
            base_url: " http://127.0.0.1:9/v1/chat/completions/ ".into(),
            model: "mock-model".into(),
            api_key: KEY.into(),
        })
        .await
        .map_err(|e| format!("add_provider: {e}"))?;
    check(
        added.id.as_str() == "api-mock-api"
            && added.base_url == "http://127.0.0.1:9/v1"
            && added.has_key
            && !added.loaded,
        format!("new provider {added:?}"),
    )?;
    for (url, model) in [("ftp://x/v1", "m"), ("http://x/v1", " ")] {
        check(
            gate.hub
                .add_provider(AddProvider {
                    label: "Bad".into(),
                    base_url: url.into(),
                    model: model.into(),
                    api_key: String::new(),
                })
                .await
                .is_err(),
            format!("invalid provider accepted: {url} / {model:?}"),
        )?;
    }
    let listed = gate.hub.list_providers().await.map_err(|e| e.to_string())?;
    let json = serde_json::to_string(&listed).map_err(|e| e.to_string())?;
    check(
        listed.len() == 1 && !json.contains(KEY),
        "list_providers must not return the key",
    )?;
    check(
        std::fs::read_to_string(home.join("providers.json"))
            .unwrap_or_default()
            .contains(KEY),
        "providers.json must hold the key",
    )?;
    check(
        !home.join("agents.json").exists(),
        "agents.json must stay untouched",
    )?;
    gate.hub.shutdown().await.map_err(|e| e.to_string())
}

/// After a restart: provider agent present, update, remove.
async fn restart_phase(env: &Env, home: &Path) -> R<()> {
    let gate = start(hub_config(env, home, false)).await?;
    let order = gate
        .hub
        .get_fallback_order()
        .await
        .map_err(|e| e.to_string())?;
    check(
        order.last().map(AgentId::as_str) == Some("api-mock-api"),
        format!("provider must be last in the default order: {order:?}"),
    )?;
    let overview = gate
        .hub
        .agents_overview()
        .await
        .map_err(|e| e.to_string())?;
    let card = overview
        .iter()
        .find(|a| a.id.as_str() == "api-mock-api")
        .ok_or("provider agent missing after restart")?;
    check(
        card.display_name == "Mock API (API)" && card.auth == AuthPresence::Present,
        format!("provider card {card:?}"),
    )?;
    check(
        !format!("{overview:?}").contains(KEY),
        "agents overview leaks the key",
    )?;
    let updated = gate
        .hub
        .update_provider(UpdateProvider {
            id: AgentId::new("api-mock-api"),
            label: None,
            base_url: None,
            model: Some("other-model".into()),
            api_key: None,
        })
        .await
        .map_err(|e| format!("update: {e}"))?;
    check(
        updated.model == "other-model" && updated.has_key && updated.loaded,
        format!("updated {updated:?}"),
    )?;
    let cleared = gate
        .hub
        .update_provider(UpdateProvider {
            id: AgentId::new("api-mock-api"),
            label: None,
            base_url: None,
            model: None,
            api_key: Some(String::new()),
        })
        .await
        .map_err(|e| format!("clear key: {e}"))?;
    check(!cleared.has_key, "key not cleared")?;
    let state = home.join("providers").join("api-mock-api");
    std::fs::create_dir_all(state.join("sessions")).map_err(|e| e.to_string())?;
    gate.hub
        .remove_provider(AgentId::new("api-mock-api"))
        .await
        .map_err(|e| format!("remove: {e}"))?;
    check(!state.exists(), "provider state dir not removed")?;
    check(
        gate.hub
            .remove_provider(AgentId::new("api-mock-api"))
            .await
            .is_err(),
        "second remove must be NotFound",
    )?;
    gate.hub.shutdown().await.map_err(|e| e.to_string())
}
