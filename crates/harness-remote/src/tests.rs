//! In-crate integration tests: a stub `HarnessCore`, the real loopback server, plain HTTP/1.1 and
//! WebSocket clients written against the socket (no extra test dependencies).

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use harness_core_api::{
    AgentId, ApiError, ApiResult, BoxFuture, CreateSession, EventKind, EventStream, HarnessCore,
    HubEvent, ListSessions, MemoryScope, MemoryWriteMode, PermissionRequestId, SendMessage,
    SessionId, SessionSummary, TurnId,
};
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::broadcast;

use crate::{RemoteConfig, RemoteHandle};

const TIMEOUT: Duration = Duration::from_secs(10);

// ---------------------------------------------------------------- stub core

/// Minimal `HarnessCore`: a session list, an event sender; everything else is unsupported.
struct StubCore {
    tx: broadcast::Sender<HubEvent>,
    seq: AtomicU64,
}

impl StubCore {
    fn new() -> Arc<Self> {
        let (tx, _) = broadcast::channel(64);
        Arc::new(Self {
            tx,
            seq: AtomicU64::new(0),
        })
    }

    fn publish(&self, kind: EventKind) {
        let seq = self.seq.fetch_add(1, Ordering::Relaxed) + 1;
        let _ = self.tx.send(HubEvent {
            seq,
            session_id: None,
            kind,
        });
    }
}

fn summary() -> SessionSummary {
    SessionSummary {
        id: SessionId::from("s1"),
        title: "Test session".to_owned(),
        workspace_path: "C:/work".to_owned(),
        workspace_key: "wk".to_owned(),
        active_agent: AgentId::from("fake-a"),
        created_at: 1,
        updated_at: 2,
        turn_count: 1,
        agents_used: Vec::new(),
        pinned: false,
        group_id: None,
        order: 0,
        unread: false,
        forked_from: None,
        imported_from: None,
        parent_id: None,
        worktree: None,
    }
}

fn unused<T>(method: &'static str) -> BoxFuture<'static, ApiResult<T>> {
    Box::pin(async move { Err(ApiError::Internal(format!("{method} not supported"))) })
}

impl HarnessCore for StubCore {
    fn list_agents(&self) -> BoxFuture<'_, ApiResult<Vec<harness_core_api::AgentInfo>>> {
        Box::pin(async { Ok(Vec::new()) })
    }

    fn get_fallback_order(&self) -> BoxFuture<'_, ApiResult<Vec<AgentId>>> {
        Box::pin(async { Ok(Vec::new()) })
    }

    fn set_fallback_order(&self, _order: Vec<AgentId>) -> BoxFuture<'_, ApiResult<()>> {
        Box::pin(async { Ok(()) })
    }

    fn create_session(&self, _request: CreateSession) -> BoxFuture<'_, ApiResult<SessionSummary>> {
        unused("create_session")
    }

    fn list_sessions(
        &self,
        _request: ListSessions,
    ) -> BoxFuture<'_, ApiResult<Vec<SessionSummary>>> {
        Box::pin(async { Ok(vec![summary()]) })
    }

    fn open_session(
        &self,
        _session_id: SessionId,
    ) -> BoxFuture<'_, ApiResult<harness_core_api::SessionDetail>> {
        unused("open_session")
    }

    fn send_message(
        &self,
        _request: SendMessage,
    ) -> BoxFuture<'_, ApiResult<harness_core_api::SendAccepted>> {
        unused("send_message")
    }

    fn set_active_agent(
        &self,
        _session_id: SessionId,
        _agent: AgentId,
    ) -> BoxFuture<'_, ApiResult<()>> {
        unused("set_active_agent")
    }

    fn stop(&self, _session_id: SessionId) -> BoxFuture<'_, ApiResult<()>> {
        Box::pin(async { Ok(()) })
    }

    fn list_modes(
        &self,
        session_id: SessionId,
    ) -> BoxFuture<'_, ApiResult<harness_core_api::SessionModes>> {
        Box::pin(async move {
            Ok(harness_core_api::SessionModes {
                agent: AgentId::from("stub"),
                current: Some(session_id.to_string()),
                available: Vec::new(),
                known: true,
            })
        })
    }

    fn cancel_queued(
        &self,
        _session_id: SessionId,
        _turn_id: TurnId,
    ) -> BoxFuture<'_, ApiResult<()>> {
        Box::pin(async { Ok(()) })
    }

    fn respond_permission(
        &self,
        _session_id: SessionId,
        _request_id: PermissionRequestId,
        _option_id: String,
    ) -> BoxFuture<'_, ApiResult<()>> {
        Box::pin(async { Ok(()) })
    }

    fn memory_read(&self, _scope: MemoryScope) -> BoxFuture<'_, ApiResult<String>> {
        Box::pin(async { Ok(String::new()) })
    }

    fn memory_write(
        &self,
        _scope: MemoryScope,
        _content: String,
        _mode: MemoryWriteMode,
    ) -> BoxFuture<'_, ApiResult<()>> {
        Box::pin(async { Ok(()) })
    }

    fn subscribe(&self) -> EventStream {
        EventStream::new(self.tx.subscribe())
    }

    fn shutdown(&self) -> BoxFuture<'_, ApiResult<()>> {
        Box::pin(async { Ok(()) })
    }
}

