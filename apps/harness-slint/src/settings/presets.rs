//! Ready-made API providers for "Add provider": picking one fills the endpoint and a default model, so the
//! user only pastes a key. Every preset is an OpenAI-compatible Chat Completions endpoint (Harness's own API
//! agent speaks that); providers that serve Claude or their best models only on another API are noted.
//!
//! Endpoints follow each provider's documentation and the provider tables of hermes-agent
//! (`hermes_cli/auth.py`) and oh-my-pi (`packages/catalog/src/provider-models/openai-compat.ts`). Default
//! models are a starting point only: model ids change often, so the dialog lists the endpoint's own models
//! (`GET <endpoint>/models`) once a key is in.

use std::time::Duration;

use slint::{ComponentHandle, ModelRc, SharedString, VecModel};

use crate::ui::{AgentsUi, AppWindow, ProviderPreset};

pub(super) struct Preset {
    pub id: &'static str,
    pub name: &'static str,
    pub tag: &'static str,
    pub url: &'static str,
    pub model: &'static str,
    pub note: &'static str,
    pub key_url: &'static str,
    pub keyless: bool,
}

const fn api(
    id: &'static str,
    name: &'static str,
    url: &'static str,
    model: &'static str,
    key_url: &'static str,
    note: &'static str,
) -> Preset {
    Preset {
        id,
        name,
        tag: "API key",
        url,
        model,
        note,
        key_url,
        keyless: false,
    }
}

pub(super) const PRESETS: &[Preset] = &[
    api(
        "command-code-api",
        "Command Code API",
        "https://api.commandcode.ai/provider/v1",
        "deepseek/deepseek-v4-flash",
        "https://commandcode.ai/docs/provider",
        "Command Code's Provider API: the same key as the CLI (made in Command Code Studio), any plan except Go. Claude models are not served on this endpoint; open-weight and GPT models are.",
    ),
    api(
        "openrouter",
        "OpenRouter",
        "https://openrouter.ai/api/v1",
        "openrouter/auto",
        "https://openrouter.ai/keys",
        "One key for hundreds of models. openrouter/auto lets OpenRouter pick; list the models to choose one that supports tools.",
    ),
    api(
        "openai",
        "OpenAI",
        "https://api.openai.com/v1",
        "gpt-5.5",
        "https://platform.openai.com/api-keys",
        "",
    ),
    api(
        "deepseek",
        "DeepSeek",
        "https://api.deepseek.com/v1",
        "deepseek-chat",
        "https://platform.deepseek.com/api_keys",
        "",
    ),
    api(
        "gemini",
        "Gemini",
        "https://generativelanguage.googleapis.com/v1beta/openai",
        "gemini-3.8-flash",
        "https://aistudio.google.com/apikey",
        "Gemini's OpenAI-compatible endpoint, with a Google AI Studio key.",
    ),
    api(
        "xai",
        "xAI Grok",
        "https://api.x.ai/v1",
        "grok-4.7",
        "https://console.x.ai",
        "",
    ),
    api(
        "mistral",
        "Mistral",
        "https://api.mistral.ai/v1",
        "mistral-large-latest",
        "https://console.mistral.ai/api-keys",
        "",
    ),
    api(
        "groq",
        "Groq",
        "https://api.groq.com/openai/v1",
        "",
        "https://console.groq.com/keys",
        "Pick a model from the list once the key is in.",
    ),
    api(
        "moonshot",
        "Kimi (Moonshot)",
        "https://api.moonshot.ai/v1",
        "kimi-k3",
        "https://platform.moonshot.ai/console/api-keys",
        "International endpoint. Keys from the China platform use https://api.moonshot.cn/v1 (edit the endpoint).",
    ),
    api(
        "zai",
        "Z.ai GLM",
        "https://api.z.ai/api/paas/v4",
        "glm-5.3",
        "https://z.ai/manage-apikey/apikey-list",
        "",
    ),
    api(
        "qwen",
        "Qwen (Alibaba)",
        "https://dashscope-intl.aliyuncs.com/compatible-mode/v1",
        "",
        "https://modelstudio.console.alibabacloud.com/",
        "Model Studio, international region. Pick a model from the list once the key is in.",
    ),
    api(
        "minimax",
        "MiniMax",
        "https://api.minimax.io/v1",
        "",
        "https://platform.minimax.io/user-center/basic-information/interface-key",
        "Pick a model from the list once the key is in.",
    ),
    api(
        "together",
        "Together AI",
        "https://api.together.xyz/v1",
        "",
        "https://api.together.ai/settings/api-keys",
        "Pick a model from the list once the key is in.",
    ),
    api(
        "fireworks",
        "Fireworks",
        "https://api.fireworks.ai/inference/v1",
        "",
        "https://fireworks.ai/account/api-keys",
        "Pick a model from the list once the key is in.",
    ),
    api(
        "cerebras",
        "Cerebras",
        "https://api.cerebras.ai/v1",
        "",
        "https://cloud.cerebras.ai",
        "Pick a model from the list once the key is in.",
    ),
    api(
        "nvidia",
        "NVIDIA NIM",
        "https://integrate.api.nvidia.com/v1",
        "",
        "https://build.nvidia.com",
        "Pick a model from the list once the key is in.",
    ),
    api(
        "huggingface",
        "Hugging Face",
        "https://router.huggingface.co/v1",
        "",
        "https://huggingface.co/settings/tokens",
        "Inference Providers router; a token with the \"Make calls to Inference Providers\" permission.",
    ),
    api(
        "vercel",
        "Vercel AI Gateway",
        "https://ai-gateway.vercel.sh/v1",
        "",
        "https://vercel.com/dashboard/ai-gateway",
        "Pick a model from the list once the key is in.",
    ),
    api(
        "opencode-zen",
        "OpenCode Zen",
        "https://opencode.ai/zen/v1",
        "",
        "https://opencode.ai/auth",
        "Pick a model from the list once the key is in.",
    ),
    Preset {
        id: "ollama",
        name: "Ollama",
        tag: "local, no key",
        url: "http://127.0.0.1:11434/v1",
        model: "",
        note: "Ollama on this computer. The list shows the models you have pulled (ollama pull <model>); pick one that supports tools.",
        key_url: "",
        keyless: true,
    },
    Preset {
        id: "lmstudio",
        name: "LM Studio",
        tag: "local, no key",
        url: "http://127.0.0.1:1234/v1",
        model: "",
        note: "LM Studio's local server (Developer tab › Start server). The list shows the models it can load.",
        key_url: "",
        keyless: true,
    },
    Preset {
        id: "hermes",
        name: "Hermes gateway",
        tag: "remote agent",
        url: "http://127.0.0.1:8642/v1",
        model: "hermes-agent",
        note: "Start the gateway with `hermes gateway` (its API server needs API_SERVER_KEY, 16+ characters) and paste that key. Hermes runs its own tools on the gateway's host; Harness shows its replies, tool progress and approval requests, and other agents can delegate to it.",
        key_url: "",
        keyless: false,
    },
    Preset {
        id: "command-code-cli",
        name: "Command Code CLI",
        tag: "ACP, CLI keys",
        url: "",
        model: "",
        note: "Runs the Command Code CLI itself (its own tools and models, Claude included), one agent per key. Needs the CLI: npm i -g command-code.",
        key_url: "https://commandcode.ai/docs/quickstart",
        keyless: false,
    },
    Preset {
        id: "custom",
        name: "Other endpoint",
        tag: "OpenAI-compatible",
        url: "",
        model: "",
        note: "Any OpenAI-compatible Chat Completions endpoint: Azure, LiteLLM, vLLM, a 9router, a company proxy.",
        key_url: "",
        keyless: false,
    },
];

