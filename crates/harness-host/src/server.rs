//! NDJSON JSON-RPC server loop (contract sections 1-5).
//!
//! ```text
//! stdin thread --lines--> dispatch loop --spawn--> blocking handler --+--> out channel --> stdout thread
//!                            |  $/cancel -> CancellationToken          |  $/progress, $/pty/*, $/watch/* (FIFO)
//! ```
//!
//! A single writer thread owns stdout, so every `$/progress` for a request is
//! written before its response (both go through the same FIFO channel, and the
//! response is queued only after the handler returned).
//!
//! stdin and stdout are served by dedicated OS threads with plain blocking IO.
//! `tokio::io::{stdin, stdout}` would run every read and every write+flush as a
//! separate blocking-pool task; for PTY output (one `$/pty/output` per `ConPTY`
//! write, often 50-80 per typed line) those hand-offs sit on the typing path.
//! The writer drains whatever is already queued into one write + flush, so a
//! burst costs one syscall; nothing ever waits for more data (no timer).

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use serde_json::{json, Value};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinSet;

use harness_search::CancellationToken;

use crate::methods::{self, AfterResponse, RequestCtx, Services};
use crate::rpc::{self, codes, RpcError, PROTOCOL_VERSION};

/// Maximum accepted line length (contract: 64 MiB).
pub const MAX_LINE_BYTES: usize = 64 * 1024 * 1024;
/// Time allowed for in-flight work to drain after EOF or `shutdown`.
const DRAIN_TIMEOUT: Duration = Duration::from_millis(1500);
/// Upper bound of one coalesced stdout write (already-queued messages only).
const WRITE_BATCH_BYTES: usize = 256 * 1024;

/// Why the server loop ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExitReason {
    /// stdin reached EOF.
    Eof,
    /// A `shutdown` request was answered.
    Shutdown,
    /// stdin failed.
    ReadError,
}

struct Shared {
    services: Services,
    initialized: AtomicBool,
    in_flight: Mutex<HashMap<u64, CancellationToken>>,
    /// Per-session FIFO: key -> (sequence, completion of the latest request).
    chains: Mutex<HashMap<String, (u64, oneshot::Receiver<()>)>>,
    next_seq: AtomicU64,
}

