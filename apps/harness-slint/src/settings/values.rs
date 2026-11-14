//! Dotted-path access to `Settings` for the generic Slint rows (`"general.showReasoning"`). The typed struct stays
//! the source of truth: a write goes through serde, so an invalid value is rejected instead of stored.

use harness_settings::Settings;
use serde_json::{Map, Value};

pub fn get<'a>(root: &'a Value, path: &str) -> Option<&'a Value> {
    path.split('.').try_fold(root, |node, key| node.get(key))
}

/// UI value → stored JSON for paths whose representation differs.
pub fn to_stored(path: &str, value: Value) -> Value {
    match (path, &value) {
        ("appearance.theme", Value::Number(n)) => Value::String(
            match n.as_i64() {
                Some(1) => "light",
                Some(2) => "dark",
                _ => "system",
            }
            .into(),
        ),
        // 可选字符串字段：空串表示“使用默认值”，存为 null 而不是 ""。
        (_, Value::String(s)) if s.trim().is_empty() && is_optional(path) => Value::Null,
        _ => value,
    }
}

fn is_optional(path: &str) -> bool {
    matches!(
        path,
        "general.terminal.fontFamily"
            | "general.terminal.shell"
            | "general.proxy.httpProxy"
            | "general.proxy.noProxy"
            | "general.proxy.caCertPath"
    ) || path.starts_with("agents.")
}

/// Sets `path` in `settings` (intermediate objects are created, e.g. `agents.codex.defaultModel`).
pub fn set_path(settings: &mut Settings, path: &str, value: Value) -> Result<(), String> {
    let mut root = serde_json::to_value(&*settings).map_err(|e| e.to_string())?;
    let mut node = &mut root;
    let keys: Vec<&str> = path.split('.').collect();
    let Some((last, parents)) = keys.split_last() else {
        return Err(format!("bad settings path {path}"));
    };
    for key in parents {
        let Value::Object(map) = node else {
            return Err(format!("bad settings path {path}"));
        };
        node = map
            .entry((*key).to_string())
            .or_insert_with(|| Value::Object(Map::new()));
    }
    let Value::Object(map) = node else {
        return Err(format!("bad settings path {path}"));
    };
    map.insert((*last).to_string(), to_stored(path, value));
    let next: Settings =
        serde_json::from_value(root).map_err(|e| format!("invalid value for {path}: {e}"))?;
    *settings = next.normalized();
    Ok(())
}