pub(super) fn find(id: &str) -> Option<&'static Preset> {
    PRESETS.iter().find(|p| p.id == id)
}

/// The dialog's provider kind for a preset: the Command Code CLI preset makes CLI-key agents.
pub(super) fn kind_of(id: &str) -> &'static str {
    if id == "command-code-cli" {
        "command-code"
    } else {
        ""
    }
}

thread_local! {
    /// The full model list of the last fetch; the dialog shows it filtered by the model field.
    static MODELS: std::cell::RefCell<Vec<String>> = const { std::cell::RefCell::new(Vec::new()) };
    /// Bumped on every fetch and preset change, so a late answer for an old endpoint is dropped.
    static EPOCH: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

fn row(p: &Preset) -> ProviderPreset {
    ProviderPreset {
        id: p.id.into(),
        name: p.name.into(),
        tag: p.tag.into(),
        url: p.url.into(),
        model: p.model.into(),
        note: p.note.into(),
        key_url: p.key_url.into(),
        keyless: p.keyless,
        letter: letter(p.name).into(),
    }
}

/// First letter or digit of a name, upper-cased ("OpenRouter" → "O").
pub(super) fn letter(name: &str) -> String {
    name.chars()
        .find(|c| c.is_alphanumeric())
        .map(|c| c.to_uppercase().collect())
        .unwrap_or_default()
}

pub(super) fn install(window: &AppWindow) {
    let ui = window.global::<AgentsUi>();
    let rows: Vec<ProviderPreset> = PRESETS.iter().map(row).collect();
    ui.set_presets(ModelRc::new(VecModel::from(rows)));

    let weak = window.as_weak();
    ui.on_pick_preset(move |id| {
        let Some(window) = weak.upgrade() else {
            return;
        };
        let ui = window.global::<AgentsUi>();
        let preset = find(&id);
        ui.set_provider_preset(id.clone());
        ui.set_provider_current(preset.map(row).unwrap_or_default());
        ui.set_provider_kind(kind_of(&id).into());
        ui.set_provider_label(
            preset
                .map_or("", |p| if p.id == "custom" { "" } else { p.name })
                .into(),
        );
        ui.set_provider_url(preset.map_or("", |p| p.url).into());
        ui.set_provider_model(preset.map_or("", |p| p.model).into());
        ui.set_provider_edit_url(id == "custom");
        clear_models(&ui);
        // A local server needs no key: list its models straight away.
        if preset.is_some_and(|p| p.keyless) {
            fetch(&window);
        }
    });

    let weak = window.as_weak();
    ui.on_fetch_models(move || {
        if let Some(window) = weak.upgrade() {
            fetch(&window);
        }
    });

    let weak = window.as_weak();
    ui.on_filter_models(move |text| {
        if let Some(window) = weak.upgrade() {
            show_models(&window.global::<AgentsUi>(), &text);
        }
    });

    ui.on_open_key_page(|url| crate::remote::open_url(&url));
}

fn clear_models(ui: &AgentsUi<'_>) {
    EPOCH.with(|e| e.set(e.get() + 1));
    MODELS.with(|m| m.borrow_mut().clear());
    ui.set_provider_models(ModelRc::new(VecModel::from(Vec::<SharedString>::new())));
    ui.set_provider_models_note(SharedString::new());
    ui.set_provider_models_loading(false);
}

/// Lists the endpoint's models on a worker thread (`GET <url>/models` with the first key).
fn fetch(window: &AppWindow) {
    let ui = window.global::<AgentsUi>();
    let url = ui
        .get_provider_url()
        .trim()
        .trim_end_matches('/')
        .to_owned();
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        ui.set_provider_models_note("Enter the endpoint first.".into());
        return;
    }
    let key = first_key(&ui.get_provider_key());
    let epoch = EPOCH.with(|e| {
        e.set(e.get() + 1);
        e.get()
    });
    ui.set_provider_models_loading(true);
    ui.set_provider_models_note("Listing models…".into());
    let weak = window.as_weak();
    let spawned = std::thread::Builder::new()
        .name("harness-list-models".into())
        .spawn(move || {
            let result = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|e| e.to_string())
                .and_then(|rt| rt.block_on(list_models(&url, key.as_deref())));
            let _ = slint::invoke_from_event_loop(move || {
                let Some(window) = weak.upgrade() else {
                    return;
                };
                if EPOCH.with(std::cell::Cell::get) != epoch {
                    return;
                }
                let ui = window.global::<AgentsUi>();
                ui.set_provider_models_loading(false);
                match result {
                    Ok(models) => {
                        ui.set_provider_models_note(
                            if models.is_empty() {
                                "The endpoint lists no models; type the model id.".to_owned()
                            } else {
                                format!("{} models: pick one, or type to filter", models.len())
                            }
                            .into(),
                        );
                        let fill = ui.get_provider_model().is_empty() && models.len() == 1;
                        if fill {
                            ui.set_provider_model(models[0].as_str().into());
                        }
                        MODELS.with(|m| *m.borrow_mut() = models);
                        // Show the whole list first; the model field filters it as the user types.
                        show_models(&ui, "");
                    }
                    Err(error) => ui.set_provider_models_note(error.into()),
                }
            });
        });
    if spawned.is_err() {
        ui.set_provider_models_loading(false);
        ui.set_provider_models_note("Could not start the request.".into());
    }
}

