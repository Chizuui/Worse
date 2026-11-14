//! Identifiers that cross Harness process and API boundaries.

use core::fmt;

use serde::{Deserialize, Serialize};

/// Identifier of a native Harness window.
///
/// Window identifiers are assigned by the shell and stay stable for the
/// lifetime of the process that owns the window.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct WindowId(u32);

impl WindowId {
    /// Creates a window identifier from its raw value.
    #[must_use]
    pub const fn new(raw: u32) -> Self {
        Self(raw)
    }

    /// Returns the raw window number.
    #[must_use]
    pub const fn get(self) -> u32 {
        self.0
    }
}

impl fmt::Display for WindowId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}", self.0)
    }
}

impl From<u32> for WindowId {
    fn from(raw: u32) -> Self {
        Self::new(raw)
    }
}

/// Identifier of a supervised sidecar process.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SidecarId(String);

impl SidecarId {
    /// Creates a sidecar identifier from any string-like value.
    #[must_use]
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// Returns the identifier as a string slice.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for SidecarId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl From<String> for SidecarId {
    fn from(value: String) -> Self {
        Self(value)
    }
}

impl From<&str> for SidecarId {
    fn from(value: &str) -> Self {
        Self(value.to_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::{SidecarId, WindowId};

    #[test]
    fn window_id_round_trips_through_display() {
        let id = WindowId::new(7);
        assert_eq!(id.get(), 7);
        assert_eq!(id.to_string(), "7");
        assert_eq!(WindowId::from(7), id);
    }

    #[test]
    fn sidecar_id_round_trips_through_display() {
        let id = SidecarId::new("agent-main");
        assert_eq!(id.as_str(), "agent-main");
        assert_eq!(id.to_string(), "agent-main");
        assert_eq!(SidecarId::from("agent-main"), id);
        assert_eq!(SidecarId::from(String::from("agent-main")), id);
    }
}
