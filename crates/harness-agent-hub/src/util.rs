//! Small helpers.

use std::time::{SystemTime, UNIX_EPOCH};

/// Unix milliseconds now.
pub(crate) fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

/// Unix seconds now.
pub(crate) fn now_secs() -> i64 {
    now_ms() / 1000
}

/// New random id with a prefix.
pub(crate) fn new_id(prefix: &str) -> String {
    format!("{prefix}-{}", uuid::Uuid::new_v4().simple())
}
