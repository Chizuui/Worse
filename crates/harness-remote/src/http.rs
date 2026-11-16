//! Routing, static assets, `Host`/`Origin` checks, the body caps and the phone upload route.

use std::convert::Infallible;
use std::fmt::Write as _;
use std::net::SocketAddr;
use std::sync::Arc;

use bytes::Bytes;
use http::header::{self, HeaderMap, HeaderValue};
use http::{Method, Request, Response, StatusCode};
use http_body_util::{BodyExt, Full, Limited};
use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use serde_json::{json, Value};
use tokio::net::{TcpListener, TcpStream};

use crate::{rpc, ws, Inner};

type Body = Full<Bytes>;

/// Largest accepted JSON body (1 MB) for every route but `/api/v1/rpc`.
const MAX_BODY: usize = 1024 * 1024;
/// `/api/v1/rpc` cap: base64 images travel inside `send_message` (`chat::limits`).
const MAX_RPC_BODY: usize = 16 * 1024 * 1024;
/// `/api/v1/upload` cap: raw attachment bytes.
const MAX_UPLOAD_BODY: usize = 25 * 1024 * 1024;
const JSON: &str = "application/json";
const COOKIE: &str = "harness_remote";

/// One embedded file served without auth. A fixed table: no path traversal is possible.
struct Asset {
    name: &'static str,
    content_type: &'static str,
    body: &'static [u8],
}

/// The phone client (stream C owns `src/assets/**`; this crate only embeds it).
const ASSETS: &[Asset] = &[
    Asset {
        name: "index.html",
        content_type: "text/html; charset=utf-8",
        body: include_bytes!("assets/index.html"),
    },
    Asset {
        name: "app.js",
        content_type: "text/javascript; charset=utf-8",
        body: include_bytes!("assets/app.js"),
    },
    Asset {
        name: "style.css",
        content_type: "text/css; charset=utf-8",
        body: include_bytes!("assets/style.css"),
    },
    Asset {
        name: "pair.css",
        content_type: "text/css; charset=utf-8",
        body: include_bytes!("assets/pair.css"),
    },
    Asset {
        name: "sessions.css",
        content_type: "text/css; charset=utf-8",
        body: include_bytes!("assets/sessions.css"),
    },
    Asset {
        name: "chat.css",
        content_type: "text/css; charset=utf-8",
        body: include_bytes!("assets/chat.css"),
    },
    Asset {
        name: "settings.css",
        content_type: "text/css; charset=utf-8",
        body: include_bytes!("assets/settings.css"),
    },
    Asset {
        name: "sessions.js",
        content_type: "text/javascript; charset=utf-8",
        body: include_bytes!("assets/sessions.js"),
    },
    Asset {
        name: "settings.js",
        content_type: "text/javascript; charset=utf-8",
        body: include_bytes!("assets/settings.js"),
    },
    Asset {
        name: "manifest.webmanifest",
        content_type: "application/manifest+json",
        body: include_bytes!("assets/manifest.webmanifest"),
    },
    Asset {
        name: "sw.js",
        content_type: "text/javascript; charset=utf-8",
        body: include_bytes!("assets/sw.js"),
    },
    Asset {
        name: "icon.svg",
        content_type: "image/svg+xml",
        body: include_bytes!("assets/icon.svg"),
    },
];

fn asset(name: &str) -> Option<&'static Asset> {
    ASSETS.iter().find(|asset| asset.name == name)
}

/// Accept loop; runs until the handle's `closing` watch flips.
pub(crate) async fn accept_loop(inner: Arc<Inner>, listener: TcpListener) {
    let mut closing = inner.closing_rx();
    loop {
        tokio::select! {
            _ = closing.changed() => break,
            accepted = listener.accept() => {
                let Ok((stream, peer)) = accepted else { continue };
                let _ = stream.set_nodelay(true);
                let conn = Arc::clone(&inner);
                tokio::spawn(async move { serve(conn, stream, peer).await });
            }
        }
    }
}

async fn serve(inner: Arc<Inner>, stream: TcpStream, peer: SocketAddr) {
    let mut closing = inner.closing_rx();
    let service = service_fn(move |req: Request<Incoming>| {
        let inner = Arc::clone(&inner);
        async move { Ok::<_, Infallible>(route(&inner, peer, req).await) }
    });
    let conn = hyper::server::conn::http1::Builder::new()
        .serve_connection(TokioIo::new(stream), service)
        .with_upgrades();
    tokio::pin!(conn);
    tokio::select! {
        _ = conn.as_mut() => {}
        _ = closing.changed() => {
            conn.as_mut().graceful_shutdown();
            let _ = conn.await;
        }
    }
}

