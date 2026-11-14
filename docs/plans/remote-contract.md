# Remote phone access — shared contract for the 4 parallel workstreams

Source plan: `docs/plans/android-apk-and-tunnel.md`. User decisions: **named tunnel** (stable URL,
`cftunnel http <port> --name <subdomain>`), real APK (Kotlin WebView), v1 scope = sessions + chat +
permissions + subagents, loopback only (no LAN), tunnel started from Settings (no auto-start).

## File ownership (never edit a file owned by another stream)

| Stream | Owns |
| --- | --- |
| A server | `crates/harness-remote/**` except `src/assets/**` |
| B desktop | `crates/harness-settings/src/model.rs`, `apps/harness-slint/**` (incl. new `src/remote/`, `ui/settings/remote.slint`), `apps/harness-slint/Cargo.toml` |
| C web client | `crates/harness-remote/src/assets/**` only |
| D APK | `apps/harness-mobile/**` only |

Root `Cargo.toml` / `Cargo.lock`: only A (adds the crate deps) and B (adds `harness-remote` + `qrcode`
to harness-slint). `crates/*` is already a workspace glob, so A needs no root edit.

## Rust API of `harness-remote` (A implements, B calls)

```rust
pub struct RemoteConfig {
    pub port: u16,              // default 7878, bind 127.0.0.1 only
    pub state_dir: PathBuf,     // <HARNESS_HOME>; devices in state_dir/remote.json
}
pub struct DeviceInfo { pub id: String, pub name: String, pub created_ms: u64, pub last_seen_ms: u64 }
pub struct PairOffer { pub token: String /*64 hex*/, pub code: String /*8 chars A-Z2-9*/, pub expires_ms: u64 }

#[derive(Clone)] pub struct RemoteHandle { /* Arc inside */ }
impl RemoteHandle {
    pub async fn start(cfg: RemoteConfig, core: Arc<dyn harness_core_api::HarnessCore>) -> std::io::Result<RemoteHandle>;
    pub fn local_addr(&self) -> std::net::SocketAddr;
    pub fn set_public_host(&self, host: Option<String>); // e.g. "harness.example.com"; Host/Origin check allows it + 127.0.0.1:<port>
    pub fn new_pair_offer(&self, device_name: &str) -> PairOffer; // single-use, 10 min
    pub fn devices(&self) -> Vec<DeviceInfo>;
    pub fn revoke(&self, id: &str);   // deletes row, closes that device's sockets
    pub async fn shutdown(&self);
}
```

## HTTP contract (A implements, C consumes)

All JSON. Cookie `harness_remote=<device token>; HttpOnly; Secure; SameSite=Strict; Path=/`.
Non-cookie clients may send `Authorization: Bearer <device token>`.

| Route | Auth | Body → Response |
| --- | --- | --- |
| `GET /`, `GET /p`, `GET /assets/<file>` | none | static files embedded from `src/assets/` (`index.html`, `app.js`, `style.css`, `manifest.webmanifest`, `sw.js`, `icon.svg`) |
| `GET /healthz` | none | `{"ok":true}` |
| `POST /api/v1/pair` | none (rate limited) | `{"token":"…"}` or `{"code":"…","name":"Pixel"}` → `{"ok":true,"device_id":"…"}` + Set-Cookie (device token, long-lived) |
| `GET /api/v1/status` | yes | `{"version","api_version":1,"device_id","hub_ready":bool}` |
| `POST /api/v1/rpc` | yes | `{"method":"list_sessions","params":{…}}` → `{"ok":true,"value":…}` / `{"ok":false,"error":"…"}` |
| `GET /api/v1/events` (WebSocket) | yes (cookie or `?bearer=` not allowed — cookie/header only) | server→client text frames: first `{"type":"hello","seq":N}`, then `{"type":"event","event":<HubEvent JSON>}`; on lag `{"type":"resync"}` (client re-opens its session) |
| `POST /api/v1/devices/revoke` | yes | `{"id":"…"}` → `{"ok":true}` |
| `POST /api/v1/upload` | yes | raw body + `X-File-Name` (name only) + `Content-Type` → `{"ok":true,"path":"<abs>"}`; saved to `state_dir/remote-uploads/<yyyymmdd>/<random>-<sanitized name>` (basename only, no `..`/separators/control chars, ≤100 chars) |

RPC allowlist (params = serde JSON of the matching `harness_core_api` request type):
`list_agents, list_sessions, open_session, send_message, stop, cancel_queued, respond_permission, list_modes, list_models,
set_active_agent, set_mode, set_model, rename_session, set_pinned, list_projects, turn_diff`.
Anything else → `{"ok":false,"error":"method not allowed"}`.

Security: 401 empty body without valid auth; constant-time compare; 10 failed pair/auth per IP per
minute → 429; Host must be public host or `127.0.0.1:<port>`/`localhost:<port>`; POST Origin (if
present) must match; body caps: 1 MB for JSON routes, 16 MB for `/api/v1/rpc` (base64 images),
25 MB for `/api/v1/upload`; never log tokens/codes.

## Attachments (C sends, A stores)

`send_message` carries `context` items — `harness_core_api::chat::ContextItem`
(`#[serde(tag = "type", rename_all = "camelCase")]`: `file` / `folder` / `image` / `text` / `session`):

- image picked or pasted on the phone → `{"type":"image","path":null,"dataBase64":"…","mime":"image/jpeg"}`
  (downscaled client-side to ≤2048 px long edge, JPEG q0.85; PNG kept when < 1 MB).
- any other file → `POST /api/v1/upload` first, then `{"type":"file","path":"<absolute path returned>"}`.

Limits come from `chat::limits` (`MAX_ITEMS` 10, `MAX_IMAGE_BYTES` 5 MB, `MAX_TOTAL_BYTES` 20 MB);
`send_message` text must not be empty, so an attachment-only message sends a generated caption.

## Pair link (B shows it, D/C consume it)

`https://<public_host>/p#t=<token>` — token in the fragment. The page JS posts it to `/api/v1/pair`,
then `history.replaceState` drops the fragment. QR encodes exactly this URL. Short code is typed on
`/p` when there is no fragment.

## Settings (B)

`RemoteSettings { enabled: bool=false, port: u16=7878, tunnel_bin: String="cftunnel",
subdomain: String="", tunnel_domain: String="" }` (serde default, field `remote` on `Settings`).
Tunnel: `<tunnel_bin> http <port> --name <subdomain>`; public URL = first `https://…` line in its
stdout/stderr (log raw output), else `https://<subdomain>.<tunnel_domain>` when configured.
