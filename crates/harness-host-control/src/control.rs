//! Validation of `DatabaseStartupControl` payloads from the webview.

use serde_json::Value;

/// Upper bound of an `attemptId`, matching the Host schema.
const MAX_ATTEMPT_ID_LEN: usize = 128;

/// A validated `DatabaseStartupControl` action.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartupControl {
    /// Replay the cached snapshot and ask the Host to publish again.
    Snapshot,
    /// Retry a failed migration attempt.
    Retry,
    /// Quit the application.
    Exit,
}

impl StartupControl {
    /// Validates `control` against the strict Host schema.
    ///
    /// Returns `None` for unknown actions, extra fields, or a missing or
    /// oversized `attemptId` on `retry`.
    #[must_use]
    pub fn parse(control: &Value) -> Option<Self> {
        let object = control.as_object()?;
        match object.get("action")?.as_str()? {
            "snapshot" if object.len() == 1 => Some(Self::Snapshot),
            "exit" if object.len() == 1 => Some(Self::Exit),
            "retry" if object.len() == 2 => {
                let attempt = object.get("attemptId")?.as_str()?;
                let valid = !attempt.is_empty() && attempt.len() <= MAX_ATTEMPT_ID_LEN;
                valid.then_some(Self::Retry)
            }
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::StartupControl;

    #[test]
    fn parses_the_three_actions() {
        assert_eq!(
            StartupControl::parse(&json!({ "action": "snapshot" })),
            Some(StartupControl::Snapshot)
        );
        assert_eq!(
            StartupControl::parse(&json!({ "action": "exit" })),
            Some(StartupControl::Exit)
        );
        assert_eq!(
            StartupControl::parse(&json!({ "action": "retry", "attemptId": "a-1" })),
            Some(StartupControl::Retry)
        );
    }

    #[test]
    fn rejects_malformed_controls() {
        for control in [
            json!(null),
            json!({ "action": "reboot" }),
            json!({ "action": "retry" }),
            json!({ "action": "retry", "attemptId": "" }),
            json!({ "action": "retry", "attemptId": "x".repeat(129) }),
            json!({ "action": "snapshot", "extra": true }),
        ] {
            assert_eq!(StartupControl::parse(&control), None, "{control}");
        }
    }
}
