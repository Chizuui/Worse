//! Agent launch overrides in `<data dir>/agents.json` (AGT-006), the file the hub reads at startup. Entries are
//! matched by `id`; other entries and unknown keys are kept. Changes apply on the next hub start.

use std::collections::BTreeMap;
use std::path::Path;

use harness_settings::json::{OMap, OValue};
use serde::{Deserialize, Serialize};

use crate::error::{ConfigError, Result};
use crate::jsonfile::{get_list, get_map, get_str, set_list, set_map, set_str, JsonFile};
use crate::plan::WritePlan;

/// File name inside the data dir.
pub const AGENTS_FILE: &str = "agents.json";

/// One launch override.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct LaunchOverride {
    pub id: String,
    pub display_name: Option<String>,
    pub program: String,
    pub args: Vec<String>,
    /// User-entered environment for the agent process.
    pub env: BTreeMap<String, String>,
}

fn entries_mut(file: &mut JsonFile) -> Result<&mut Vec<OValue>> {
    // `agents.json` is either a list or `{ replaceBuiltins, agents: [...] }`.
    if matches!(file.tree, OValue::Array(_)) {
        return match &mut file.tree {
            OValue::Array(items) => Ok(items),
            _ => unreachable!("checked above"),
        };
    }
    let root = file.root();
    if !matches!(root.get("agents"), Some(OValue::Array(_)) | None) {
        return Err(ConfigError::invalid("agents.json: `agents` is not a list"));
    }
    if root.get("agents").is_none() {
        root.insert("agents", OValue::Array(Vec::new()));
    }
    match root.get_mut("agents") {
        Some(OValue::Array(items)) => Ok(items),
        _ => unreachable!("agents was just inserted"),
    }
}

async fn load(data_dir: &Path) -> Result<JsonFile> {
    JsonFile::load_with(&data_dir.join(AGENTS_FILE), true).await
}

/// Current overrides.
///
/// # Errors
///
/// Invalid file.
pub async fn list(data_dir: &Path) -> Result<Vec<LaunchOverride>> {
    let mut file = load(data_dir).await?;
    Ok(entries_mut(&mut file)?
        .iter()
        .filter_map(OValue::as_object)
        .map(|m| LaunchOverride {
            id: get_str(m, "id").unwrap_or_default(),
            display_name: get_str(m, "displayName"),
            program: get_str(m, "program").unwrap_or_default(),
            args: get_list(m, "args"),
            env: get_map(m, "env"),
        })
        .collect())
}

/// Creates or updates the override with `entry.id`.
///
/// # Errors
///
/// Empty id/program or invalid file.
pub async fn plan_upsert(data_dir: &Path, entry: &LaunchOverride) -> Result<WritePlan> {
    if entry.id.trim().is_empty() || entry.program.trim().is_empty() {
        return Err(ConfigError::invalid(
            "an agent override needs an id and a program",
        ));
    }
    let mut file = load(data_dir).await?;
    let items = entries_mut(&mut file)?;
    let index = items.iter().position(|v| {
        v.as_object().and_then(|m| get_str(m, "id")).as_deref() == Some(entry.id.as_str())
    });
    let index = index.unwrap_or_else(|| {
        let mut m = OMap::new();
        m.insert("id", OValue::String(entry.id.clone()));
        items.push(OValue::Object(m));
        items.len() - 1
    });
    if let Some(m) = items[index].as_object_mut() {
        set_str(m, "displayName", entry.display_name.as_deref());
        set_str(m, "program", Some(&entry.program));
        set_list(m, "args", &entry.args);
        set_map(m, "env", &entry.env);
    }
    Ok(WritePlan::single(file.into_change()))
}

/// Removes the override for `id` (the built-in definition applies again).
///
/// # Errors
///
/// Not found or invalid file.
pub async fn plan_remove(data_dir: &Path, id: &str) -> Result<WritePlan> {
    let mut file = load(data_dir).await?;
    let items = entries_mut(&mut file)?;
    let before = items.len();
    items.retain(|v| v.as_object().and_then(|m| get_str(m, "id")).as_deref() != Some(id));
    if items.len() == before {
        return Err(ConfigError::NotFound(id.to_owned()));
    }
    Ok(WritePlan::single(file.into_change()))
}
