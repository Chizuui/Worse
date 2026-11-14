//! Local-time display of hub timestamps (API request 2, spec phase-8-core.md §15.1).
//!
//! The hub keeps every time in UTC (`resets_at` = unix seconds). UIs format them in the system time zone with
//! [`local_time_label`]; the zone is read by chrono (Windows API / tz database), no unsafe code.

use chrono::{DateTime, Local, TimeZone, Utc};

/// `HH:MM` when `unix_secs` falls on today's local date, else `MM-DD HH:MM` (both in local time).
#[must_use]
pub fn local_time_label(unix_secs: i64) -> String {
    local_time_label_at(unix_secs, Utc::now().timestamp())
}

/// [`local_time_label`] relative to an explicit "now" (unix seconds).
#[must_use]
pub fn local_time_label_at(unix_secs: i64, now_secs: i64) -> String {
    let (Some(at), Some(now)) = (to_local(unix_secs), to_local(now_secs)) else {
        return String::new();
    };
    if at.date_naive() == now.date_naive() {
        at.format("%H:%M").to_string()
    } else {
        at.format("%m-%d %H:%M").to_string()
    }
}

/// Offset of the local time zone from UTC at `unix_secs`, in seconds (east positive).
#[must_use]
pub fn local_utc_offset_secs(unix_secs: i64) -> i32 {
    to_local(unix_secs).map_or(0, |t| t.offset().local_minus_utc())
}

fn to_local(unix_secs: i64) -> Option<DateTime<Local>> {
    Local.timestamp_opt(unix_secs, 0).single()
}
