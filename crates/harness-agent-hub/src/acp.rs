//! Minimal ACP client over a child's stdio (newline-delimited JSON-RPC 2.0).
//!
//! Ordering contract (spec §9): the reader task hands every `session/update` notification and every agent
//! request for a session to that session's route **synchronously and in wire order**, and only then
//! resolves the oneshot of a response. A consumer that posts "request finished" into the same FIFO as the
//! route therefore always observes all earlier updates first.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Child;
use tokio::sync::{mpsc, oneshot};

use crate::config::SHUTDOWN_GRACE;
use crate::process::{kill_tree, spawn_piped};
use crate::quota::AgentErrorInfo;

/// ACP protocol version we speak.
pub const ACP_PROTOCOL_VERSION: u64 = 1;

/// JSON-RPC error from the agent, or a transport failure.
#[derive(Debug, Clone)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
    pub data: Option<Value>,
}

impl RpcError {
    /// Transport closed (agent exited).
    pub const EXITED: i64 = -32099;

    fn exited() -> Self {
        Self {
            code: Self::EXITED,
            message: "agent process exited".into(),
            data: None,
        }
    }

    /// As classification input.
    #[must_use]
    pub fn info(&self) -> AgentErrorInfo {
        AgentErrorInfo {
            code: Some(self.code),
            message: self.message.clone(),
            data: self.data.clone(),
        }
    }
}

impl std::fmt::Display for RpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} ({})", self.message, self.code)
    }
}

/// Something the agent sent for one ACP session.
#[derive(Debug)]
pub enum Inbound {
    /// `session/update` notification payload (`params.update`).
    Update { native: String, update: Value },
    /// Agent→client request (e.g. `session/request_permission`); answer with [`AcpConnection::respond`].
    Request {
        native: String,
        id: Value,
        method: String,
        params: Value,
    },
}

/// Per-session sink called by the reader task. Must not block.
pub type RouteSink = Arc<dyn Fn(Inbound) + Send + Sync>;

type Pending = HashMap<u64, oneshot::Sender<Result<Value, RpcError>>>;

#[derive(Default)]
struct ConnState {
    next_id: u64,
    pending: Pending,
    routes: HashMap<String, RouteSink>,
    closed: bool,
}

/// One running ACP agent process.
pub struct AcpConnection {
    generation: u64,
    pid: Option<u32>,
    state: Arc<Mutex<ConnState>>,
    writer: Mutex<Option<mpsc::UnboundedSender<String>>>,
    child: tokio::sync::Mutex<Option<Child>>,
    agent_capabilities: Mutex<Value>,
    /// `agentInfo` from `initialize` (name, version).
    agent_info: Mutex<Value>,
    steering: std::sync::atomic::AtomicBool,
    /// Last stderr lines (at most [`STDERR_TAIL_LINES`]), the `trace` of a typed failure (§9.2.5 R20).
    stderr_tail: Arc<Mutex<std::collections::VecDeque<String>>>,
}

/// Stderr lines kept per agent process for failure traces.
const STDERR_TAIL_LINES: usize = 40;
/// Longest kept stderr line (chars).
const STDERR_LINE_CHARS: usize = 300;

impl std::fmt::Debug for AcpConnection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AcpConnection")
            .field("generation", &self.generation)
            .field("pid", &self.pid)
            .finish_non_exhaustive()
    }
}

