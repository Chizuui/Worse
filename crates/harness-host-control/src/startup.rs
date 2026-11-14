//! Shell-side cache of the Host `database-startup-state`.
//!
//! The Host owns the startup state machine; the shell only keeps the newest
//! accepted snapshot so a reloaded renderer can render it immediately. The
//! admission rule mirrors the Electron relay: a snapshot must carry this
//! Host's `startupId`, and a sequence that is not newer is ignored.

use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};
use uuid::Uuid;

/// Latest accepted startup snapshot for one Host process.
pub(crate) struct StartupCache {
    startup_id: String,
    latest: Option<Value>,
}

impl StartupCache {
    pub(crate) fn new(startup_id: String) -> Self {
        Self {
            startup_id,
            latest: None,
        }
    }

    pub(crate) fn startup_id(&self) -> &str {
        &self.startup_id
    }

    pub(crate) fn latest(&self) -> Option<Value> {
        self.latest.clone()
    }

    /// Stores `state` when it belongs to this Host and is newer than the cache.
    pub(crate) fn accept(&mut self, state: &Value) -> bool {
        if state.get("startupId").and_then(Value::as_str) != Some(self.startup_id.as_str()) {
            return false;
        }
        let Some(sequence) = state.get("sequence").and_then(Value::as_u64) else {
            return false;
        };
        if let Some(previous) = self.latest_sequence() {
            if sequence <= previous {
                return false;
            }
        }
        self.latest = Some(state.clone());
        true
    }

    /// Records that the Host transport closed and returns the failed snapshot.
    ///
    /// Returns `None` when the cache already reports a failure.
    pub(crate) fn transport_closed(&mut self) -> Option<Value> {
        // 修复依据：Electron relay 在 Host 退出后把最新快照推进为 failed/transport_closed，
        // 否则重载后的渲染端会读到旧的 ready 快照并放行一个已经失效的端口。
        let now = now_ms();
        let mut next = self.latest.clone().unwrap_or_else(|| {
            json!({
                "schemaVersion": 1,
                "startupId": self.startup_id,
                "attemptId": Uuid::new_v4().to_string(),
                "sequence": 0,
                "startedAt": now,
                "updatedAt": now,
                "phase": "starting",
                "disk": [],
            })
        });
        let phase = next.get("phase").cloned().unwrap_or(Value::Null);
        if phase.as_str() == Some("failed") {
            return None;
        }
        let sequence = next.get("sequence").and_then(Value::as_u64).unwrap_or(0) + 1;
        let object = next.as_object_mut()?;
        object.insert("sequence".to_owned(), json!(sequence));
        object.insert("updatedAt".to_owned(), json!(now));
        object.insert("failedPhase".to_owned(), phase);
        object.insert("phase".to_owned(), json!("failed"));
        object.insert("errorCode".to_owned(), json!("transport_closed"));
        self.latest = Some(next.clone());
        Some(next)
    }

    fn latest_sequence(&self) -> Option<u64> {
        self.latest
            .as_ref()
            .and_then(|state| state.get("sequence"))
            .and_then(Value::as_u64)
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| {
            u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
        })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::StartupCache;

    fn state(startup_id: &str, sequence: u64, phase: &str) -> serde_json::Value {
        json!({ "startupId": startup_id, "sequence": sequence, "phase": phase })
    }

    #[test]
    fn accepts_only_newer_snapshots_of_this_host() {
        let mut cache = StartupCache::new("s-1".to_owned());
        assert!(cache.accept(&state("s-1", 1, "starting")));
        assert!(!cache.accept(&state("s-1", 1, "ready")));
        assert!(!cache.accept(&state("other", 9, "ready")));
        assert!(cache.accept(&state("s-1", 2, "ready")));
        assert_eq!(cache.latest(), Some(state("s-1", 2, "ready")));
    }

    #[test]
    fn transport_close_turns_the_latest_snapshot_into_a_failure_once() {
        let mut cache = StartupCache::new("s-1".to_owned());
        assert!(cache.accept(&state("s-1", 4, "ready")));
        let failed = cache.transport_closed().unwrap();
        assert_eq!(failed["phase"], "failed");
        assert_eq!(failed["failedPhase"], "ready");
        assert_eq!(failed["errorCode"], "transport_closed");
        assert_eq!(failed["sequence"], 5);
        assert!(cache.transport_closed().is_none());
    }

    #[test]
    fn transport_close_without_a_snapshot_synthesizes_one() {
        let mut cache = StartupCache::new("s-2".to_owned());
        let failed = cache.transport_closed().unwrap();
        assert_eq!(failed["startupId"], "s-2");
        assert_eq!(failed["failedPhase"], "starting");
        assert_eq!(failed["sequence"], 1);
    }
}
