//! Phase 5 plugin UI sandbox: the shell side of the host-node sandbox owner.
//!
//! The vendored `ZCode` sandbox registry, CSP builders and protocol handler run
//! inside the host-node sidecar; see
//! `Worse/docs/specs/harness/phase-5-plugins.md`. This module only:
//! - serves the `hsandbox` URI scheme by forwarding each request to the
//!   sidecar (`harness-web-remote-sandbox-call {method:"fetch"}`) and
//!   returning its bytes and headers unchanged;
//! - forwards the renderer's `dispose` / `resetOwner` calls.
//!
//! Camera / microphone / geolocation are not granted here: the renderer
//! delegates only `clipboard-write` to the shell iframe, so Permissions
//! Policy blocks them in every sandbox frame (spec section 2). A global
//! permission handler is deliberately not used, because the app's own speech
//! input needs the microphone.
//!
//! It keeps no sandbox state; the pending map lives only while a call is in
//! flight.

#![allow(clippy::needless_pass_by_value)]

#[cfg(not(any(target_os = "android", target_os = "ios")))]
// 通配导入：tauri::command 生成的隐藏宏（__cmd__*）也要一并导出，generate_handler! 才能找到。
pub(crate) use desktop::*;

/// Mobile shells own no Host and therefore no plugin UI sandbox.
#[cfg(any(target_os = "android", target_os = "ios"))]
#[tauri::command]
#[allow(clippy::unused_async)]
pub(crate) async fn plugin_sandbox_call(
    method: String,
) -> Result<(), harness_protocol::CommandError> {
    Err(crate::error::unsupported(format!(
        "plugin sandbox `{method}` needs a desktop Host"
    )))
}