// ---------------------------------------------------------------- harness

static COUNTER: AtomicU64 = AtomicU64::new(0);

struct Server {
    handle: RemoteHandle,
    addr: SocketAddr,
    core: Arc<StubCore>,
    dir: PathBuf,
}

impl Server {
    async fn start() -> Self {
        let dir = std::env::temp_dir().join(format!(
            "harness-remote-test-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let core = StubCore::new();
        let handle = RemoteHandle::start(
            RemoteConfig {
                port: 0,
                state_dir: dir.clone(),
            },
            core.clone(),
        )
        .await
        .unwrap();
        Self {
            addr: handle.local_addr(),
            handle,
            core,
            dir,
        }
    }

    fn host(&self) -> String {
        format!("127.0.0.1:{}", self.addr.port())
    }

    /// Pairs one device through the real `/api/v1/pair` route and returns `(device_id, cookie)`.
    async fn pair(&self, name: &str) -> (String, String) {
        let offer = self.handle.new_pair_offer(name);
        let reply = call(
            self.addr,
            "POST",
            "/api/v1/pair",
            &self.host(),
            &[],
            &json!({ "token": offer.token, "name": name }).to_string(),
        )
        .await;
        assert_eq!(reply.status, 200, "pair failed: {:?}", reply.text());
        let value: Value = serde_json::from_slice(&reply.body).unwrap();
        let device_id = value["device_id"].as_str().unwrap().to_owned();
        let cookie = reply.cookie.unwrap();
        (device_id, cookie)
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

// ---------------------------------------------------------------- HTTP client

struct Reply {
    status: u16,
    head: String,
    body: Vec<u8>,
    cookie: Option<String>,
}

impl Reply {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or(Value::Null)
    }
}

async fn call(
    addr: SocketAddr,
    method: &str,
    path: &str,
    host: &str,
    extra: &[(&str, String)],
    body: &str,
) -> Reply {
    send(addr, method, path, host, extra, body.len(), body).await
}

/// Like [`call`], but with a declared `Content-Length` the body does not have: the server must
/// refuse the oversized body from the header alone (413) instead of the test writing 25 MB.
async fn call_declared(
    addr: SocketAddr,
    path: &str,
    host: &str,
    extra: &[(&str, String)],
    declared: usize,
) -> Reply {
    send(addr, "POST", path, host, extra, declared, "").await
}

async fn send(
    addr: SocketAddr,
    method: &str,
    path: &str,
    host: &str,
    extra: &[(&str, String)],
    declared: usize,
    body: &str,
) -> Reply {
    let mut stream = TcpStream::connect(addr).await.unwrap();
    let mut request = format!(
        "{method} {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\
         Content-Type: application/json\r\nContent-Length: {declared}\r\n"
    );
    for (name, value) in extra {
        request.push_str(name);
        request.push_str(": ");
        request.push_str(value);
        request.push_str("\r\n");
    }
    request.push_str("\r\n");
    request.push_str(body);
    let _ = stream.write_all(request.as_bytes()).await;
    let raw = read_to_end(&mut stream).await;
    parse(&raw)
}

pub(crate) async fn read_to_end(stream: &mut TcpStream) -> Vec<u8> {
    let mut raw = Vec::new();
    let _ = tokio::time::timeout(TIMEOUT, stream.read_to_end(&mut raw)).await;
    raw
}

fn parse(raw: &[u8]) -> Reply {
    let split = raw
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .unwrap_or(raw.len());
    let head = String::from_utf8_lossy(&raw[..split]).into_owned();
    let body = if split + 4 <= raw.len() {
        raw[split + 4..].to_vec()
    } else {
        Vec::new()
    };
    let mut lines = head.lines();
    let head = head.clone();
    let status = lines
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse().ok())
        .unwrap_or(0);
    let cookie = lines
        .filter_map(|line| line.split_once(':'))
        .find(|(name, _)| name.eq_ignore_ascii_case("set-cookie"))
        .and_then(|(_, value)| value.trim().split(';').next())
        .map(str::to_owned);
    Reply {
        status,
        head,
        body,
        cookie,
    }
}

fn cookie_header(cookie: &str) -> Vec<(&'static str, String)> {
    vec![("Cookie", cookie.to_owned())]
}

// ---------------------------------------------------------------- WS client

/// Opens `/api/v1/events`, returns the raw stream once the 101 response is read.
async fn ws_connect(addr: SocketAddr, host: &str, cookie: &str) -> TcpStream {
    let mut stream = TcpStream::connect(addr).await.unwrap();
    let request = format!(
        "GET /api/v1/events HTTP/1.1\r\nHost: {host}\r\nConnection: Upgrade\r\n\
         Upgrade: websocket\r\nSec-WebSocket-Version: 13\r\n\
         Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nCookie: {cookie}\r\n\r\n"
    );
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut head = Vec::new();
    let mut byte = [0_u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        let read = tokio::time::timeout(TIMEOUT, stream.read_exact(&mut byte))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(read, 1);
        head.push(byte[0]);
    }
    let head = String::from_utf8_lossy(&head).into_owned();
    assert!(head.starts_with("HTTP/1.1 101"), "handshake failed: {head}");
    assert!(
        head.to_ascii_lowercase()
            .contains("sec-websocket-accept: s3pplmbitxaq9kygzzhzrbk+xoo="),
        "missing accept header: {head}"
    );
    stream
}

/// Reads one server→client frame: `(opcode, payload)`.
async fn ws_frame(stream: &mut TcpStream) -> (u8, Vec<u8>) {
    let mut head = [0_u8; 2];
    tokio::time::timeout(TIMEOUT, stream.read_exact(&mut head))
        .await
        .unwrap()
        .unwrap();
    let opcode = head[0] & 0x0f;
    let length = match head[1] & 0x7f {
        126 => {
            let mut raw = [0_u8; 2];
            stream.read_exact(&mut raw).await.unwrap();
            usize::from(u16::from_be_bytes(raw))
        }
        127 => {
            let mut raw = [0_u8; 8];
            stream.read_exact(&mut raw).await.unwrap();
            usize::try_from(u64::from_be_bytes(raw)).unwrap()
        }
        short => usize::from(short),
    };
    let mut payload = vec![0_u8; length];
    tokio::time::timeout(TIMEOUT, stream.read_exact(&mut payload))
        .await
        .unwrap()
        .unwrap();
    (opcode, payload)
}

async fn ws_json(stream: &mut TcpStream) -> Value {
    let (opcode, payload) = ws_frame(stream).await;
    assert_eq!(opcode, 0x1, "expected a text frame");
    serde_json::from_slice(&payload).unwrap()
}

// ---------------------------------------------------------------- tests

#[tokio::test]
async fn status_requires_a_device_token() {
    let server = Server::start().await;
    let no_token = call(
        server.addr,
        "GET",
        "/api/v1/status",
        &server.host(),
        &[],
        "",
    )
    .await;
    assert_eq!(no_token.status, 401);
    assert!(no_token.body.is_empty(), "401 must have an empty body");

    let empty = call(
        server.addr,
        "GET",
        "/api/v1/status",
        &server.host(),
        &cookie_header("harness_remote="),
        "",
    )
    .await;
    assert_eq!(empty.status, 401);

    let wrong = call(
        server.addr,
        "GET",
        "/api/v1/status",
        &server.host(),
        &cookie_header("harness_remote=deadbeef"),
        "",
    )
    .await;
    assert_eq!(wrong.status, 401);

    let (device_id, cookie) = server.pair("Pixel").await;
    let ok = call(
        server.addr,
        "GET",
        "/api/v1/status",
        &server.host(),
        &cookie_header(&cookie),
        "",
    )
    .await;
    assert_eq!(ok.status, 200);
    let value = ok.json();
    assert_eq!(value["device_id"], json!(device_id));
    assert_eq!(value["api_version"], json!(1));
    assert_eq!(value["hub_ready"], json!(true));
    // The bearer header works for non-cookie clients.
    let bearer = call(
        server.addr,
        "GET",
        "/api/v1/status",
        &server.host(),
        &[(
            "Authorization",
            format!("Bearer {}", cookie.split_once('=').unwrap().1),
        )],
        "",
    )
    .await;
    assert_eq!(bearer.status, 200);
}

#[tokio::test]
async fn pair_by_token_and_by_code_is_single_use() {
    let server = Server::start().await;

    let offer = server.handle.new_pair_offer("Pixel");
    assert_eq!(offer.token.len(), 64);
    assert_eq!(offer.code.len(), 8);
    let first = call(
        server.addr,
        "POST",
        "/api/v1/pair",
        &server.host(),
        &[],
        &json!({ "token": offer.token }).to_string(),
    )
    .await;
    assert_eq!(first.status, 200);
    assert!(first.cookie.is_some());
    let reused = call(
        server.addr,
        "POST",
        "/api/v1/pair",
        &server.host(),
        &[],
        &json!({ "token": offer.token }).to_string(),
    )
    .await;
    assert_eq!(reused.status, 401, "pair tokens are single use");

    // Short codes are typed in lower case and must still redeem.
    let code_offer = server.handle.new_pair_offer("Tablet");
    let by_code = call(
        server.addr,
        "POST",
        "/api/v1/pair",
        &server.host(),
        &[],
        &json!({ "code": code_offer.code.to_lowercase(), "name": "Tablet" }).to_string(),
    )
    .await;
    assert_eq!(by_code.status, 200);
    assert_eq!(server.handle.devices().len(), 2);
    let again = call(
        server.addr,
        "POST",
        "/api/v1/pair",
        &server.host(),
        &[],
        &json!({ "code": code_offer.code }).to_string(),
    )
    .await;
    assert_eq!(again.status, 401);
}

#[tokio::test]
async fn expired_pair_offer_is_rejected() {
    let server = Server::start().await;
    let offer = server.handle.new_pair_offer("Old phone");
    server.handle.inner.auth.expire(&offer.token);
    let reply = call(
        server.addr,
        "POST",
        "/api/v1/pair",
        &server.host(),
        &[],
        &json!({ "token": offer.token }).to_string(),
    )
    .await;
    assert_eq!(reply.status, 401);
    assert_eq!(server.handle.devices().len(), 0);
}

#[tokio::test]
async fn eleven_bad_attempts_get_429() {
    let server = Server::start().await;
    for attempt in 1..=10 {
        let reply = call(
            server.addr,
            "GET",
            "/api/v1/status",
            &server.host(),
            &cookie_header("harness_remote=wrong"),
            "",
        )
        .await;
        assert_eq!(reply.status, 401, "attempt {attempt}");
    }
    let limited = call(
        server.addr,
        "GET",
        "/api/v1/status",
        &server.host(),
        &cookie_header("harness_remote=wrong"),
        "",
    )
    .await;
    assert_eq!(limited.status, 429);
    // Pairing from the same IP is limited too (counted before the credential check).
    let pair = call(
        server.addr,
        "POST",
        "/api/v1/pair",
        &server.host(),
        &[],
        &json!({ "token": "x" }).to_string(),
    )
    .await;
    assert_eq!(pair.status, 429);
}

#[tokio::test]
async fn evil_host_and_origin_are_rejected() {
    let server = Server::start().await;
    let (_device_id, cookie) = server.pair("Pixel").await;
    let evil = call(
        server.addr,
        "GET",
        "/api/v1/status",
        "evil.example.com",
        &cookie_header(&cookie),
        "",
    )
    .await;
    assert_eq!(evil.status, 403);
    let evil_static = call(server.addr, "GET", "/", "evil.example.com", &[], "").await;
    assert_eq!(evil_static.status, 403);

    let bad_origin = call(
        server.addr,
        "POST",
        "/api/v1/rpc",
        &server.host(),
        &[
            ("Origin", "https://evil.example.com".to_owned()),
            cookie_header(&cookie)[0].clone(),
        ],
        &json!({ "method": "list_sessions", "params": {} }).to_string(),
    )
    .await;
    assert_eq!(bad_origin.status, 403);

    // The configured public host is accepted (tunnel case).
    server
        .handle
        .set_public_host(Some("harness.example.com".to_owned()));
    let tunnel = call(
        server.addr,
        "GET",
        "/api/v1/status",
        "harness.example.com",
        &cookie_header(&cookie),
        "",
    )
    .await;
    assert_eq!(tunnel.status, 200);
    let tunnel_origin = call(
        server.addr,
        "POST",
        "/api/v1/rpc",
        "harness.example.com",
        &[
            ("Origin", "https://harness.example.com".to_owned()),
            cookie_header(&cookie)[0].clone(),
        ],
        &json!({ "method": "list_sessions", "params": {} }).to_string(),
    )
    .await;
    assert_eq!(tunnel_origin.status, 200);
}

#[tokio::test]
async fn allowlist_rejects_shutdown_and_roundtrips_list_sessions() {
    let server = Server::start().await;
    let (_device_id, cookie) = server.pair("Pixel").await;
    let blocked = call(
        server.addr,
        "POST",
        "/api/v1/rpc",
        &server.host(),
        &cookie_header(&cookie),
        &json!({ "method": "shutdown" }).to_string(),
    )
    .await;
    assert_eq!(blocked.status, 200);
    let value = blocked.json();
    assert_eq!(value["ok"], json!(false));
    assert!(value["error"]
        .as_str()
        .unwrap()
        .contains("method not allowed"));

    let listed = call(
        server.addr,
        "POST",
        "/api/v1/rpc",
        &server.host(),
        &cookie_header(&cookie),
        &json!({ "method": "list_sessions", "params": {} }).to_string(),
    )
    .await;
    assert_eq!(listed.status, 200);
    let value = listed.json();
    assert_eq!(value["ok"], json!(true));
    assert_eq!(value["value"][0]["id"], json!("s1"));
    assert_eq!(value["value"][0]["title"], json!("Test session"));

    let stopped = call(
        server.addr,
        "POST",
        "/api/v1/rpc",
        &server.host(),
        &cookie_header(&cookie),
        &json!({ "method": "stop", "params": { "sessionId": "s1" } }).to_string(),
    )
    .await;
    assert_eq!(stopped.json()["ok"], json!(true));
}

#[tokio::test]
async fn rpc_body_over_sixteen_megabytes_is_413() {
    let server = Server::start().await;
    let (_device_id, cookie) = server.pair("Pixel").await;
    let reply = call_declared(
        server.addr,
        "/api/v1/rpc",
        &server.host(),
        &cookie_header(&cookie),
        16 * 1024 * 1024 + 16,
    )
    .await;
    assert_eq!(reply.status, 413);
}

#[tokio::test]
async fn rpc_accepts_a_three_megabyte_body() {
    let server = Server::start().await;
    let (_device_id, cookie) = server.pair("Pixel").await;
    let image = "A".repeat(3 * 1024 * 1024);
    let body = json!({
        "method": "send_message",
        "params": {
            "sessionId": "s1",
            "text": "look at this",
            "context": [{ "type": "image", "path": null, "dataBase64": image, "mime": "image/jpeg" }],
        },
    })
    .to_string();
    assert!(body.len() > 3 * 1024 * 1024);
    let reply = call(
        server.addr,
        "POST",
        "/api/v1/rpc",
        &server.host(),
        &cookie_header(&cookie),
        &body,
    )
    .await;
    assert_eq!(reply.status, 200, "3 MB rpc body must be accepted");
    // The stub core rejects `send_message`; reaching it proves the body was parsed.
    assert!(reply.json()["error"]
        .as_str()
        .is_some_and(|error| error.contains("send_message")));
}

#[tokio::test]
async fn upload_saves_a_sanitized_file_inside_remote_uploads() {
    let server = Server::start().await;
    let (_device_id, cookie) = server.pair("Pixel").await;
    let reply = call(
        server.addr,
        "POST",
        "/api/v1/upload",
        &server.host(),
        &[
            ("X-File-Name", "../../evil.txt".to_owned()),
            cookie_header(&cookie)[0].clone(),
        ],
        "mobile attachment",
    )
    .await;
    assert_eq!(reply.status, 200, "{:?}", reply.text());
    let value = reply.json();
    assert_eq!(value["ok"], json!(true), "{value}");
    let path = PathBuf::from(value["path"].as_str().unwrap());
    assert!(path.is_absolute(), "path must be absolute: {path:?}");
    let name = path.file_name().unwrap().to_string_lossy().into_owned();
    assert!(
        name.ends_with("-evil.txt") && !name.contains(".."),
        "sanitized name expected, got {name}"
    );
    let parent = path.parent().unwrap();
    assert_eq!(
        parent.file_name().unwrap().to_string_lossy().len(),
        8,
        "uploads are grouped by yyyymmdd"
    );
    assert!(parent.parent().unwrap().ends_with("remote-uploads"));
    assert!(path.starts_with(&server.dir), "must stay inside state_dir");
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "mobile attachment");
    assert!(!server.dir.join("evil.txt").exists());
    assert!(!server.dir.join("..").join("evil.txt").exists());
}

#[tokio::test]
async fn upload_requires_auth() {
    let server = Server::start().await;
    let reply = call(
        server.addr,
        "POST",
        "/api/v1/upload",
        &server.host(),
        &[("X-File-Name", "note.txt".to_owned())],
        "secret",
    )
    .await;
    assert_eq!(reply.status, 401);
    assert_eq!(reply.body.len(), 0);
}

#[tokio::test]
async fn upload_over_twenty_five_megabytes_is_413() {
    let server = Server::start().await;
    let (_device_id, cookie) = server.pair("Pixel").await;
    let reply = call_declared(
        server.addr,
        "/api/v1/upload",
        &server.host(),
        &[
            ("X-File-Name", "big.bin".to_owned()),
            cookie_header(&cookie)[0].clone(),
        ],
        25 * 1024 * 1024 + 16,
    )
    .await;
    assert_eq!(reply.status, 413);
}

#[tokio::test]
async fn websocket_sends_hello_then_event_frames() {
    let server = Server::start().await;
    let (_device_id, cookie) = server.pair("Pixel").await;
    let mut stream = ws_connect(server.addr, &server.host(), &cookie).await;
    let hello = ws_json(&mut stream).await;
    assert_eq!(hello["type"], json!("hello"));
    assert_eq!(hello["seq"], json!(0));

    server.core.publish(EventKind::SessionsChanged);
    let event = ws_json(&mut stream).await;
    assert_eq!(event["type"], json!("event"));
    assert_eq!(event["event"]["seq"], json!(1));
    assert_eq!(event["event"]["kind"]["type"], json!("sessionsChanged"));
}

#[tokio::test]
async fn revoke_drops_access_and_closes_the_socket() {
    let server = Server::start().await;
    let (device_id, cookie) = server.pair("Pixel").await;
    let mut stream = ws_connect(server.addr, &server.host(), &cookie).await;
    assert_eq!(ws_json(&mut stream).await["type"], json!("hello"));

    let revoked = call(
        server.addr,
        "POST",
        "/api/v1/devices/revoke",
        &server.host(),
        &cookie_header(&cookie),
        &json!({ "id": device_id }).to_string(),
    )
    .await;
    assert_eq!(revoked.json()["ok"], json!(true));
    assert_eq!(server.handle.devices().len(), 0);

    let (opcode, payload) = ws_frame(&mut stream).await;
    assert_eq!(opcode, 0x8, "revoked device gets a close frame");
    assert_eq!(payload, vec![0x03, 0xF0]);

    let after = call(
        server.addr,
        "GET",
        "/api/v1/status",
        &server.host(),
        &cookie_header(&cookie),
        "",
    )
    .await;
    assert_eq!(after.status, 401);
}

#[tokio::test]
async fn static_shell_and_healthz_need_no_auth() {
    let server = Server::start().await;
    let index = call(server.addr, "GET", "/", &server.host(), &[], "").await;
    assert_eq!(index.status, 200);
    let shell = call(server.addr, "GET", "/p", &server.host(), &[], "").await;
    assert_eq!(shell.status, 200);
    for name in [
        "app.js",
        "style.css",
        "manifest.webmanifest",
        "sw.js",
        "icon.svg",
    ] {
        let reply = call(
            server.addr,
            "GET",
            &format!("/assets/{name}"),
            &server.host(),
            &[],
            "",
        )
        .await;
        assert_eq!(reply.status, 200, "asset {name}");
        assert_ne!(reply.body.len(), 0);
    }
    let traversal = call(
        server.addr,
        "GET",
        "/assets/../Cargo.toml",
        &server.host(),
        &[],
        "",
    )
    .await;
    assert_eq!(traversal.status, 404);
    let health = call(server.addr, "GET", "/healthz", &server.host(), &[], "").await;
    assert_eq!(health.status, 200);
    assert_eq!(health.json()["ok"], json!(true));
}

#[tokio::test]
async fn shutdown_stops_the_listener() {
    let server = Server::start().await;
    let addr = server.addr;
    server.handle.shutdown().await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(TcpStream::connect(addr).await.is_err());
}

#[tokio::test]
async fn session_methods_accept_object_and_bare_string_params() {
    let server = Server::start().await;
    let (_device_id, cookie) = server.pair("Pixel").await;
    let rpc = |body: Value| {
        let host = server.host();
        let headers = cookie_header(&cookie);
        let addr = server.addr;
        async move {
            call(
                addr,
                "POST",
                "/api/v1/rpc",
                &host,
                &headers,
                &body.to_string(),
            )
            .await
            .json()
        }
    };

    let object = rpc(json!({ "method": "list_modes", "params": { "sessionId": "s1" } })).await;
    assert_eq!(object["ok"], json!(true), "{object}");
    assert_eq!(object["value"]["current"], json!("s1"));

    let bare = rpc(json!({ "method": "list_modes", "params": "s2" })).await;
    assert_eq!(bare["ok"], json!(true), "{bare}");
    assert_eq!(bare["value"]["current"], json!("s2"));

    let stop_bare = rpc(json!({ "method": "stop", "params": "s1" })).await;
    assert_eq!(stop_bare["ok"], json!(true), "{stop_bare}");

    let empty = rpc(json!({ "method": "list_modes", "params": "" })).await;
    assert_eq!(empty["ok"], json!(false));

    let no_params = rpc(json!({ "method": "list_sessions" })).await;
    assert_eq!(no_params["ok"], json!(true), "{no_params}");
    let agents = rpc(json!({ "method": "list_agents", "params": {} })).await;
    assert_eq!(agents["ok"], json!(true), "{agents}");
}

#[tokio::test]
async fn service_worker_is_allowed_to_control_root() {
    let server = Server::start().await;
    let reply = call(server.addr, "GET", "/assets/sw.js", &server.host(), &[], "").await;
    assert_eq!(reply.status, 200);
    assert!(reply
        .head
        .lines()
        .any(|line| line.eq_ignore_ascii_case("service-worker-allowed: /")));
}

#[tokio::test]
async fn valid_device_token_bypasses_the_lockout() {
    let server = Server::start().await;
    let (_device_id, cookie) = server.pair("Pixel").await;
    for _ in 0..11 {
        call(
            server.addr,
            "GET",
            "/api/v1/status",
            &server.host(),
            &cookie_header("harness_remote=wrong"),
            "",
        )
        .await;
    }
    let bad = call(
        server.addr,
        "GET",
        "/api/v1/status",
        &server.host(),
        &cookie_header("harness_remote=wrong"),
        "",
    )
    .await;
    assert_eq!(bad.status, 429);
    let good = call(
        server.addr,
        "GET",
        "/api/v1/status",
        &server.host(),
        &cookie_header(&cookie),
        "",
    )
    .await;
    assert_eq!(good.status, 200);
}

#[tokio::test]
async fn cf_connecting_ip_keys_the_rate_limit_behind_the_tunnel() {
    let server = Server::start().await;
    let from = |ip: &str| {
        let mut headers = cookie_header("harness_remote=wrong");
        headers.push(("CF-Connecting-IP", ip.to_owned()));
        headers
    };
    for _ in 0..11 {
        call(
            server.addr,
            "GET",
            "/api/v1/status",
            &server.host(),
            &from("203.0.113.7"),
            "",
        )
        .await;
    }
    let attacker = call(
        server.addr,
        "GET",
        "/api/v1/status",
        &server.host(),
        &from("203.0.113.7"),
        "",
    )
    .await;
    assert_eq!(attacker.status, 429);
    let attacker_pair = call(
        server.addr,
        "POST",
        "/api/v1/pair",
        &server.host(),
        &[("CF-Connecting-IP", "203.0.113.7".to_owned())],
        &json!({ "token": "x" }).to_string(),
    )
    .await;
    assert_eq!(attacker_pair.status, 429);

    let other = call(
        server.addr,
        "GET",
        "/api/v1/status",
        &server.host(),
        &from("198.51.100.2"),
        "",
    )
    .await;
    assert_eq!(other.status, 401);
    let (_device_id, _cookie) = server.pair("Pixel").await;
}

#[test]
fn cf_connecting_ip_is_trusted_only_from_loopback() {
    use crate::http::client_ip;
    let mut headers = http::HeaderMap::new();
    headers.insert("cf-connecting-ip", "203.0.113.7".parse().unwrap());
    let loopback: SocketAddr = "127.0.0.1:5000".parse().unwrap();
    let remote: SocketAddr = "192.0.2.9:5000".parse().unwrap();
    assert_eq!(client_ip(loopback, &headers).to_string(), "203.0.113.7");
    assert_eq!(client_ip(remote, &headers).to_string(), "192.0.2.9");
    headers.insert("cf-connecting-ip", "not-an-ip".parse().unwrap());
    assert_eq!(client_ip(loopback, &headers).to_string(), "127.0.0.1");
    assert_eq!(
        client_ip(loopback, &http::HeaderMap::new()).to_string(),
        "127.0.0.1"
    );
}
