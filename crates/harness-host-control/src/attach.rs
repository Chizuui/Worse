//! Attachment credentials and the Host message carried by `attach.open`.

use std::time::Duration;

use harness_host_protocol::{AttachOpenLine, HOST_WS_PATH};
use serde_json::{json, Map, Value};
use uuid::Uuid;

use crate::error::HostControlError;

/// Fields the shell must never put into `init-local` (contract section 7).
const FORBIDDEN_INIT_FIELDS: [&str; 3] = ["hostId", "deliveryKind", "port"];

/// Freshly generated identifiers and secret for one attachment.
pub(crate) struct AttachCredentials {
    pub(crate) request_id: String,
    pub(crate) attachment_id: String,
    pub(crate) token: String,
}

impl AttachCredentials {
    /// Generates a UUID attachment id and a 64-hex single-use token.
    ///
    /// The token concatenates two v4 UUIDs (about 244 random bits) so no
    /// separate random-number dependency is needed.
    pub(crate) fn generate() -> Self {
        let mut bytes = [0_u8; 32];
        bytes[..16].copy_from_slice(Uuid::new_v4().as_bytes());
        bytes[16..].copy_from_slice(Uuid::new_v4().as_bytes());
        Self {
            request_id: Uuid::new_v4().to_string(),
            attachment_id: Uuid::new_v4().to_string(),
            token: hex::encode(bytes),
        }
    }

    /// Builds the `attach.open` line for these credentials.
    pub(crate) fn open_line(&self, ttl: Duration, host_message: Value) -> AttachOpenLine {
        let ttl_ms = u64::try_from(ttl.as_millis()).unwrap_or(u64::MAX);
        AttachOpenLine::new(
            self.request_id.clone(),
            self.attachment_id.clone(),
            self.token.clone(),
            ttl_ms,
            host_message,
        )
    }
}

/// Returns the loopback attachment URL; the token is never part of it.
pub(crate) fn attach_url(ws_port: u16) -> String {
    format!("ws://127.0.0.1:{ws_port}{HOST_WS_PATH}")
}

/// Builds the `attach-service-port` message used after the Host initialized.
pub(crate) fn service_port_message(credentials: &AttachCredentials) -> Value {
    json!({
        "type": "attach-service-port",
        "requestId": credentials.request_id,
        "attachmentId": credentials.attachment_id,
        "clientMode": "desktop-continuous",
        "scope": { "kind": "local" },
    })
}

/// Validates the configured `init-local` message and returns it with a
/// guaranteed `databaseStartupId`, plus that id.
pub(crate) fn prepare_init_message(message: &Value) -> Result<(Value, String), HostControlError> {
    let object = message
        .as_object()
        .ok_or_else(|| invalid_init("init message must be a JSON object"))?;
    if object.get("type").and_then(Value::as_str) != Some("init-local") {
        return Err(invalid_init("init message type must be init-local"));
    }
    if let Some(field) = FORBIDDEN_INIT_FIELDS
        .iter()
        .find(|field| object.contains_key(**field))
    {
        return Err(invalid_init(&format!(
            "init message must not carry {field}"
        )));
    }
    let mut prepared: Map<String, Value> = object.clone();
    let startup_id = match object.get("databaseStartupId").and_then(Value::as_str) {
        Some(id) if !id.is_empty() => id.to_owned(),
        _ => {
            let generated = Uuid::new_v4().to_string();
            prepared.insert("databaseStartupId".to_owned(), json!(generated));
            generated
        }
    };
    Ok((Value::Object(prepared), startup_id))
}

fn invalid_init(reason: &str) -> HostControlError {
    HostControlError::Protocol(reason.to_owned())
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use serde_json::json;

    use super::{attach_url, prepare_init_message, service_port_message, AttachCredentials};

    #[test]
    fn credentials_are_unique_lowercase_hex_tokens() {
        let first = AttachCredentials::generate();
        let second = AttachCredentials::generate();
        assert_eq!(first.token.len(), 64);
        assert!(first
            .token
            .chars()
            .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c)));
        assert_ne!(first.token, second.token);
        assert_ne!(first.attachment_id, second.attachment_id);
        let line = first.open_line(Duration::from_secs(30), json!({}));
        assert_eq!(line.ttl_ms, 30_000);
        assert_eq!(line.attachment_id, first.attachment_id);
    }

    #[test]
    fn url_never_contains_the_token() {
        assert_eq!(attach_url(4242), "ws://127.0.0.1:4242/v1/attach");
    }

    #[test]
    fn service_port_message_is_desktop_continuous_local() {
        let credentials = AttachCredentials::generate();
        let message = service_port_message(&credentials);
        assert_eq!(message["type"], "attach-service-port");
        assert_eq!(message["clientMode"], "desktop-continuous");
        assert_eq!(message["scope"], json!({ "kind": "local" }));
        assert_eq!(message["attachmentId"], credentials.attachment_id.as_str());
    }

    #[test]
    fn init_message_validation() {
        let (message, id) = prepare_init_message(&json!({
            "type": "init-local",
            "databaseStartupId": "s-1",
        }))
        .unwrap();
        assert_eq!(id, "s-1");
        assert_eq!(message["databaseStartupId"], "s-1");

        let (message, id) = prepare_init_message(&json!({ "type": "init-local" })).unwrap();
        assert_eq!(message["databaseStartupId"], id.as_str());

        assert!(prepare_init_message(&json!({ "type": "attach-service-port" })).is_err());
        assert!(prepare_init_message(&json!({ "type": "init-local", "hostId": "h" })).is_err());
        assert!(prepare_init_message(&json!("init-local")).is_err());
    }
}