impl AcpConnection {
    /// Spawns the agent and starts reader/writer tasks. `on_exit` runs once when stdout closes.
    ///
    /// # Errors
    ///
    /// Spawn failures.
    pub fn spawn(
        program: &Path,
        args: &[String],
        env: &[(String, String)],
        generation: u64,
        on_exit: Box<dyn FnOnce() + Send>,
    ) -> std::io::Result<Arc<Self>> {
        let mut child = spawn_piped(program, args, env, None)?;
        let pid = child.id();
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| std::io::Error::other("agent stdin missing"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| std::io::Error::other("agent stdout missing"))?;
        let stderr = child.stderr.take();
        let (tx, mut rx) = mpsc::unbounded_channel::<String>();
        let state = Arc::new(Mutex::new(ConnState::default()));
        let stderr_tail = Arc::new(Mutex::new(std::collections::VecDeque::new()));
        let connection = Arc::new(Self {
            generation,
            pid,
            state: Arc::clone(&state),
            writer: Mutex::new(Some(tx)),
            child: tokio::sync::Mutex::new(Some(child)),
            agent_capabilities: Mutex::new(Value::Null),
            agent_info: Mutex::new(Value::Null),
            steering: std::sync::atomic::AtomicBool::new(false),
            stderr_tail: Arc::clone(&stderr_tail),
        });

        tokio::spawn(async move {
            let mut stdin = stdin;
            while let Some(line) = rx.recv().await {
                if stdin.write_all(line.as_bytes()).await.is_err()
                    || stdin.write_all(b"\n").await.is_err()
                    || stdin.flush().await.is_err()
                {
                    break;
                }
            }
            // Dropping stdin = EOF for the agent (graceful shutdown signal).
        });

        if let Some(stderr) = stderr {
            tokio::spawn(async move {
                let mut lines = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    tracing::debug!(target: "harness_agent_stderr", pid = ?pid, "{line}");
                    if let Ok(mut tail) = stderr_tail.lock() {
                        if tail.len() == STDERR_TAIL_LINES {
                            tail.pop_front();
                        }
                        tail.push_back(line.chars().take(STDERR_LINE_CHARS).collect());
                    }
                }
            });
        }

