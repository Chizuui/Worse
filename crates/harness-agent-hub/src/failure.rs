//! Typed turn failures (phase-9-parity.md §9.2.5 R20): kind, details, trace and a retry hint from what the agent
//! process reported. Quota exhaustion is not a failure here (it fails over).

use harness_core_api::{ErrorClass, Failure, FailureKind, RetryHint};

use crate::acp::RpcError;
use crate::quota::AgentErrorInfo;

/// ACP `auth_required` JSON-RPC error code.
const AUTH_REQUIRED: i64 = -32000;
/// Longest `details` text.
const MAX_DETAILS: usize = 4096;

/// Where the attempt failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Phase {
    /// Spawn, `initialize`, `session/new` / `session/load` (before the prompt was sent).
    Setup,
    Prompt,
}

fn mentions_login(message: &str) -> bool {
    let lower = message.to_lowercase();
    [
        "authenticat",
        "auth required",
        "auth_required",
        "unauthorized",
        "login",
        "log in",
        "not logged",
    ]
    .iter()
    .any(|word| lower.contains(word))
}

/// Classifies one failed attempt.
pub(crate) fn classify(
    info: &AgentErrorInfo,
    class: &ErrorClass,
    phase: Phase,
    trace: Option<String>,
) -> Failure {
    let kind = if info.code == Some(AUTH_REQUIRED) || mentions_login(&info.message) {
        FailureKind::Auth
    } else if info.code == Some(RpcError::EXITED) {
        FailureKind::ProcessExited
    } else if phase == Phase::Setup {
        FailureKind::InitFailed
    } else if matches!(class, ErrorClass::Transient { .. }) {
        FailureKind::Transient
    } else {
        FailureKind::Other
    };
    let retry = match (kind, class) {
        (FailureKind::Auth, _) => RetryHint::Login,
        (FailureKind::Transient, ErrorClass::Transient { retry_after_secs }) => RetryHint::After {
            secs: *retry_after_secs,
        },
        (FailureKind::ProcessExited | FailureKind::InitFailed | FailureKind::Transient, _) => {
            RetryHint::Retry
        }
        // 其他错误（如模型拒绝、参数错误）原样重试通常仍会失败，但用户可能改了配置后再试，因此仍允许重试。
        (FailureKind::Other, _) => RetryHint::Retry,
    };
    let mut details = Vec::new();
    if let Some(code) = info.code {
        details.push(format!("code {code}"));
    }
    if let Some(data) = &info.data {
        details.push(serde_json::to_string_pretty(data).unwrap_or_else(|_| data.to_string()));
    }
    let details = (!details.is_empty()).then(|| truncate(&details.join("\n"), MAX_DETAILS));
    Failure {
        kind,
        message: info.message.clone(),
        details,
        trace: trace.map(|t| truncate(&t, MAX_DETAILS)),
        retry,
    }
}

fn truncate(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_owned();
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &text[..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(code: Option<i64>, message: &str) -> AgentErrorInfo {
        AgentErrorInfo {
            code,
            message: message.into(),
            data: None,
        }
    }

    #[test]
    fn kinds_and_hints() {
        let fatal = ErrorClass::Fatal;
        let auth = classify(
            &info(Some(-32000), "Authentication required"),
            &fatal,
            Phase::Prompt,
            None,
        );
        assert_eq!(
            (auth.kind, auth.retry),
            (FailureKind::Auth, RetryHint::Login)
        );
        let exited = classify(
            &info(Some(RpcError::EXITED), "agent process exited"),
            &fatal,
            Phase::Prompt,
            Some("boom".into()),
        );
        assert_eq!(exited.kind, FailureKind::ProcessExited);
        assert_eq!(exited.trace.as_deref(), Some("boom"));
        let init = classify(&info(None, "spawn failed"), &fatal, Phase::Setup, None);
        assert_eq!(
            (init.kind, init.retry),
            (FailureKind::InitFailed, RetryHint::Retry)
        );
        let transient = ErrorClass::Transient {
            retry_after_secs: Some(5),
        };
        let slow = classify(&info(Some(-32603), "rate"), &transient, Phase::Prompt, None);
        assert_eq!(slow.retry, RetryHint::After { secs: Some(5) });
        assert_eq!(slow.details.as_deref(), Some("code -32603"));
    }

    #[test]
    fn truncates_on_char_boundary() {
        let text = "é".repeat(10);
        assert_eq!(truncate(&text, 5), "éé…");
    }
}