fn first_key(keys: &str) -> Option<String> {
    keys.lines()
        .map(str::trim)
        .find(|k| !k.is_empty())
        .map(str::to_owned)
}

const SHOWN_MAX: usize = 200;

fn show_models(ui: &AgentsUi<'_>, filter: &str) {
    let shown: Vec<SharedString> = MODELS.with(|m| {
        filtered(&m.borrow(), filter)
            .into_iter()
            .take(SHOWN_MAX)
            .map(SharedString::from)
            .collect()
    });
    ui.set_provider_models(ModelRc::new(VecModel::from(shown)));
}

/// Models whose id contains every word of `filter` (case-insensitive); all of them when `filter` is empty or
/// is exactly one of the ids (a model was just picked).
pub(super) fn filtered<'a>(models: &'a [String], filter: &str) -> Vec<&'a str> {
    let filter = filter.trim().to_lowercase();
    if filter.is_empty() || models.iter().any(|m| m.to_lowercase() == filter) {
        return models.iter().map(String::as_str).collect();
    }
    let words: Vec<&str> = filter.split_whitespace().collect();
    models
        .iter()
        .filter(|m| {
            let m = m.to_lowercase();
            words.iter().all(|w| m.contains(w))
        })
        .map(String::as_str)
        .collect()
}

