//! Settings › Orchestra › Decision model (`harness_agent_hub::decision`): which API provider makes Harness's
//! routing choices. The choice lives in `<home>/decision.json` and is read by the hub each time, so it
//! applies at once.

use std::path::{Path, PathBuf};

use harness_agent_hub::decision::{self, Config};
use slint::{ComponentHandle, ModelRc, VecModel};

use crate::ui::{AppWindow, DecisionUi, SelectOption};

/// `(id, label, model)` of the API providers in `providers.json` that can answer a decision: the
/// OpenAI-compatible ones (a Command Code CLI key is an agent, not an endpoint).
fn providers(home: &Path) -> Vec<(String, String, String)> {
    let Some(file) = std::fs::read_to_string(home.join("providers.json"))
        .ok()
        .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
    else {
        return Vec::new();
    };
    file.get("providers")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter(|p| {
            p.get("kind")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("")
                != "command-code"
        })
        .filter_map(|p| {
            let text = |key: &str| {
                p.get(key)
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_owned)
            };
            Some((
                text("id")?,
                text("label")?,
                text("model").unwrap_or_default(),
            ))
        })
        .collect()
}

/// `OpenRouter · openrouter/auto`.
fn option_label(label: &str, model: &str) -> String {
    if model.is_empty() {
        label.to_owned()
    } else {
        format!("{label} · {model}")
    }
}

fn show(window: &AppWindow, home: &Path, note: Option<String>) {
    let ui = window.global::<DecisionUi>();
    let list = providers(home);
    let config = decision::load(home);
    let current = list.iter().find(|(id, _, _)| *id == config.provider);
    ui.set_providers(ModelRc::new(VecModel::from(
        list.iter()
            .map(|(id, label, model)| SelectOption {
                id: id.as_str().into(),
                label: option_label(label, model).into(),
            })
            .collect::<Vec<_>>(),
    )));
    ui.set_provider(
        if current.is_some() {
            config.provider.as_str()
        } else {
            ""
        }
        .into(),
    );
    ui.set_provider_label(
        current
            .map(|(_, label, model)| option_label(label, model))
            .unwrap_or_default()
            .into(),
    );
    ui.set_model(config.model.as_str().into());
    ui.set_delegation(config.delegation);
    let note = note.unwrap_or_else(|| {
        if config.active() && current.is_none() {
            format!(
                "The provider {} is no longer configured, so no decision model is used.",
                config.provider
            )
        } else {
            String::new()
        }
    });
    ui.set_note(note.into());
}

pub fn install(window: &AppWindow, home: &Path) {
    let home: PathBuf = home.to_path_buf();
    let ui = window.global::<DecisionUi>();
    let change = |window: &AppWindow, home: &Path, edit: &dyn Fn(&mut Config)| {
        let mut config = decision::load(home);
        edit(&mut config);
        let note = decision::save(home, &config)
            .err()
            .map(|error| format!("Could not save the decision model: {error}"));
        show(window, home, note);
    };

    let (weak, h) = (window.as_weak(), home.clone());
    ui.on_refresh(move || {
        if let Some(window) = weak.upgrade() {
            show(&window, &h, None);
        }
    });
    let (weak, h) = (window.as_weak(), home.clone());
    ui.on_set_provider(move |id| {
        if let Some(window) = weak.upgrade() {
            change(&window, &h, &|config| config.provider = id.to_string());
        }
    });
    let (weak, h) = (window.as_weak(), home.clone());
    ui.on_set_model(move |model| {
        if let Some(window) = weak.upgrade() {
            // Saved as typed; the field keeps the text, so only the file is written.
            let mut config = decision::load(&h);
            model.trim().clone_into(&mut config.model);
            if let Err(error) = decision::save(&h, &config) {
                window
                    .global::<DecisionUi>()
                    .set_note(format!("Could not save the decision model: {error}").into());
            }
        }
    });
    let (weak, h) = (window.as_weak(), home);
    ui.on_set_delegation(move |on| {
        if let Some(window) = weak.upgrade() {
            change(&window, &h, &|config| config.delegation = on);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::{option_label, providers};

    #[test]
    fn lists_endpoint_providers_only() {
        let home = std::env::temp_dir().join(format!("harness-decision-ui-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&home);
        let _ = std::fs::write(
            home.join("providers.json"),
            r#"{"providers":[
                {"id":"api-openrouter","label":"OpenRouter","baseUrl":"https://openrouter.ai/api/v1","model":"openrouter/auto","apiKey":"k"},
                {"id":"command-code-key","label":"Key","baseUrl":"","model":"","apiKey":"k","kind":"command-code"}]}"#,
        );
        assert_eq!(
            providers(&home),
            [(
                "api-openrouter".to_owned(),
                "OpenRouter".to_owned(),
                "openrouter/auto".to_owned()
            )]
        );
        assert_eq!(
            option_label("OpenRouter", "openrouter/auto"),
            "OpenRouter · openrouter/auto"
        );
        assert_eq!(option_label("Local", ""), "Local");
        let _ = std::fs::remove_dir_all(&home);
    }
}
