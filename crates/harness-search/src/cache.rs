//! Workspace index cache (TS `workspaceFileListCache` + in-flight sharing).
//!
//! * Key: `workspaceIdentity?.trim() || rootPath`.
//! * Signature: `rootPath + "\n" + .zcodeignore fingerprint`; a mismatch
//!   rebuilds.
//! * 60 s TTL, at most 4 entries (LRU).
//! * Concurrent requests for the same key and signature share one scan;
//!   `refresh` bypasses both the cache and the in-flight scan, and a replaced
//!   scan only serves its own waiters (it never overwrites the newer index).
//!
//! Scans themselves are not cancellable (other waiters may depend on them),
//! but each waiter observes its own cancellation token while waiting.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use crate::cancel::CancellationToken;
use crate::error::SearchError;
use crate::ignore_rules::ignore_file_fingerprint;
use crate::index::{build_index, WorkspaceIndex};
use crate::utf16::js_trim;

/// Cache entry lifetime (TS `WORKSPACE_FILE_LIST_CACHE_TTL_MS`).
pub const INDEX_CACHE_TTL: Duration = Duration::from_secs(60);
/// Maximum cached workspaces (TS `WORKSPACE_FILE_LIST_CACHE_MAX_ENTRIES`).
pub const INDEX_CACHE_MAX_ENTRIES: usize = 4;

type ScanResult = Result<Arc<WorkspaceIndex>, SearchError>;

#[derive(Default)]
struct Slot {
    result: Mutex<Option<ScanResult>>,
    ready: Condvar,
}

struct Cached {
    at: Instant,
    signature: String,
    index: Arc<WorkspaceIndex>,
}

struct InFlight {
    signature: String,
    slot: Arc<Slot>,
}

#[derive(Default)]
struct State {
    /// LRU order: front = oldest.
    cached: Vec<(String, Cached)>,
    in_flight: HashMap<String, InFlight>,
}

/// Thread-safe index cache shared by all requests of one Host process.
#[derive(Default)]
pub struct IndexCache {
    state: Mutex<State>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// TS identity key: `workspaceIdentity?.trim() || rootPath`.
#[must_use]
pub fn workspace_key(root_path: &str, workspace_identity: Option<&str>) -> String {
    match workspace_identity.map(js_trim) {
        Some(identity) if !identity.is_empty() => identity.to_owned(),
        _ => root_path.to_owned(),
    }
}

impl IndexCache {
    /// Empty cache.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns the index for `root_path`, scanning when needed.
    ///
    /// # Errors
    /// [`SearchError::Cancelled`] when `cancel` fires while waiting.
    pub fn get(
        &self,
        root_path: &str,
        workspace_identity: Option<&str>,
        refresh: bool,
        cancel: &CancellationToken,
    ) -> ScanResult {
        let key = workspace_key(root_path, workspace_identity);
        let signature = format!(
            "{root_path}\n{}",
            ignore_file_fingerprint(Path::new(root_path))
        );
        let slot = {
            let mut state = lock(&self.state);
            state
                .cached
                .retain(|(_, c)| c.at.elapsed() < INDEX_CACHE_TTL);
            if !refresh {
                if let Some(flight) = state.in_flight.get(&key) {
                    if flight.signature == signature {
                        let slot = Arc::clone(&flight.slot);
                        drop(state);
                        return wait(&slot, cancel);
                    }
                }
                if let Some(pos) = state
                    .cached
                    .iter()
                    .position(|(k, c)| *k == key && c.signature == signature)
                {
                    let item = state.cached.remove(pos);
                    let index = Arc::clone(&item.1.index);
                    state.cached.push(item);
                    return Ok(index);
                }
            }
            let slot = Arc::new(Slot::default());
            state.in_flight.insert(
                key.clone(),
                InFlight {
                    signature: signature.clone(),
                    slot: Arc::clone(&slot),
                },
            );
            slot
        };
        let result = build_index(root_path, &CancellationToken::new()).map(Arc::new);
        {
            let mut state = lock(&self.state);
            let current = state
                .in_flight
                .get(&key)
                .is_some_and(|f| Arc::ptr_eq(&f.slot, &slot));
            if current {
                state.in_flight.remove(&key);
                if let Ok(index) = &result {
                    state.cached.retain(|(k, _)| *k != key);
                    state.cached.push((
                        key,
                        Cached {
                            at: Instant::now(),
                            signature,
                            index: Arc::clone(index),
                        },
                    ));
                    while state.cached.len() > INDEX_CACHE_MAX_ENTRIES {
                        state.cached.remove(0);
                    }
                }
            }
        }
        *lock(&slot.result) = Some(result.clone());
        slot.ready.notify_all();
        result
    }
}

fn wait(slot: &Slot, cancel: &CancellationToken) -> ScanResult {
    let mut guard = lock(&slot.result);
    loop {
        if let Some(result) = guard.as_ref() {
            return result.clone();
        }
        if cancel.is_cancelled() {
            return Err(SearchError::Cancelled);
        }
        guard = slot
            .ready
            .wait_timeout(guard, Duration::from_millis(50))
            .map_or_else(|e| e.into_inner().0, |(g, _)| g);
    }
}