async fn route(inner: &Arc<Inner>, peer: SocketAddr, req: Request<Incoming>) -> Response<Body> {
    // Host first: DNS rebinding is refused before anything else happens.
    let Some(host) = header_str(req.headers(), header::HOST) else {
        return empty(StatusCode::FORBIDDEN);
    };
    if !inner.host_allowed(host) {
        return empty(StatusCode::FORBIDDEN);
    }
    let method = req.method().clone();
    let path = req.uri().path().to_owned();
    match (method, path.as_str()) {
        (Method::GET, "/" | "/p") => static_file("index.html"),
        (Method::GET, "/healthz") => json_response(StatusCode::OK, &json!({ "ok": true })),
        (Method::GET, "/api/v1/status") => status(inner, peer, &req),
        (Method::GET, "/api/v1/events") => events(inner, peer, req),
        (Method::POST, "/api/v1/pair") => pair(inner, peer, req).await,
        (Method::POST, "/api/v1/rpc") => rpc_call(inner, peer, req).await,
        (Method::POST, "/api/v1/upload") => upload(inner, peer, req).await,
        (Method::POST, "/api/v1/devices/revoke") => revoke(inner, peer, req).await,
        (Method::GET, other) => static_route(other),
        _ => empty(StatusCode::NOT_FOUND),
    }
}

fn static_route(path: &str) -> Response<Body> {
    let name = path
        .strip_prefix("/assets/")
        .unwrap_or_else(|| path.strip_prefix('/').unwrap_or(path));
    if name.is_empty() || name.contains('/') || name.contains("..") {
        return empty(StatusCode::NOT_FOUND);
    }
    static_file(name)
}

fn static_file(name: &str) -> Response<Body> {
    let Some(asset) = asset(name) else {
        return empty(StatusCode::NOT_FOUND);
    };
    let mut res = Response::new(Body::from(Bytes::from_static(asset.body)));
    res.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static(asset.content_type),
    );
    res.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    if asset.name == "sw.js" {
        res.headers_mut().insert(
            header::HeaderName::from_static("service-worker-allowed"),
            HeaderValue::from_static("/"),
        );
    }
    res
}

async fn pair(inner: &Arc<Inner>, peer: SocketAddr, req: Request<Incoming>) -> Response<Body> {
    let ip = client_ip(peer, req.headers());
    if !inner.auth.allow(ip) {
        return empty(StatusCode::TOO_MANY_REQUESTS);
    }
    if !origin_ok(inner, req.headers()) {
        return empty(StatusCode::FORBIDDEN);
    }
    let Some(body) = read_body(req, MAX_BODY).await else {
        return empty(StatusCode::PAYLOAD_TOO_LARGE);
    };
    let Ok(params) = serde_json::from_slice::<Value>(&body) else {
        inner.auth.fail(ip);
        return json_response(
            StatusCode::UNAUTHORIZED,
            &json!({ "ok": false, "error": "invalid body" }),
        );
    };
    let token = params.get("token").and_then(Value::as_str);
    let code = params.get("code").and_then(Value::as_str);
    let name = params.get("name").and_then(Value::as_str);
    if token.is_none() && code.is_none() {
        inner.auth.fail(ip);
        return json_response(
            StatusCode::UNAUTHORIZED,
            &json!({ "ok": false, "error": "token or code required" }),
        );
    }
    let Some((device_id, device_token)) = inner.auth.pair(token, code, name) else {
        inner.auth.fail(ip);
        return json_response(
            StatusCode::UNAUTHORIZED,
            &json!({ "ok": false, "error": "unknown or expired pair offer" }),
        );
    };
    let mut res = json_response(
        StatusCode::OK,
        &json!({ "ok": true, "device_id": device_id }),
    );
    let cookie = format!("{COOKIE}={device_token}; HttpOnly; Secure; SameSite=Strict; Path=/");
    if let Ok(value) = HeaderValue::from_str(&cookie) {
        res.headers_mut().append(header::SET_COOKIE, value);
    }
    res
}

fn status(inner: &Arc<Inner>, peer: SocketAddr, req: &Request<Incoming>) -> Response<Body> {
    let device = match authenticate(inner, peer, req.headers()) {
        Ok(device) => device,
        Err(status) => return empty(status),
    };
    json_response(
        StatusCode::OK,
        &json!({
            "version": env!("CARGO_PKG_VERSION"),
            "api_version": 1,
            "device_id": device,
            "hub_ready": true,
        }),
    )
}