async fn list_models(url: &str, key: Option<&str>) -> Result<Vec<String>, String> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(20))
        .user_agent(concat!("harness/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|e| format!("http client: {e}"))?;
    let mut request = client.get(format!("{url}/models"));
    if let Some(key) = key {
        request = request.bearer_auth(key);
    }
    let response = request.send().await.map_err(|e| {
        if e.is_connect() {
            format!("Cannot reach {url}. Is the server running?")
        } else if e.is_timeout() {
            "The endpoint did not answer in time.".to_owned()
        } else {
            format!("Request failed: {e}")
        }
    })?;
    let status = response.status();
    if status.as_u16() == 401 || status.as_u16() == 403 {
        return Err(if key.is_some() {
            "The endpoint refused this key.".to_owned()
        } else {
            "The endpoint needs a key: paste it above, then list the models.".to_owned()
        });
    }
    if !status.is_success() {
        return Err(format!(
            "The endpoint answered {status} to GET /models; type the model id instead."
        ));
    }
    let body: serde_json::Value = response
        .json()
        .await
        .map_err(|_| "The model list is not JSON; type the model id instead.".to_owned())?;
    Ok(model_ids(&body))
}

/// Model ids of a `/models` answer: OpenAI's `{"data": [{"id"}]}`, or a bare list, or Ollama's
/// `{"models": [{"name"}]}`. Sorted, duplicates dropped.
pub(super) fn model_ids(body: &serde_json::Value) -> Vec<String> {
    let list = body
        .get("data")
        .or_else(|| body.get("models"))
        .unwrap_or(body)
        .as_array()
        .cloned()
        .unwrap_or_default();
    let mut ids: Vec<String> = list
        .iter()
        .filter_map(|m| {
            m.as_str()
                .or_else(|| m.get("id").and_then(serde_json::Value::as_str))
                .or_else(|| m.get("name").and_then(serde_json::Value::as_str))
                .map(str::to_owned)
        })
        .filter(|id| !id.trim().is_empty())
        .collect();
    ids.sort_by_key(|id| id.to_lowercase());
    ids.dedup();
    ids
}

#[cfg(test)]
mod tests {
    use super::{filtered, find, kind_of, model_ids, PRESETS};

    #[test]
    fn presets_are_well_formed() {
        let mut ids = Vec::new();
        for p in PRESETS {
            assert!(!ids.contains(&p.id), "duplicate preset {}", p.id);
            ids.push(p.id);
            assert!(!p.name.is_empty());
            if !matches!(p.id, "custom" | "command-code-cli") {
                assert!(
                    p.url.starts_with("https://") || p.url.starts_with("http://127.0.0.1"),
                    "{}",
                    p.id
                );
                assert!(!p.url.ends_with('/'), "{}", p.id);
            }
            if !p.key_url.is_empty() {
                assert!(p.key_url.starts_with("https://"), "{}", p.id);
            }
        }
        assert_eq!(super::letter("OpenRouter"), "O");
        assert_eq!(super::letter("  9router"), "9");
        assert_eq!(kind_of("command-code-cli"), "command-code");
        assert_eq!(kind_of("command-code-api"), "");
        assert_eq!(
            find("command-code-api").map(|p| p.url),
            Some("https://api.commandcode.ai/provider/v1")
        );
    }

    #[test]
    fn reads_openai_ollama_and_bare_lists() {
        let openai = serde_json::json!({"object": "list", "data": [{"id": "b-model"}, {"id": "A-model"}, {"id": "b-model"}]});
        assert_eq!(model_ids(&openai), ["A-model", "b-model"]);
        let ollama = serde_json::json!({"models": [{"name": "qwen3:8b"}]});
        assert_eq!(model_ids(&ollama), ["qwen3:8b"]);
        let bare = serde_json::json!(["x", {"id": "y"}]);
        assert_eq!(model_ids(&bare), ["x", "y"]);
        assert!(model_ids(&serde_json::json!({"error": "nope"})).is_empty());
    }

    #[test]
    fn filter_matches_all_words_and_keeps_list_after_a_pick() {
        let models: Vec<String> = [
            "deepseek/deepseek-v4-flash",
            "openai/gpt-5.5",
            "deepseek/deepseek-v4-pro",
        ]
        .map(String::from)
        .to_vec();
        assert_eq!(filtered(&models, "seek pro"), ["deepseek/deepseek-v4-pro"]);
        assert_eq!(filtered(&models, "GPT").len(), 1);
        assert_eq!(filtered(&models, "").len(), 3);
        assert_eq!(filtered(&models, "openai/gpt-5.5").len(), 3);
    }
}