        let reader_state = Arc::clone(&state);
        let weak = Arc::downgrade(&connection);
        tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                if line.trim().is_empty() {
                    continue;
                }
                match serde_json::from_str::<Value>(&line) {
                    Ok(message) => dispatch(&reader_state, &weak, &message),
                    Err(error) => tracing::debug!(%error, "non-JSON line from agent"),
                }
            }
            let pending = match reader_state.lock() {
                Ok(mut s) => {
                    s.closed = true;
                    s.routes.clear();
                    std::mem::take(&mut s.pending)
                }
                Err(_) => Pending::new(),
            };
            for (_, sender) in pending {
                let _ = sender.send(Err(RpcError::exited()));
            }
            on_exit();
        });
        Ok(connection)
    }

    /// Last stderr lines of the process, `None` when it wrote none.
    #[must_use]
    pub fn stderr_tail(&self) -> Option<String> {
        let tail = self.stderr_tail.lock().ok()?;
        (!tail.is_empty()).then(|| tail.iter().cloned().collect::<Vec<_>>().join("\n"))
    }

    /// Pool generation of this process.
    #[must_use]
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// OS process id.
    #[must_use]
    pub fn pid(&self) -> Option<u32> {
        self.pid
    }

    /// True after the agent's stdout closed.
    #[must_use]
    pub fn is_closed(&self) -> bool {
        self.state.lock().map_or(true, |s| s.closed)
    }

    /// `agentCapabilities.loadSession` from `initialize`.
    #[must_use]
    pub fn supports_load(&self) -> bool {
        self.agent_capabilities
            .lock()
            .ok()
            .and_then(|c| c.get("loadSession").and_then(Value::as_bool))
            .unwrap_or(false)
    }

    /// `agentCapabilities.promptCapabilities` from `initialize` (image, embeddedContext); phase-9-parity.md §9.2.1.
    #[must_use]
    pub fn prompt_capabilities(&self) -> (bool, bool) {
        self.agent_capabilities
            .lock()
            .ok()
            .map_or((false, false), |c| {
                let flag = |k: &str| {
                    c.get("promptCapabilities")
                        .and_then(|m| m.get(k))
                        .and_then(Value::as_bool)
                        .unwrap_or(false)
                };
                (flag("image"), flag("embeddedContext"))
            })
    }

    /// `agentCapabilities.mcpCapabilities` from `initialize` (http, sse).
    #[must_use]
    pub fn mcp_capabilities(&self) -> (bool, bool) {
        self.agent_capabilities
            .lock()
            .ok()
            .map_or((false, false), |c| {
                let flag = |k: &str| {
                    c.get("mcpCapabilities")
                        .and_then(|m| m.get(k))
                        .and_then(Value::as_bool)
                        .unwrap_or(false)
                };
                (flag("http"), flag("sse"))
            })
    }

    #[must_use]
    pub fn supports_steering(&self) -> bool {
        self.steering.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Enqueues a request (synchronously, in order) and returns the receiver of its response.
    pub fn send_request(
        &self,
        method: &str,
        params: Value,
    ) -> oneshot::Receiver<Result<Value, RpcError>> {
        let (tx, rx) = oneshot::channel();
        let id = {
            let Ok(mut state) = self.state.lock() else {
                let _ = tx.send(Err(RpcError::exited()));
                return rx;
            };
            if state.closed {
                drop(state);
                let _ = tx.send(Err(RpcError::exited()));
                return rx;
            }
            state.next_id += 1;
            let id = state.next_id;
            state.pending.insert(id, tx);
            id
        };
        let line = envelope(vec![
            ("id", Value::from(id)),
            ("method", Value::from(method)),
            ("params", params),
        ]);
        if !self.write(line) {
            if let Ok(mut state) = self.state.lock() {
                if let Some(sender) = state.pending.remove(&id) {
                    let _ = sender.send(Err(RpcError::exited()));
                }
            }
        }
        rx
    }

    /// Request and await its response.
    ///
    /// # Errors
    ///
    /// The agent's JSON-RPC error or [`RpcError::EXITED`].
    pub async fn call(&self, method: &str, params: Value) -> Result<Value, RpcError> {
        self.send_request(method, params)
            .await
            .unwrap_or_else(|_| Err(RpcError::exited()))
    }

    /// Sends a notification (e.g. `session/cancel`).
    pub fn notify(&self, method: &str, params: Value) {
        let _ = self.write(envelope(vec![
            ("method", Value::from(method)),
            ("params", params),
        ]));
    }

    /// Answers an agent→client request.
    pub fn respond(&self, id: &Value, result: Value) {
        let _ = self.write(envelope(vec![("id", id.clone()), ("result", result)]));
    }

    /// Answers an agent→client request with an error.
    pub fn respond_error(&self, id: &Value, code: i64, message: &str) {
        let _ = self.write(
            json!({"jsonrpc":"2.0","id":id,"error":{"code":code,"message":message}}).to_string(),
        );
    }

    /// Routes inbound traffic of one ACP session to `sink` (replaces an earlier route).
    pub fn register_route(&self, native: &str, sink: RouteSink) {
        if let Ok(mut state) = self.state.lock() {
            state.routes.insert(native.to_owned(), sink);
        }
    }

    /// ACP `initialize`.
    ///
    /// # Errors
    ///
    /// Agent errors.
    pub async fn initialize(&self) -> Result<(), RpcError> {
        let result = self
            .call(
                "initialize",
                json!({
                    "protocolVersion": ACP_PROTOCOL_VERSION,
                    "clientCapabilities": {"fs": {"readTextFile": false, "writeTextFile": false}, "terminal": false, "_meta": {"jetbrains": {"air": {"version": 1, "capabilities": ["asyncTasks"]}}}},
                    "clientInfo": {"name": "harness", "version": env!("CARGO_PKG_VERSION")}
                }),
            )
            .await?;
        if let Ok(mut caps) = self.agent_capabilities.lock() {
            *caps = result
                .get("agentCapabilities")
                .cloned()
                .unwrap_or(Value::Null);
        }
        if let Ok(mut info) = self.agent_info.lock() {
            *info = result.get("agentInfo").cloned().unwrap_or(Value::Null);
        }
        let steering = result
            .pointer("/_meta/steering/supported")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        self.steering
            .store(steering, std::sync::atomic::Ordering::Relaxed);
        Ok(())
    }

    /// `agentInfo.name` / `agentInfo.version` from `initialize` (phase-9-parity.md §9.1.5a).
    #[must_use]
    pub fn agent_info(&self) -> (Option<String>, Option<String>) {
        self.agent_info.lock().ok().map_or((None, None), |info| {
            let field = |k: &str| info.get(k).and_then(Value::as_str).map(str::to_owned);
            (field("name"), field("version"))
        })
    }

    /// Closes stdin, waits the grace period, then kills the process tree.
    pub async fn shutdown(&self) {
        if let Ok(mut writer) = self.writer.lock() {
            writer.take();
        }
        let mut guard = self.child.lock().await;
        let Some(mut child) = guard.take() else {
            return;
        };
        if tokio::time::timeout(SHUTDOWN_GRACE, child.wait())
            .await
            .is_err()
        {
            if let Some(pid) = child.id() {
                kill_tree(pid).await;
            }
            let _ = tokio::time::timeout(Duration::from_secs(5), child.wait()).await;
        }
    }

    fn write(&self, line: String) -> bool {
        self.writer
            .lock()
            .ok()
            .and_then(|w| w.as_ref().map(|tx| tx.send(line).is_ok()))
            .unwrap_or(false)
    }
}