async fn rpc_call(inner: &Arc<Inner>, peer: SocketAddr, req: Request<Incoming>) -> Response<Body> {
    if let Err(status) = authenticate(inner, peer, req.headers()) {
        return empty(status);
    }
    if !origin_ok(inner, req.headers()) {
        return empty(StatusCode::FORBIDDEN);
    }
    let Some(body) = read_body(req, MAX_RPC_BODY).await else {
        return empty(StatusCode::PAYLOAD_TOO_LARGE);
    };
    let Ok(params) = serde_json::from_slice::<Value>(&body) else {
        return json_response(StatusCode::OK, &rpc::bad_request("invalid json"));
    };
    let method = params
        .get("method")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let args = params.get("params").cloned().unwrap_or(Value::Null);
    if !rpc::is_allowed(&method) {
        return json_response(StatusCode::OK, &rpc::not_allowed(&method));
    }
    let view = inner.settings.read().ok().and_then(|slot| slot.clone());
    if let Some(value) = rpc::settings_call(view.as_deref(), &method, &args) {
        return json_response(StatusCode::OK, &json!({ "ok": true, "value": value }));
    }
    let response = match rpc::call(inner.core.as_ref(), &method, args).await {
        Ok(value) => json!({ "ok": true, "value": value }),
        Err(error) => json!({ "ok": false, "error": error }),
    };
    json_response(StatusCode::OK, &response)
}

async fn revoke(inner: &Arc<Inner>, peer: SocketAddr, req: Request<Incoming>) -> Response<Body> {
    if let Err(status) = authenticate(inner, peer, req.headers()) {
        return empty(status);
    }
    if !origin_ok(inner, req.headers()) {
        return empty(StatusCode::FORBIDDEN);
    }
    let Some(body) = read_body(req, MAX_BODY).await else {
        return empty(StatusCode::PAYLOAD_TOO_LARGE);
    };
    let Ok(params) = serde_json::from_slice::<Value>(&body) else {
        return json_response(
            StatusCode::OK,
            &json!({ "ok": false, "error": "invalid json" }),
        );
    };
    let id = params
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    if id.is_empty() {
        return json_response(
            StatusCode::OK,
            &json!({ "ok": false, "error": "id required" }),
        );
    }
    inner.auth.revoke(&id);
    json_response(StatusCode::OK, &json!({ "ok": true }))
}

/// `POST /api/v1/upload`: raw body saved under `state_dir/remote-uploads/<yyyymmdd>/`, returning the
/// absolute path so the phone can attach it as `ContextItem::File`.
async fn upload(inner: &Arc<Inner>, peer: SocketAddr, req: Request<Incoming>) -> Response<Body> {
    if let Err(status) = authenticate(inner, peer, req.headers()) {
        return empty(status);
    }
    if !origin_ok(inner, req.headers()) {
        return empty(StatusCode::FORBIDDEN);
    }
    let requested = header_str(req.headers(), "x-file-name").unwrap_or("file");
    let name = sanitize_file_name(requested);
    let Some(body) = read_body(req, MAX_UPLOAD_BODY).await else {
        return empty(StatusCode::PAYLOAD_TOO_LARGE);
    };
    if body.is_empty() {
        return json_response(
            StatusCode::OK,
            &json!({ "ok": false, "error": "empty file" }),
        );
    }
    let dir = inner.state_dir.join("remote-uploads").join(utc_day());
    if let Err(error) = tokio::fs::create_dir_all(&dir).await {
        return json_response(
            StatusCode::OK,
            &json!({ "ok": false, "error": format!("cannot create upload dir: {error}") }),
        );
    }
    let path = dir.join(format!("{}-{name}", random_hex(8)));
    if let Err(error) = tokio::fs::write(&path, &body).await {
        return json_response(
            StatusCode::OK,
            &json!({ "ok": false, "error": format!("cannot write upload: {error}") }),
        );
    }
    let absolute = std::path::absolute(&path).unwrap_or(path);
    json_response(
        StatusCode::OK,
        &json!({ "ok": true, "path": absolute.to_string_lossy() }),
    )
}

/// Basename only, no `..`, no separators, no control chars, at most 100 chars.
fn sanitize_file_name(raw: &str) -> String {
    let base = raw.rsplit(['/', '\\']).next().unwrap_or_default();
    let mut name: String = base
        .chars()
        .filter(|ch| !ch.is_control() && *ch != '{' && *ch != '}')
        .collect();
    while name.contains("..") {
        name = name.replace("..", ".");
    }
    let trimmed = name.trim_matches(|ch: char| ch == '.' || ch.is_whitespace());
    let clipped: String = trimmed.chars().take(100).collect();
    if clipped.is_empty() {
        "file".to_owned()
    } else {
        clipped
    }
}

/// UTC `yyyymmdd` without pulling in a date crate.
fn utc_day() -> String {
    let secs = crate::auth::now_ms() / 1000;
    let (year, month, day) = civil_from_days(i64::try_from(secs / 86_400).unwrap_or(0));
    format!("{year:04}{month:02}{day:02}")
}

