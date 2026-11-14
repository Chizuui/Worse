//! JSON-RPC envelope types and the error model of the Phase 3a contract.

use harness_fs::FsError;
use harness_search::SearchError;
use serde_json::{json, Value};

/// Wire protocol version announced in `initialize`.
pub const PROTOCOL_VERSION: u64 = 1;

/// Error codes (contract section 4).
pub mod codes {
    /// Invalid JSON line.
    pub const PARSE_ERROR: i64 = -32700;
    /// Not a valid request object.
    pub const INVALID_REQUEST: i64 = -32600;
    /// Unknown method.
    pub const METHOD_NOT_FOUND: i64 = -32601;
    /// Params do not match the method.
    pub const INVALID_PARAMS: i64 = -32602;
    /// Unexpected server failure.
    pub const INTERNAL_ERROR: i64 = -32603;
    /// Request before `initialize`.
    pub const SERVER_NOT_INITIALIZED: i64 = -32002;
    /// Request cancelled through `$/cancel`.
    pub const REQUEST_CANCELLED: i64 = -32800;
    /// Node-style system error (`data: {errno, syscall, path}`).
    pub const IO_ERROR: i64 = 1;
    /// TS `throw new Error(message)` equivalent.
    pub const APP_ERROR: i64 = 2;
}

/// A structured JSON-RPC error.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
#[error("rpc error {code}: {message}")]
pub struct RpcError {
    /// Error code.
    pub code: i64,
    /// Human-readable message.
    pub message: String,
    /// Optional structured data.
    pub data: Option<Value>,
}

impl RpcError {
    /// Error without data.
    #[must_use]
    pub fn new(code: i64, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            data: None,
        }
    }

    /// `-32602` with a description of the bad params.
    #[must_use]
    pub fn invalid_params(message: impl Into<String>) -> Self {
        Self::new(codes::INVALID_PARAMS, message)
    }

    /// `-32603`.
    #[must_use]
    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(codes::INTERNAL_ERROR, message)
    }

    /// `-32800`.
    #[must_use]
    pub fn cancelled() -> Self {
        Self::new(codes::REQUEST_CANCELLED, "Request cancelled")
    }

    /// `AppError` with the TS message text.
    #[must_use]
    pub fn app(message: impl Into<String>) -> Self {
        Self::new(codes::APP_ERROR, message)
    }

    /// JSON form of the error object.
    #[must_use]
    pub fn to_json(&self) -> Value {
        let mut error = json!({"code": self.code, "message": self.message});
        if let (Some(data), Some(map)) = (&self.data, error.as_object_mut()) {
            map.insert("data".to_owned(), data.clone());
        }
        error
    }
}

impl From<FsError> for RpcError {
    fn from(error: FsError) -> Self {
        let message = error.to_string();
        match error {
            FsError::Io {
                errno,
                syscall,
                path,
            } => Self {
                code: codes::IO_ERROR,
                message,
                data: Some(json!({"errno": errno, "syscall": syscall, "path": path})),
            },
            FsError::App { message } => Self::app(message),
        }
    }
}

impl From<SearchError> for RpcError {
    fn from(error: SearchError) -> Self {
        match error {
            SearchError::Cancelled => Self::cancelled(),
            SearchError::Fs(fs) => fs.into(),
            SearchError::InvalidPattern(message) => Self::app(message),
        }
    }
}

/// Success response line.
#[must_use]
pub fn response(id: &Value, result: Value) -> Value {
    let mut map = serde_json::Map::new();
    map.insert("id".to_owned(), id.clone());
    map.insert("result".to_owned(), result);
    Value::Object(map)
}

/// Error response line.
#[must_use]
pub fn error_response(id: &Value, error: &RpcError) -> Value {
    json!({"id": id, "error": error.to_json()})
}
