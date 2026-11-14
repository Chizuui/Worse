//! Antigravity (agy) account list and the credential swaps between account slots and agy's live login.
//!
//! Owner: the hub owns `<home>/agy_accounts.json` (no secrets). Logins live only in the credential store:
//! agy's own entry ([`AGY_LIVE_TARGET`]) and one Harness copy per account ([`slot_target`]).

use std::collections::HashSet;
use std::path::Path;

use harness_core_api::{AgyAccountInfo, AgyAccounts, ApiError};
use serde::{Deserialize, Serialize};

use crate::agy_cred::{slot_target, AgyCredStore, AGY_LIVE_TARGET};
use crate::util::now_ms;

pub(crate) const AGY_ACCOUNTS_FILE: &str = "agy_accounts.json";
const NAME_MAX: usize = 40;
const SLUG_MAX: usize = 24;

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct AgyFile {
    #[serde(default)]
    pub accounts: Vec<AgyEntry>,
    #[serde(default)]
    pub active: Option<String>,
    #[serde(default)]
    pub order: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct AgyEntry {
    pub id: String,
    pub name: String,
    pub created_ms: i64,
}

pub(crate) async fn load(home: &Path) -> Result<AgyFile, ApiError> {
    let path = home.join(AGY_ACCOUNTS_FILE);
    match tokio::fs::read(&path).await {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map_err(|e| ApiError::Storage(format!("{}: {e}", path.display()))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(AgyFile::default()),
        Err(e) => Err(ApiError::Storage(format!("{}: {e}", path.display()))),
    }
}

pub(crate) async fn save(home: &Path, file: &AgyFile) -> Result<(), ApiError> {
    let path = home.join(AGY_ACCOUNTS_FILE);
    let tmp = home.join(format!("{AGY_ACCOUNTS_FILE}.tmp"));
    let bytes = serde_json::to_vec_pretty(file).map_err(|e| ApiError::Internal(e.to_string()))?;
    let storage = |e: std::io::Error| ApiError::Storage(format!("{}: {e}", path.display()));
    tokio::fs::create_dir_all(home).await.map_err(storage)?;
    tokio::fs::write(&tmp, bytes).await.map_err(storage)?;
    tokio::fs::rename(&tmp, &path).await.map_err(storage)
}

fn slug(name: &str) -> String {
    let mut out = String::new();
    for c in name.chars() {
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

impl AgyFile {
    fn contains(&self, id: &str) -> bool {
        self.accounts.iter().any(|a| a.id == id)
    }

    fn require(&self, id: &str) -> Result<(), String> {
        if self.contains(id) {
            Ok(())
        } else {
            Err(format!("no Antigravity account {id}"))
        }
    }

    /// Keeps `order` a permutation of the account ids and drops dangling references.
    pub(crate) fn normalize(&mut self) {
        let ids: Vec<String> = self.accounts.iter().map(|a| a.id.clone()).collect();
        let mut seen = HashSet::new();
        self.order
            .retain(|id| ids.contains(id) && seen.insert(id.clone()));
        for id in ids {
            if !self.order.contains(&id) {
                self.order.push(id);
            }
        }
        if self.active.as_ref().is_some_and(|id| !self.contains(id)) {
            self.active = None;
        }
        if self.pending.as_ref().is_some_and(|id| !self.contains(id)) {
            self.pending = None;
        }
    }

    pub(crate) fn add(&mut self, name: &str, now_ms: i64) -> Result<String, String> {
        let name = name.trim();
        if name.is_empty() || name.chars().count() > NAME_MAX {
            return Err(format!("name must be 1-{NAME_MAX} characters"));
        }
        let stem = slug(name);
        let mut id = stem.clone();
        let mut n = 2;
        while self.contains(&id) {
            id = format!("{stem}-{n}");
            n += 1;
        }
        self.accounts.push(AgyEntry {
            id: id.clone(),
            name: name.to_owned(),
            created_ms: now_ms,
        });
        self.normalize();
        Ok(id)
    }

    pub(crate) fn set_order(&mut self, order: Vec<String>) {
        self.order = order;
        self.normalize();
    }

    /// Next account in fallback order after the active one that has a login and is not exhausted.
    pub(crate) fn next_fallback(
        &self,
        has_login: &dyn Fn(&str) -> bool,
        exhausted: &HashSet<String>,
    ) -> Option<String> {
        let start = self
            .active
            .as_ref()
            .and_then(|a| self.order.iter().position(|id| id == a))
            .map_or(0, |i| i + 1);
        (0..self.order.len())
            .map(|offset| &self.order[(start + offset) % self.order.len()])
            .find(|id| {
                Some(*id) != self.active.as_ref() && !exhausted.contains(*id) && has_login(id)
            })
            .cloned()
    }
    fn free_name(&self, stem: &str) -> String {
        let mut name = stem.to_owned();
        let mut n = 2;
        while self
            .accounts
            .iter()
            .any(|a| a.name.eq_ignore_ascii_case(&name))
        {
            name = format!("{stem} {n}");
            n += 1;
        }
        name
    }
}

/// Copies agy's live login into the active account's slot (keeps refreshed tokens). When no account is
/// active and the live login is not already stored in a slot, auto-imports it as "Main" (or "Main 2"…) so
/// a subsequent swap or sign-in never loses an unimported login.
pub(crate) fn save_live(store: &dyn AgyCredStore, file: &mut AgyFile) -> Result<(), String> {
    let Some(live) = store.read(AGY_LIVE_TARGET)? else {
        return Ok(());
    };
    if let Some(active) = &file.active {
        return store.write(&slot_target(active), &live);
    }
    for entry in &file.accounts {
        if store.read(&slot_target(&entry.id))?.as_ref() == Some(&live) {
            return Ok(());
        }
    }
    let name = file.free_name("Main");
    let id = file.add(&name, now_ms())?;
    store.write(&slot_target(&id), &live)
}

/// Saves the current login, signs agy out and marks `id` as waiting for its login.
pub(crate) fn begin_sign_in(
    store: &dyn AgyCredStore,
    file: &mut AgyFile,
    id: &str,
) -> Result<(), String> {
    file.require(id)?;
    save_live(store, file)?;
    store.delete(AGY_LIVE_TARGET)?;
    file.active = None;
    file.pending = Some(id.to_owned());
    Ok(())
}

/// Stores a fresh live login into the pending account. True when it completed.
pub(crate) fn finish_sign_in(store: &dyn AgyCredStore, file: &mut AgyFile) -> Result<bool, String> {
    let Some(id) = file.pending.clone() else {
        return Ok(false);
    };
    let Some(live) = store.read(AGY_LIVE_TARGET)? else {
        return Ok(false);
    };
    store.write(&slot_target(&id), &live)?;
    file.active = Some(id);
    file.pending = None;
    Ok(true)
}

/// Makes `id`'s saved login the live one.
pub(crate) fn use_account(
    store: &dyn AgyCredStore,
    file: &mut AgyFile,
    id: &str,
) -> Result<(), String> {
    file.require(id)?;
    let Some(slot) = store.read(&slot_target(id))? else {
        return Err("this account has no saved login; click Sign in first".into());
    };
    save_live(store, file)?;
    store.write(AGY_LIVE_TARGET, &slot)?;
    file.active = Some(id.to_owned());
    file.pending = None;
    Ok(())
}

/// New account from agy's current login; it becomes active.
pub(crate) fn import_live(
    store: &dyn AgyCredStore,
    file: &mut AgyFile,
    name: &str,
    now_ms: i64,
) -> Result<String, String> {
    let Some(live) = store.read(AGY_LIVE_TARGET)? else {
        return Err("agy is not signed in".into());
    };
    let id = file.add(name, now_ms)?;
    store.write(&slot_target(&id), &live)?;
    file.active = Some(id.clone());
    Ok(id)
}

/// Deletes the slot and the row; the live login is left alone.
pub(crate) fn remove(store: &dyn AgyCredStore, file: &mut AgyFile, id: &str) -> Result<(), String> {
    file.require(id)?;
    store.delete(&slot_target(id))?;
    file.accounts.retain(|a| a.id != id);
    file.normalize();
    Ok(())
}

pub(crate) fn view(
    store: &dyn AgyCredStore,
    file: &AgyFile,
    exhausted: &HashSet<String>,
) -> AgyAccounts {
    if !store.available() {
        return AgyAccounts::default();
    }
    let accounts = file
        .accounts
        .iter()
        .map(|a| AgyAccountInfo {
            id: a.id.clone(),
            name: a.name.clone(),
            created_ms: a.created_ms,
            signed_in: store.exists(&slot_target(&a.id)).unwrap_or(false),
            active: file.active.as_deref() == Some(a.id.as_str()),
            exhausted: exhausted.contains(&a.id),
        })
        .collect();
    AgyAccounts {
        available: true,
        accounts,
        order: file.order.clone(),
        live_signed_in: store.exists(AGY_LIVE_TARGET).unwrap_or(false),
        pending: file.pending.clone(),
    }
}

#[cfg(test)]
#[path = "agy_accounts_tests.rs"]
mod tests;