fn dispatch(state: &Arc<Mutex<ConnState>>, weak: &std::sync::Weak<AcpConnection>, message: &Value) {
    let method = message
        .get("method")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let id = message.get("id").cloned();
    match (method, id) {
        (None, Some(id)) => {
            let Some(id) = id.as_u64() else { return };
            let sender = state.lock().ok().and_then(|mut s| s.pending.remove(&id));
            if let Some(sender) = sender {
                let result = if let Some(error) = message.get("error") {
                    Err(RpcError {
                        code: error.get("code").and_then(Value::as_i64).unwrap_or(-32603),
                        message: error
                            .get("message")
                            .and_then(Value::as_str)
                            .unwrap_or("agent error")
                            .to_owned(),
                        data: error.get("data").cloned(),
                    })
                } else {
                    Ok(message.get("result").cloned().unwrap_or(Value::Null))
                };
                let _ = sender.send(result);
            }
        }
        (Some(method), id) => {
            let params = message.get("params").cloned().unwrap_or(Value::Null);
            let native = params
                .get("sessionId")
                .and_then(Value::as_str)
                .map(str::to_owned);
            let sink = native
                .as_ref()
                .and_then(|n| state.lock().ok().and_then(|s| s.routes.get(n).cloned()));
            match (id, sink, native) {
                (None, Some(sink), Some(native)) if method == "session/update" => {
                    let update = params.get("update").cloned().unwrap_or(Value::Null);
                    sink(Inbound::Update { native, update });
                }
                (None, _, _) => {} // unrouted notification (e.g. replay during session/load): dropped
                (Some(id), Some(sink), Some(native)) => sink(Inbound::Request {
                    native,
                    id,
                    method,
                    params,
                }),
                (Some(id), _, _) => {
                    if let Some(connection) = weak.upgrade() {
                        connection.respond_error(&id, -32601, "method not supported by Harness");
                    }
                }
            }
        }
        (None, None) => {}
    }
}

/// Serializes a JSON-RPC 2.0 message, moving the field values.
fn envelope(fields: Vec<(&str, Value)>) -> String {
    let mut map = serde_json::Map::new();
    map.insert("jsonrpc".into(), Value::from("2.0"));
    for (key, value) in fields {
        map.insert(key.into(), value);
    }
    Value::Object(map).to_string()
}
