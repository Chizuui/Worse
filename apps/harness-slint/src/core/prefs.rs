//! Theme / language / agent defaults read from the harness-settings store (phase-9-parity.md
//! §9.1.2, R1/R6): `SettingsStore` is the only writer of `<data dir>/settings.json`; the cores only read it.

use std::collections::HashMap;

use harness_core_api::{AgentDefaults, AgentId, AgentOptions, ConfigOption};
use harness_settings::{Language, Settings as StoreSettings, ThemePref as StoreTheme};

use super::{Event, ThemePref};

/// UI language tag of the store value (`system`, `en-US`, `zh-CN`).
pub(crate) fn language_tag(language: Language) -> String {
    match language {
        Language::System => "system",
        Language::EnUs => "en-US",
        Language::ZhCn => "zh-CN",
    }
    .to_owned()
}

pub(crate) fn theme_of(settings: &StoreSettings) -> ThemePref {
    match settings.appearance.theme {
        StoreTheme::System => ThemePref::System,
        StoreTheme::Light => ThemePref::Light,
        StoreTheme::Dark => ThemePref::Dark,
    }
}

/// `Event::AgentOptions` from the hub's cached lists (R7). Efforts = values of the config option `effort`, else of
/// the first option whose ACP category is `thought_level` / `effort`.
pub(crate) fn agent_options_event(agent: String, options: &AgentOptions) -> Event {
    let effort = effort_option(&options.config.options);
    let model = if options.models.available.is_empty() {
        model_option(&options.config.options)
    } else {
        None
    };
    Event::AgentOptions {
        agent,
        modes: options
            .modes
            .available
            .iter()
            .map(|m| (m.id.clone(), m.name.clone()))
            .collect(),
        models: match model {
            Some(o) => o
                .values
                .iter()
                .map(|v| (v.value.clone(), v.name.clone()))
                .collect(),
            None => options
                .models
                .available
                .iter()
                .map(|m| (m.id.clone(), m.name.clone()))
                .collect(),
        },
        efforts: effort
            .map(|o| {
                o.values
                    .iter()
                    .map(|v| (v.value.clone(), v.name.clone()))
                    .collect()
            })
            .unwrap_or_default(),
        known: options.modes.known || options.models.known || options.config.known,
    }
}

/// The model option of agents that advertise their models only as a config option (newer Claude Code ACP: no
/// `models` in `session/new`, a `configOptions` entry with category `model`): id `model`, else category `model`.
pub(crate) fn model_option(options: &[ConfigOption]) -> Option<&ConfigOption> {
    options
        .iter()
        .find(|o| o.id == "model" && !o.values.is_empty())
        .or_else(|| {
            options
                .iter()
                .find(|o| o.category.as_deref() == Some("model") && !o.values.is_empty())
        })
}

/// The effort option: id `effort`, else the first option whose ACP category is `thought_level` / `effort`.
pub(crate) fn effort_option(options: &[ConfigOption]) -> Option<&ConfigOption> {
    options.iter().find(|o| o.id == "effort").or_else(|| {
        options
            .iter()
            .find(|o| matches!(o.category.as_deref(), Some("thought_level" | "effort")))
    })
}

/// Fixed lists of the fake core (same ids as `harness-fake-agent`).
pub(crate) fn fake_agent_options(agent: String) -> Event {
    let pairs = |ids: &[&str]| {
        ids.iter()
            .map(|id| ((*id).to_owned(), (*id).to_owned()))
            .collect()
    };
    Event::AgentOptions {
        agent,
        modes: pairs(&["default", "plan"]),
        models: pairs(&["fake-small", "fake-large"]),
        efforts: pairs(&["low", "medium", "high"]),
        known: true,
    }
}

/// `HubConfig.agent_defaults` from settings `agents.<id>` (AGT-007). The effort default is passed as config option
/// `effort`; an agent that names it differently refuses the call, which the hub logs and ignores.
pub(crate) fn agent_defaults(settings: &StoreSettings) -> HashMap<AgentId, AgentDefaults> {
    settings
        .agents
        .iter()
        .filter_map(|(id, prefs)| {
            let mut defaults = AgentDefaults {
                mode: prefs.default_mode.clone(),
                model: prefs.default_model.clone(),
                ..AgentDefaults::default()
            };
            if let Some(effort) = &prefs.default_effort {
                defaults.config.insert("effort".into(), effort.clone());
            }
            (defaults != AgentDefaults::default()).then(|| (AgentId::new(id.clone()), defaults))
        })
        .collect()
}
