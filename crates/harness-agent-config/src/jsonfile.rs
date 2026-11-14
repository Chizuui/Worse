//! Editing a JSON config file in place: order, indentation and unknown keys are kept; same content = no-op.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use harness_settings::fsx;
use harness_settings::json::{detect_indent, OMap, OValue};

use crate::error::{ConfigError, Result};
use crate::plan::{FileChange, Kind};

pub(crate) struct JsonFile {
    pub path: PathBuf,
    pub before: Option<String>,
    pub tree: OValue,
    original: OValue,
}

impl JsonFile {
    /// Loads `path` (missing → empty object). Invalid JSON is an error: never clobber a file we cannot read.
    pub(crate) async fn load(path: &Path) -> Result<Self> {
        Self::load_with(path, false).await
    }

    /// Like [`JsonFile::load`]; `allow_array` also accepts a top-level list.
    pub(crate) async fn load_with(path: &Path, allow_array: bool) -> Result<Self> {
        let before = fsx::read_to_string_opt(path).await?;
        let tree = match before.as_deref() {
            None => OValue::Object(OMap::new()),
            Some(text) if text.trim().is_empty() => OValue::Object(OMap::new()),
            Some(text) => match OValue::parse(text) {
                Ok(tree @ OValue::Object(_)) => tree,
                Ok(tree @ OValue::Array(_)) if allow_array => tree,
                Ok(_) => {
                    return Err(ConfigError::invalid(format!(
                        "{}: top level is not an object",
                        path.display()
                    )))
                }
                Err(e) => return Err(ConfigError::invalid(format!("{}: {e}", path.display()))),
            },
        };
        Ok(Self {
            path: path.to_path_buf(),
            before,
            original: tree.clone(),
            tree,
        })
    }

    /// Root object.
    pub(crate) fn root(&mut self) -> &mut OMap {
        if !matches!(self.tree, OValue::Object(_)) {
            self.tree = OValue::Object(OMap::new());
        }
        match &mut self.tree {
            OValue::Object(map) => map,
            _ => unreachable!("tree was just set to an object"),
        }
    }

    /// Read-only object at `key` of the root.
    pub(crate) fn object(&self, key: &str) -> Option<&OMap> {
        self.tree.as_object()?.get(key)?.as_object()
    }

    pub(crate) fn into_change(self) -> FileChange {
        if self.tree == self.original {
            if let Some(before) = self.before {
                return FileChange::write(self.path, Some(before.clone()), before, Kind::Json);
            }
        }
        let indent = self
            .before
            .as_deref()
            .map_or_else(|| "  ".to_owned(), detect_indent);
        let trailing = self.before.as_deref().map_or(true, |t| t.ends_with('\n'));
        let text = self.tree.to_pretty(&indent, trailing);
        FileChange::write(self.path, self.before, text, Kind::Json)
    }
}

/// Sets `key` to a string, or removes it.
pub(crate) fn set_str(map: &mut OMap, key: &str, value: Option<&str>) {
    match value {
        Some(v) => {
            if map.get(key).and_then(OValue::as_str) != Some(v) {
                map.insert(key, OValue::String(v.to_owned()));
            }
        }
        None => {
            map.remove(key);
        }
    }
}

/// Sets `key` to a string list; an empty list removes the key unless it was present.
pub(crate) fn set_list(map: &mut OMap, key: &str, items: &[String]) {
    if items.is_empty() && map.get(key).is_none() {
        return;
    }
    let value = OValue::Array(items.iter().cloned().map(OValue::String).collect());
    if map.get(key) != Some(&value) {
        map.insert(key, value);
    }
}

/// Sets `key` to a string map; empty removes the key unless it was present.
pub(crate) fn set_map(map: &mut OMap, key: &str, items: &BTreeMap<String, String>) {
    if items.is_empty() && map.get(key).is_none() {
        return;
    }
    let mut existing = match map.get(key) {
        Some(OValue::Object(m)) => m.clone(),
        _ => OMap::new(),
    };
    let stale: Vec<String> = existing
        .keys()
        .filter(|k| !items.contains_key(*k))
        .map(str::to_owned)
        .collect();
    for k in stale {
        existing.remove(&k);
    }
    for (k, v) in items {
        set_str(&mut existing, k, Some(v));
    }
    map.insert(key, OValue::Object(existing));
}

pub(crate) fn set_number(map: &mut OMap, key: &str, value: Option<f64>) {
    match value.and_then(number) {
        Some(n) => {
            let v = OValue::Number(n);
            if map.get(key) != Some(&v) {
                map.insert(key, v);
            }
        }
        None => {
            map.remove(key);
        }
    }
}

pub(crate) fn set_bool(map: &mut OMap, key: &str, value: Option<bool>) {
    match value {
        Some(b) => {
            if map.get(key) != Some(&OValue::Bool(b)) {
                map.insert(key, OValue::Bool(b));
            }
        }
        None => {
            map.remove(key);
        }
    }
}

/// Integral values become JSON integers (`30`, not `30.0`).
pub(crate) fn number(value: f64) -> Option<serde_json::Number> {
    if value.fract() == 0.0 && value.abs() < 9.0e15 {
        #[allow(clippy::cast_possible_truncation)]
        return Some(serde_json::Number::from(value as i64));
    }
    serde_json::Number::from_f64(value)
}

/// Replaces the unknown keys of `map` (keys not in `known`) by `extra`.
pub(crate) fn set_extra(
    map: &mut OMap,
    known: &[&str],
    extra: &serde_json::Map<String, serde_json::Value>,
) {
    let stale: Vec<String> = map
        .keys()
        .filter(|k| !known.contains(k) && !extra.contains_key(*k))
        .map(str::to_owned)
        .collect();
    for k in stale {
        map.remove(&k);
    }
    for (k, v) in extra {
        if known.contains(&k.as_str()) {
            continue;
        }
        let value = OValue::from_json(v.clone());
        if map.get(k) != Some(&value) {
            map.insert(k.clone(), value);
        }
    }
}

/// Unknown keys of `map` as JSON.
pub(crate) fn extra_of(map: &OMap, known: &[&str]) -> serde_json::Map<String, serde_json::Value> {
    map.iter()
        .filter(|(k, _)| !known.contains(k))
        .map(|(k, v)| (k.to_owned(), v.to_json()))
        .collect()
}

pub(crate) fn get_str(map: &OMap, key: &str) -> Option<String> {
    map.get(key).and_then(OValue::as_str).map(str::to_owned)
}

pub(crate) fn get_list(map: &OMap, key: &str) -> Vec<String> {
    match map.get(key) {
        Some(OValue::Array(items)) => items
            .iter()
            .filter_map(|i| i.as_str().map(str::to_owned))
            .collect(),
        _ => Vec::new(),
    }
}

pub(crate) fn get_map(map: &OMap, key: &str) -> BTreeMap<String, String> {
    map.get(key)
        .and_then(OValue::as_object)
        .map(|m| {
            m.iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (k.to_owned(), s.to_owned())))
                .collect()
        })
        .unwrap_or_default()
}

pub(crate) fn get_f64(map: &OMap, key: &str) -> Option<f64> {
    match map.get(key) {
        Some(OValue::Number(n)) => n.as_f64(),
        _ => None,
    }
}

pub(crate) fn get_bool(map: &OMap, key: &str) -> Option<bool> {
    match map.get(key) {
        Some(OValue::Bool(b)) => Some(*b),
        _ => None,
    }
}
