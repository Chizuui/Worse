//! Typed, versioned IPC contract shared by the Harness shell and its frontends.
//!
//! Every public type derives `serde` and `ts_rs::TS`; running
//! `cargo test -p harness-protocol` regenerates the TypeScript bindings under
//! `apps/harness-app/src/generated/`.

mod app;
mod capabilities;
pub mod commands;
mod error;
mod host;
mod requests;
mod version;

pub use app::{AppInfo, SurfaceDto};
pub use capabilities::Capabilities;
pub use error::{CommandError, ErrorCode};
pub use host::{HostAttachmentGrant, HostStateDto, HostStatus};
pub use requests::{
    OpenExternalRequest, RevealPathRequest, SelectDirectoryResponse, SelectFileResponse,
};
pub use version::PROTOCOL_VERSION;

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn app_info_round_trips_with_camel_case_keys() {
        let app = AppInfo {
            name: "Harness".to_owned(),
            version: "0.1.0".to_owned(),
            protocol_version: PROTOCOL_VERSION,
            surface: SurfaceDto::Desktop,
            os: "windows".to_owned(),
        };

        let json = serde_json::to_string(&app).unwrap();

        assert_eq!(
            json,
            concat!(
                r#"{"name":"Harness","version":"0.1.0","protocolVersion":1,"#,
                r#""surface":"desktop","os":"windows"}"#,
            )
        );
        assert_eq!(serde_json::from_str::<AppInfo>(&json).unwrap(), app);
    }
    #[test]
    fn capabilities_round_trip_with_camel_case_keys() {
        let capabilities = Capabilities {
            open_external: true,
            reveal_path: false,
            select_directory: false,
            select_file: false,
            local_host: true,
            notifications: true,
        };

        let json = serde_json::to_string(&capabilities).unwrap();

        assert_eq!(
            json,
            concat!(
                r#"{"openExternal":true,"revealPath":false,"selectDirectory":false,"#,
                r#""selectFile":false,"localHost":true,"notifications":true}"#,
            )
        );
        assert_eq!(
            serde_json::from_str::<Capabilities>(&json).unwrap(),
            capabilities
        );
    }

    #[test]
    fn command_error_serializes_camel_case_code() {
        let error = CommandError {
            code: ErrorCode::InvalidInput,
            message: "unsupported scheme".to_owned(),
        };

        let json = serde_json::to_string(&error).unwrap();

        assert_eq!(
            json,
            r#"{"code":"invalidInput","message":"unsupported scheme"}"#
        );
    }
    #[test]
    fn host_error_codes_serialize_camel_case() {
        let codes = [
            (ErrorCode::HostNotStarted, "hostNotStarted"),
            (ErrorCode::HostSpawnFailed, "hostSpawnFailed"),
            (ErrorCode::HostProtocolError, "hostProtocolError"),
            (ErrorCode::HostTimeout, "hostTimeout"),
        ];

        for (code, expected) in codes {
            let json = serde_json::to_string(&code).unwrap();
            assert_eq!(json, format!("\"{expected}\""));
        }
    }

    #[test]
    fn host_attachment_grant_round_trips_with_camel_case_keys() {
        let grant = HostAttachmentGrant {
            attachment_id: "attachment-1".to_owned(),
            url: "ws://127.0.0.1:4242/v1/attach".to_owned(),
            token: "a".repeat(64),
            database_startup_id: "startup-1".to_owned(),
            startup_state: Some(serde_json::json!({ "phase": "starting" })),
        };

        let json = serde_json::to_string(&grant).unwrap();
        let expected = format!(
            concat!(
                r#"{{"attachmentId":"attachment-1","#,
                r#""url":"ws://127.0.0.1:4242/v1/attach","#,
                r#""token":"{}","#,
                r#""databaseStartupId":"startup-1","#,
                r#""startupState":{{"phase":"starting"}}}}"#,
            ),
            "a".repeat(64)
        );

        assert_eq!(json, expected);
        assert_eq!(
            serde_json::from_str::<HostAttachmentGrant>(&json).unwrap(),
            grant
        );
    }
    #[test]
    fn host_status_serializes_state_and_errors() {
        let ready = HostStatus {
            state: HostStateDto::Ready,
            error_code: None,
            error_message: None,
        };
        assert_eq!(
            serde_json::to_string(&ready).unwrap(),
            r#"{"state":"ready","errorCode":null,"errorMessage":null}"#
        );

        let failed = HostStatus {
            state: HostStateDto::Failed,
            error_code: Some("hostSpawnFailed".to_owned()),
            error_message: Some("node is missing".to_owned()),
        };
        assert_eq!(
            serde_json::to_string(&failed).unwrap(),
            concat!(
                r#"{"state":"failed","errorCode":"hostSpawnFailed","#,
                r#""errorMessage":"node is missing"}"#,
            )
        );

        let parsed = serde_json::from_str::<HostStatus>(
            r#"{"state":"starting","errorCode":null,"errorMessage":null}"#,
        )
        .unwrap();
        assert_eq!(parsed.state, HostStateDto::Starting);
    }

    #[test]
    fn requests_and_cancelled_selection_round_trip() {
        let request = OpenExternalRequest {
            url: "https://example.com".to_owned(),
        };
        let json = serde_json::to_string(&request).unwrap();
        assert_eq!(json, r#"{"url":"https://example.com"}"#);
        assert_eq!(
            serde_json::from_str::<OpenExternalRequest>(&json).unwrap(),
            request
        );

        let cancelled = SelectDirectoryResponse { path: None };
        let json = serde_json::to_string(&cancelled).unwrap();
        assert_eq!(json, r#"{"path":null}"#);
        assert_eq!(
            serde_json::from_str::<SelectDirectoryResponse>(&json).unwrap(),
            cancelled
        );
    }
}