/// Howard Hinnant's `civil_from_days`: days since 1970-01-01 → (year, month, day).
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// `n` random bytes as hex (`getrandom`, process/time fallback like [`crate::auth`]).
fn random_hex(n: usize) -> String {
    let mut bytes = vec![0_u8; n];
    if getrandom::fill(&mut bytes).is_err() {
        let mut state = crate::auth::now_ms()
            .rotate_left(17)
            .wrapping_add(u64::from(std::process::id()))
            | 1;
        for byte in &mut bytes {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            *byte = (state & 0xff) as u8;
        }
    }
    let mut out = String::with_capacity(n * 2);
    for byte in bytes {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

fn events(inner: &Arc<Inner>, peer: SocketAddr, req: Request<Incoming>) -> Response<Body> {
    let device = match authenticate(inner, peer, req.headers()) {
        Ok(device) => device,
        Err(status) => return empty(status),
    };
    if !ws::is_upgrade(req.headers()) {
        return empty(StatusCode::UPGRADE_REQUIRED);
    }
    let Some(key) = header_str(req.headers(), "sec-websocket-key").map(str::to_owned) else {
        return empty(StatusCode::BAD_REQUEST);
    };
    let Some(res) = ws::handshake(&key) else {
        return empty(StatusCode::BAD_REQUEST);
    };
    let stream = inner.core.subscribe();
    let state = Arc::clone(inner);
    let closing = inner.closing_rx();
    tokio::spawn(async move {
        if let Ok(upgraded) = hyper::upgrade::on(req).await {
            ws::serve(upgraded, state, device, stream, closing).await;
        }
    });
    res
}

/// `Origin` (when present on a `POST`) must belong to this server.
fn origin_ok(inner: &Inner, headers: &HeaderMap) -> bool {
    match header_str(headers, header::ORIGIN) {
        None => true,
        Some(origin) => inner.origin_allowed(origin),
    }
}

/// Device auth of a request; the `Err` status is 429 when the IP is rate limited, else 401.
fn authenticate(
    inner: &Arc<Inner>,
    peer: SocketAddr,
    headers: &HeaderMap,
) -> Result<String, StatusCode> {
    if let Some(device) = token_of(headers).and_then(|token| inner.auth.device_for_token(&token)) {
        return Ok(device);
    }
    let ip = client_ip(peer, headers);
    if !inner.auth.allow(ip) {
        return Err(StatusCode::TOO_MANY_REQUESTS);
    }
    inner.auth.fail(ip);
    Err(StatusCode::UNAUTHORIZED)
}

/// Rate-limit key: behind a local `cloudflared` every peer is loopback, so the tunnel's
/// `CF-Connecting-IP` names the real client. Only trusted when the peer itself is loopback.
pub(crate) fn client_ip(peer: SocketAddr, headers: &HeaderMap) -> std::net::IpAddr {
    let ip = peer.ip();
    if ip.is_loopback() {
        if let Some(forwarded) = header_str(headers, "cf-connecting-ip")
            .and_then(|value| value.trim().parse::<std::net::IpAddr>().ok())
        {
            return forwarded;
        }
    }
    ip
}

/// Cookie first (browsers cannot set headers on `new WebSocket`), then `Authorization: Bearer`.
fn token_of(headers: &HeaderMap) -> Option<String> {
    if let Some(cookies) = header_str(headers, header::COOKIE) {
        for part in cookies.split(';') {
            let part = part.trim();
            if let Some(value) = part.strip_prefix(&format!("{COOKIE}=")) {
                let value = value.trim_matches('"');
                if !value.is_empty() {
                    return Some(value.to_owned());
                }
            }
        }
    }
    let auth = header_str(headers, header::AUTHORIZATION)?;
    let (scheme, value) = auth.split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("bearer") || value.trim().is_empty() {
        return None;
    }
    Some(value.trim().to_owned())
}

fn header_str(headers: &HeaderMap, name: impl http::header::AsHeaderName) -> Option<&str> {
    headers.get(name).and_then(|value| value.to_str().ok())
}

/// Reads the body, refusing anything over `limit` (`None` → 413).
async fn read_body(req: Request<Incoming>, limit: usize) -> Option<Bytes> {
    let length = header_str(req.headers(), header::CONTENT_LENGTH)
        .and_then(|value| value.parse::<usize>().ok());
    if length.is_some_and(|length| length > limit) {
        return None;
    }
    let limited = Limited::new(req.into_body(), limit);
    limited
        .collect()
        .await
        .ok()
        .map(http_body_util::Collected::to_bytes)
}

fn empty(code: StatusCode) -> Response<Body> {
    let mut res = Response::new(Body::default());
    *res.status_mut() = code;
    res
}

fn json_response(code: StatusCode, value: &Value) -> Response<Body> {
    let mut res = Response::new(Body::from(Bytes::from(
        serde_json::to_vec(value).unwrap_or_else(|_| b"{}".to_vec()),
    )));
    *res.status_mut() = code;
    res.headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static(JSON));
    res
}