impl Shared {
    fn in_flight(&self) -> MutexGuard<'_, HashMap<u64, CancellationToken>> {
        self.in_flight
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn chains(&self) -> MutexGuard<'_, HashMap<String, (u64, oneshot::Receiver<()>)>> {
        self.chains
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

type Out = mpsc::UnboundedSender<Value>;

/// One stdin event from the reader thread.
enum Inbound {
    Line(Vec<u8>),
    Eof,
    Failed(std::io::Error),
}

/// Runs the server until EOF, `shutdown` or a read error. `reader` and
/// `writer` are blocking streams (stdin/stdout), each served by its own thread.
pub async fn serve<R, W>(reader: R, writer: W) -> ExitReason
where
    R: Read + Send + 'static,
    W: Write + Send + 'static,
{
    let (out_tx, out_rx) = mpsc::unbounded_channel::<Value>();
    let (writer_done_tx, writer_done) = oneshot::channel::<()>();
    if let Err(error) = std::thread::Builder::new()
        .name("harness-host-stdout".to_owned())
        .spawn(move || {
            write_loop(writer, out_rx);
            let _ = writer_done_tx.send(());
        })
    {
        tracing::error!(%error, "failed to start the stdout thread");
        return ExitReason::ReadError;
    }
    let (in_tx, mut in_rx) = mpsc::unbounded_channel::<Inbound>();
    if let Err(error) = std::thread::Builder::new()
        .name("harness-host-stdin".to_owned())
        .spawn(move || read_loop(reader, &in_tx))
    {
        tracing::error!(%error, "failed to start the stdin thread");
        return ExitReason::ReadError;
    }
    let shared = Arc::new(Shared {
        services: Services::default(),
        initialized: AtomicBool::new(false),
        in_flight: Mutex::new(HashMap::new()),
        chains: Mutex::new(HashMap::new()),
        next_seq: AtomicU64::new(0),
    });
    let mut tasks = JoinSet::new();
    let reason = loop {
        let line = match in_rx.recv().await {
            Some(Inbound::Line(line)) => line,
            Some(Inbound::Eof) | None => break ExitReason::Eof,
            Some(Inbound::Failed(error)) => {
                tracing::error!(%error, "stdin read failed");
                break ExitReason::ReadError;
            }
        };
        while tasks.try_join_next().is_some() {}
        if line.len() > MAX_LINE_BYTES {
            send_error(
                &out_tx,
                &Value::Null,
                &RpcError::new(codes::INVALID_REQUEST, "line too long"),
            );
            continue;
        }
        if handle_line(&line, &shared, &out_tx, &mut tasks) {
            break ExitReason::Shutdown;
        }
    };
    tracing::info!(?reason, "harness-host stopping");
    for token in shared.in_flight().values() {
        token.cancel();
    }
    // 契约 r2 §9.4：退出前关闭全部 watcher、结束全部 PTY 会话（不再发通知），与在途请求共享 1.5 秒排空期限。
    let dispose_shared = Arc::clone(&shared);
    let dispose = tokio::task::spawn_blocking(move || dispose_shared.services.dispose_sessions());
    let _ = tokio::time::timeout(DRAIN_TIMEOUT, async {
        while tasks.join_next().await.is_some() {}
        let _ = dispose.await;
    })
    .await;
    drop(out_tx);
    let _ = tokio::time::timeout(Duration::from_millis(300), writer_done).await;
    reason
}

/// stdin thread: one NDJSON line per event; ends after EOF, an error, or when
/// the dispatch loop is gone.
fn read_loop<R: Read>(reader: R, events: &mpsc::UnboundedSender<Inbound>) {
    let mut reader = BufReader::with_capacity(64 * 1024, reader);
    loop {
        let mut line = Vec::new();
        let event = match reader.read_until(b'\n', &mut line) {
            Ok(0) => Inbound::Eof,
            Ok(_) => Inbound::Line(line),
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => Inbound::Failed(error),
        };
        let last = !matches!(event, Inbound::Line(_));
        if events.send(event).is_err() || last {
            return;
        }
    }
}

fn append_message(buf: &mut Vec<u8>, message: &Value) {
    let start = buf.len();
    match serde_json::to_writer(&mut *buf, message) {
        Ok(()) => buf.push(b'\n'),
        Err(error) => {
            buf.truncate(start);
            tracing::error!(%error, "failed to serialize outgoing message");
        }
    }
}

/// stdout thread: blocks for the next message, appends everything already
/// queued (up to [`WRITE_BATCH_BYTES`]), then one write + flush. Order is the
/// channel's FIFO order; a full pipe blocks only this thread.
fn write_loop<W: Write>(mut writer: W, mut rx: mpsc::UnboundedReceiver<Value>) {
    let mut buf = Vec::with_capacity(64 * 1024);
    while let Some(message) = rx.blocking_recv() {
        buf.clear();
        append_message(&mut buf, &message);
        while buf.len() < WRITE_BATCH_BYTES {
            match rx.try_recv() {
                Ok(message) => append_message(&mut buf, &message),
                Err(_) => break,
            }
        }
        if buf.is_empty() {
            continue;
        }
        if writer
            .write_all(&buf)
            .and_then(|()| writer.flush())
            .is_err()
        {
            tracing::warn!("stdout closed; dropping further output");
            return;
        }
        if buf.capacity() > 4 * WRITE_BATCH_BYTES {
            buf = Vec::with_capacity(64 * 1024);
        }
    }
}

fn send_error(out: &Out, id: &Value, error: &RpcError) {
    let _ = out.send(rpc::error_response(id, error));
}

fn parse_id(value: &Value) -> Option<u64> {
    value.as_u64()
}

/// Handles one input line. Returns `true` after answering `shutdown`.
fn handle_line(line: &[u8], shared: &Arc<Shared>, out: &Out, tasks: &mut JoinSet<()>) -> bool {
    let text = String::from_utf8_lossy(line);
    let text = text.trim();
    if text.is_empty() {
        return false;
    }
    let message: Value = match serde_json::from_str(text) {
        Ok(value) => value,
        Err(error) => {
            send_error(
                out,
                &Value::Null,
                &RpcError::new(codes::PARSE_ERROR, format!("Parse error: {error}")),
            );
            return false;
        }
    };
    let Some(object) = message.as_object() else {
        send_error(
            out,
            &Value::Null,
            &RpcError::new(codes::INVALID_REQUEST, "message must be an object"),
        );
        return false;
    };
    let method = object.get("method").and_then(Value::as_str);
    let params = object.get("params").cloned().unwrap_or(Value::Null);
    let Some(raw_id) = object.get("id").filter(|v| !v.is_null()) else {
        if method == Some("$/cancel") {
            if let Some(id) = params.get("id").and_then(parse_id) {
                if let Some(token) = shared.in_flight().get(&id) {
                    tracing::debug!(id, "cancel requested");
                    token.cancel();
                }
            }
        }
        return false;
    };
    let (Some(id), Some(method)) = (parse_id(raw_id), method) else {
        send_error(
            out,
            raw_id,
            &RpcError::new(
                codes::INVALID_REQUEST,
                "invalid request: id must be a non-negative integer and method a string",
            ),
        );
        return false;
    };
    let id_value = json!(id);
    match method {
        "initialize" => {
            shared.initialized.store(true, Ordering::Release);
            let requested = params.get("protocolVersion").and_then(Value::as_u64);
            tracing::info!(?requested, "initialize");
            let _ = out.send(rpc::response(
                &id_value,
                json!({
                    "protocolVersion": PROTOCOL_VERSION,
                    "server": "harness-host",
                    "serverVersion": env!("CARGO_PKG_VERSION"),
                    "services": methods::service_names(),
                    "methods": methods::method_names(),
                }),
            ));
            return false;
        }
        _ if !shared.initialized.load(Ordering::Acquire) => {
            send_error(
                out,
                &id_value,
                &RpcError::new(codes::SERVER_NOT_INITIALIZED, "Server not initialized"),
            );
            return false;
        }
        "shutdown" => {
            let _ = out.send(rpc::response(&id_value, Value::Null));
            return true;
        }
        "ping" => {
            let _ = out.send(rpc::response(&id_value, json!({"pong": true})));
            return false;
        }
        _ => {}
    }
    let Some(handler) = methods::lookup(method) else {
        send_error(
            out,
            &id_value,
            &RpcError::new(
                codes::METHOD_NOT_FOUND,
                format!("Method not found: {method}"),
            ),
        );
        return false;
    };
    spawn_request(id, method, handler, params, shared, out, tasks);
    false
}

/// Runs one request handler on the blocking pool and queues its response.
fn spawn_request(
    id: u64,
    method: &str,
    handler: methods::Handler,
    params: Value,
    shared: &Arc<Shared>,
    out: &Out,
    tasks: &mut JoinSet<()>,
) {
    let token = CancellationToken::new();
    shared.in_flight().insert(id, token.clone());
    // 在读循环里同步登记链条，保证同一会话的请求顺序就是到达顺序。
    let (done_tx, done_rx) = oneshot::channel::<()>();
    let mut previous = None;
    let chain_key = methods::session_key(method, &params).map(|key| {
        let seq = shared.next_seq.fetch_add(1, Ordering::Relaxed);
        if let Some((_, rx)) = shared.chains().insert(key.clone(), (seq, done_rx)) {
            previous = Some(rx);
        }
        (key, seq)
    });
    let shared = Arc::clone(shared);
    let out = out.clone();
    let method = method.to_owned();
    tasks.spawn(async move {
        let progress_out = out.clone();
        let task_shared = Arc::clone(&shared);
        let task_token = token.clone();
        if let Some(previous) = previous {
            // 同一 watchId/sessionId 的请求按到达顺序执行（契约 r2 变更 3）。
            let _ = previous.await;
        }
        let ctx_out = out.clone();
        let after_slot: Arc<Mutex<Option<AfterResponse>>> = Arc::new(Mutex::new(None));
        let task_after = Arc::clone(&after_slot);
        let joined = tokio::task::spawn_blocking(move || {
            let progress = move |value: Value| {
                let _ = progress_out.send(json!({
                    "method": "$/progress",
                    "params": {"id": id, "value": value},
                }));
            };
            let ctx = RequestCtx {
                services: &task_shared.services,
                cancel: task_token,
                progress: &progress,
                out: &ctx_out,
                after_response: &task_after,
            };
            handler(&ctx, params)
        })
        .await;
        shared.in_flight().remove(&id);
        let result = joined.unwrap_or_else(|error| {
            tracing::error!(%error, method, "handler panicked");
            Err(RpcError::internal(format!("handler failed: {error}")))
        });
        let message = match result {
            Ok(value) => rpc::response(&json!(id), value),
            Err(error) => {
                if error.code != codes::IO_ERROR && error.code != codes::APP_ERROR {
                    tracing::debug!(method, code = error.code, message = %error.message, "request failed");
                }
                rpc::error_response(&json!(id), &error)
            }
        };
        let _ = out.send(message);
        let after = after_slot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if let Some(after) = after {
            after();
        }
        if let Some((key, seq)) = chain_key {
            let _ = done_tx.send(());
            let mut chains = shared.chains();
            if chains.get(&key).is_some_and(|(s, _)| *s == seq) {
                chains.remove(&key);
            }
        }
    });
}