#[cfg(not(any(target_os = "android", target_os = "ios")))]
mod desktop {
    use std::borrow::Cow;
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Mutex, MutexGuard, PoisonError};
    use std::time::Duration;

    use base64::Engine as _;
    use harness_protocol::{CommandError, ErrorCode};
    use serde_json::{json, Value};
    use tauri::http::{Request, Response, StatusCode};
    use tauri::{AppHandle, Manager, Runtime, State, UriSchemeContext, UriSchemeResponder};
    use tokio::sync::oneshot;

    use crate::state::AppState;

    /// Custom scheme of sandbox documents (`hsandbox://shell-<h32>.localhost/...`;
    /// `WebView2` serves it as `http://hsandbox.shell-<h32>.localhost/...`).
    pub(crate) const SCHEME: &str = "hsandbox";
    const CALL: &str = "harness-web-remote-sandbox-call";
    const RESULT: &str = "harness-web-remote-sandbox-result";
    /// Only the app window may load sandbox documents.
    const MAIN_WEBVIEW: &str = "main";
    const CALL_TIMEOUT: Duration = Duration::from_secs(30);
    /// Renderer-callable methods; `fetch` is reserved for the scheme handler.
    const METHODS: &[&str] = &["dispose", "resetOwner"];

    type Reply = Result<Value, String>;

    /// In-flight call correlation only; entries are removed on reply, send
    /// failure, or timeout.
    #[derive(Default)]
    pub(crate) struct PluginSandboxCalls {
        next: AtomicU64,
        pending: Mutex<HashMap<String, oneshot::Sender<Reply>>>,
    }

    impl PluginSandboxCalls {
        fn pending(&self) -> MutexGuard<'_, HashMap<String, oneshot::Sender<Reply>>> {
            self.pending.lock().unwrap_or_else(PoisonError::into_inner)
        }

        fn begin(&self) -> (String, oneshot::Receiver<Reply>) {
            let id = format!("sandbox-{}", self.next.fetch_add(1, Ordering::Relaxed) + 1);
            let (sender, receiver) = oneshot::channel();
            self.pending().insert(id.clone(), sender);
            (id, receiver)
        }

        fn abandon(&self, id: &str) {
            self.pending().remove(id);
        }

        fn settle(&self, message: &Value) {
            let Some(id) = message.get("id").and_then(Value::as_str) else {
                return;
            };
            let Some(sender) = self.pending().remove(id) else {
                // 调用已超时被放弃，迟到的结果直接丢弃。
                return;
            };
            let reply = if message.get("ok").and_then(Value::as_bool) == Some(true) {
                Ok(message.get("value").cloned().unwrap_or(Value::Null))
            } else {
                Err(message
                    .get("error")
                    .and_then(Value::as_str)
                    .unwrap_or("plugin sandbox call failed")
                    .to_owned())
            };
            let _ignored = sender.send(reply);
        }
    }

    /// Routes `harness-web-remote-sandbox-result` Host messages; other types
    /// are ignored. Called by the Host sink for every prefixed message.
    pub(crate) fn on_host_message<R: Runtime>(app: &AppHandle<R>, message: &Value) {
        if message.get("type").and_then(Value::as_str) == Some(RESULT) {
            if let Some(calls) = app.try_state::<PluginSandboxCalls>() {
                calls.settle(message);
            }
        }
    }

    async fn call<R: Runtime>(
        app: &AppHandle<R>,
        method: &str,
        params: Value,
    ) -> Result<Value, CommandError> {
        let state = app
            .try_state::<AppState>()
            .ok_or_else(|| crate::error::internal("application state is not ready"))?;
        let calls = app
            .try_state::<PluginSandboxCalls>()
            .ok_or_else(|| crate::error::internal("plugin sandbox calls are not ready"))?;
        let controller = state.host().running_controller().ok_or_else(|| {
            crate::error::host(ErrorCode::HostNotStarted, "the local Host is not running")
        })?;
        let (id, receiver) = calls.begin();
        let message = json!({ "type": CALL, "id": id, "method": method, "params": params });
        if let Err(error) = controller.send_web_remote(message).await {
            calls.abandon(&id);
            return Err(crate::error::host(
                ErrorCode::HostProtocolError,
                error.to_string(),
            ));
        }
        match tokio::time::timeout(CALL_TIMEOUT, receiver).await {
            Ok(Ok(Ok(value))) => Ok(value),
            Ok(Ok(Err(message))) => Err(crate::error::internal(message)),
            Ok(Err(_dropped)) => Err(crate::error::host(
                ErrorCode::HostNotStarted,
                "the local Host stopped during the call",
            )),
            Err(_elapsed) => {
                calls.abandon(&id);
                Err(crate::error::host(
                    ErrorCode::HostTimeout,
                    format!("plugin sandbox `{method}` timed out"),
                ))
            }
        }
    }

    /// Renderer → sidecar sandbox calls (`dispose`, `resetOwner`).
    #[tauri::command]
    pub(crate) async fn plugin_sandbox_call(
        app: AppHandle,
        _calls: State<'_, PluginSandboxCalls>,
        method: String,
        params: Option<Value>,
    ) -> Result<Value, CommandError> {
        if !METHODS.contains(&method.as_str()) {
            return Err(crate::error::invalid_input(format!(
                "unknown plugin sandbox method `{method}`"
            )));
        }
        call(&app, &method, params.unwrap_or_else(|| json!({}))).await
    }

    fn plain(status: StatusCode) -> Response<Cow<'static, [u8]>> {
        let mut response = Response::new(Cow::Borrowed(&[][..]));
        *response.status_mut() = status;
        response.headers_mut().insert(
            tauri::http::header::CACHE_CONTROL,
            tauri::http::HeaderValue::from_static("no-store"),
        );
        response
    }

    fn build_response(value: &Value) -> Option<Response<Cow<'static, [u8]>>> {
        let status = u16::try_from(value.get("status")?.as_u64()?).ok()?;
        let body = base64::engine::general_purpose::STANDARD
            .decode(value.get("bodyBase64")?.as_str()?)
            .ok()?;
        let mut builder = Response::builder().status(status);
        for pair in value.get("headers")?.as_array()? {
            let name = pair.get(0)?.as_str()?;
            let header = pair.get(1)?.as_str()?;
            builder = builder.header(name, header);
        }
        builder.body(Cow::Owned(body)).ok()
    }

    fn webview_origin<R: Runtime>(app: &AppHandle<R>, label: &str) -> Option<String> {
        let url = app.get_webview_window(label)?.url().ok()?;
        let origin = url.origin();
        origin.is_tuple().then(|| origin.ascii_serialization())
    }

    /// `hsandbox` scheme handler: forwards one request to the sidecar owner.
    pub(crate) fn handle_scheme<R: Runtime>(
        ctx: UriSchemeContext<'_, R>,
        request: Request<Vec<u8>>,
        responder: UriSchemeResponder,
    ) {
        let app = ctx.app_handle().clone();
        let label = ctx.webview_label().to_owned();
        let url = request.uri().to_string();
        let method = request.method().as_str().to_owned();
        tauri::async_runtime::spawn(async move {
            if label != MAIN_WEBVIEW {
                responder.respond(plain(StatusCode::FORBIDDEN));
                return;
            }
            let Some(embedder_origin) = webview_origin(&app, &label) else {
                responder.respond(plain(StatusCode::FORBIDDEN));
                return;
            };
            let params = json!({ "url": url, "method": method, "embedderOrigin": embedder_origin });
            let response = match call(&app, "fetch", params).await {
                Ok(value) => build_response(&value).unwrap_or_else(|| {
                    tracing::warn!("plugin sandbox returned a malformed response");
                    plain(StatusCode::BAD_GATEWAY)
                }),
                Err(error) => {
                    tracing::warn!(code = ?error.code, "plugin sandbox request failed");
                    plain(StatusCode::BAD_GATEWAY)
                }
            };
            responder.respond(response);
        });
    }
}
